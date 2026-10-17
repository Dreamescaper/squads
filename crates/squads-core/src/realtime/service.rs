//! Realtime service wiring (milestone **M4**).
//!
//! [`RealtimeService::start`] spawns two tasks on a caller-provided
//! [`tokio::runtime::Handle`]:
//!
//! 1. the **connection** task — [`ost::trouter::connect_and_run_with_config`]
//!    with an injected in-memory [`ost::config::Config`] built from our
//!    [`TokenSet`] (via [`crate::api::tokens_to_config`]). The vendored loop
//!    negotiates, opens the socket.io v1 WebSocket, registers with the registrar
//!    and honours the `3:::` ack contract, reconnecting with exponential backoff
//!    (1s…64s) forever. We do **not** fork the socket layer (ADR-013).
//! 2. the **collector** task — loops [`ost::event_hub::drain_wait`], decodes each
//!    payload with [`decode_all`] and, for every event, writes through to the
//!    shared [`Store`] and forwards it to the [`EventBus`].
//!
//! ## Deterministic testing
//!
//! The collector's clock-free core is [`collect_once`]: it drains whatever is
//! queued right now, decodes, applies and returns how many events it handled.
//! Tests publish a fixture into the global [`ost::event_hub`] and call
//! [`collect_once`] directly — no timing, no connection, no network. The task
//! loop is a thin wrapper around the same function.
//!
//! ## Constraints
//!
//! * [`drain_wait`](ost::event_hub::drain_wait) blocks the calling thread on a
//!   condition variable, so the service expects a **multi-threaded** runtime (the
//!   core owns one, ADR-004). The wait is bounded by [`COLLECT_TIMEOUT_MS`].
//! * The vendored loop reuses the same config across reconnects; if tokens
//!   expire mid-connection the reconnect carries stale credentials (B-008).

use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::api::tokens_to_config;
use crate::auth::tokens::TokenSet;
use crate::store::Store;

use super::bus::EventBus;
use super::event::{RealtimeEvent, decode_all};

/// Maximum payloads taken from the global hub per collector pass (the hub itself
/// is bounded to 1024, so this drains it fully).
const COLLECT_BATCH: usize = 1024;

/// How long the collector waits for the first event of a pass before going
/// around again (bounded so `stop()` is responsive).
const COLLECT_TIMEOUT_MS: u64 = 250;

/// Drain everything currently queued in the process-global event hub, decode it,
/// write through to `store` and publish to `bus`.
///
/// Returns the number of [`RealtimeEvent`]s handled. This is the deterministic
/// core of the collector loop and the entry point for tests.
pub fn collect_once(profile: &str, store: &Store, bus: &EventBus) -> usize {
    let payloads = ost::event_hub::drain(COLLECT_BATCH);
    apply_payloads(profile, store, bus, payloads)
}

fn apply_payloads(profile: &str, store: &Store, bus: &EventBus, payloads: Vec<String>) -> usize {
    let mut applied = 0;
    for payload in payloads {
        for event in decode_all(&payload) {
            apply_event(profile, store, bus, event);
            applied += 1;
        }
    }
    applied
}

/// Apply one event's store write-through, then publish it to the bus.
///
/// The store write happens **before** the publish so a consumer that reacts to
/// the event and immediately reads the store sees the new state.
fn apply_event(profile: &str, store: &Store, bus: &EventBus, event: RealtimeEvent) {
    match &event {
        RealtimeEvent::NewMessage { message, .. } => {
            if let Err(e) = store.upsert_messages(profile, std::slice::from_ref(message)) {
                tracing::warn!(error = %e, "realtime: failed to cache a new message");
            }
        }
        RealtimeEvent::MessageEdited {
            chat_id,
            message_id,
            content,
            ..
        } => match store.update_message_edit(profile, chat_id, message_id, content) {
            Ok(false) => {
                tracing::debug!(%chat_id, %message_id, "realtime: edit for an uncached message");
            }
            Err(e) => tracing::warn!(error = %e, "realtime: failed to apply a message edit"),
            Ok(true) => {}
        },
        RealtimeEvent::MessageDeleted {
            chat_id,
            message_id,
        } => {
            if let Err(e) = store.delete_message(profile, chat_id, message_id) {
                tracing::warn!(error = %e, "realtime: failed to delete a message");
            }
        }
        // Typing, read-state, thread updates, resyncs and unknowns are bus-only
        // in M4; M5+ decides what (if anything) to persist.
        _ => {}
    }
    bus.publish(event);
}

/// A running realtime feed: the Trouter connection task plus the collector.
///
/// Dropping the service aborts both tasks; [`RealtimeService::stop`] does the
/// same explicitly.
pub struct RealtimeService {
    bus: EventBus,
    tasks: Vec<JoinHandle<()>>,
}

impl RealtimeService {
    /// Spawn the connection and collector tasks on `handle`.
    ///
    /// `tokens` is mapped into an injected [`ost::config::Config`]; `profile` is
    /// the store namespace the collector writes through to. Requires a
    /// multi-threaded runtime (see the module docs).
    pub fn start(
        handle: &Handle,
        profile: impl Into<String>,
        tokens: &TokenSet,
        store: Arc<Store>,
    ) -> Self {
        let bus = EventBus::default();
        let config = tokens_to_config(tokens);
        let profile = profile.into();

        let connection = handle.spawn(async move {
            tracing::debug!("realtime: trouter connection loop starting");
            if let Err(e) = ost::trouter::connect_and_run_with_config(Some(config)).await {
                tracing::warn!(error = %e, "realtime: trouter connection loop exited");
            }
        });

        let collector_bus = bus.clone();
        let collector = handle.spawn(async move {
            loop {
                let payloads = ost::event_hub::drain_wait(COLLECT_BATCH, COLLECT_TIMEOUT_MS);
                if !payloads.is_empty() {
                    let n = apply_payloads(&profile, &store, &collector_bus, payloads);
                    tracing::trace!(events = n, "realtime: collected push events");
                }
                // Yield so the task is cancellable between (blocking) waits.
                tokio::task::yield_now().await;
            }
        });

        Self {
            bus,
            tasks: vec![connection, collector],
        }
    }

    /// The bus this service publishes on.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// Subscribe to this service's event bus.
    pub fn subscribe(&self) -> broadcast::Receiver<RealtimeEvent> {
        self.bus.subscribe()
    }

    /// Abort both tasks. Idempotent; also called on drop.
    pub fn stop(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }

    /// Whether the tasks have not been stopped yet.
    pub fn is_running(&self) -> bool {
        !self.tasks.is_empty()
    }
}

impl Drop for RealtimeService {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for RealtimeService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealtimeService")
            .field("running", &self.is_running())
            .field("bus", &self.bus)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Message;

    /// The global hub is shared by every test in this binary, so tests that
    /// touch it serialize on this lock and use unique ids.
    static HUB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn hub_guard() -> std::sync::MutexGuard<'static, ()> {
        HUB_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn message_fixture(chat_id: &str, id: &str, body: &str) -> String {
        serde_json::json!({
            "resourceType": "NewMessage",
            "resource": {
                "id": id,
                "conversationId": chat_id,
                "from": "https://teams.microsoft.com/api/v1/users/ME/contacts/8:orgid:aaaa",
                "imdisplayname": "Alice",
                "content": body,
                "messagetype": "RichText/Html",
                "originalarrivaltime": "2026-01-02T03:04:05Z"
            }
        })
        .to_string()
    }

    #[test]
    fn collect_once_writes_a_new_message_through_and_publishes_it() {
        let _guard = hub_guard();
        ost::event_hub::drain(usize::MAX);

        let store = Store::open_in_memory().unwrap();
        let bus = EventBus::default();
        let mut rx = bus.subscribe();

        let chat_id = format!("19:m4-{}@unq.gbl.spaces", uuid::Uuid::new_v4());
        let id = format!("m4-{}", uuid::Uuid::new_v4());
        ost::event_hub::publish(message_fixture(&chat_id, &id, "<p>hello <b>m4</b></p>"));

        let applied = collect_once("default", &store, &bus);
        assert_eq!(applied, 1);

        // Write-through: the message is in the store with the stripped body.
        let stored = store.messages(&chat_id, 10).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, id);
        assert_eq!(stored[0].content, "hello m4");
        assert_eq!(stored[0].raw, "<p>hello <b>m4</b></p>");
        assert_eq!(stored[0].sender_name, "Alice");
        assert_eq!(stored[0].sender_mri, "8:orgid:aaaa");

        // And the bus produced the matching domain event.
        let event = rx.try_recv().expect("a published event");
        match event {
            RealtimeEvent::NewMessage {
                chat_id: ev_chat,
                message,
            } => {
                assert_eq!(ev_chat, chat_id);
                assert_eq!(message.id, id);
                assert_eq!(message.chat_id, chat_id);
            }
            other => panic!("expected NewMessage, got {other:?}"),
        }
    }

    #[test]
    fn collect_once_applies_an_edit_to_an_existing_row() {
        let _guard = hub_guard();
        ost::event_hub::drain(usize::MAX);

        let store = Store::open_in_memory().unwrap();
        let bus = EventBus::default();

        let chat_id = format!("19:m4-edit-{}@unq.gbl.spaces", uuid::Uuid::new_v4());
        let id = format!("m4-{}", uuid::Uuid::new_v4());
        // Seed the row (as history would have).
        store
            .upsert_messages(
                "default",
                &[Message {
                    id: id.clone(),
                    chat_id: chat_id.clone(),
                    sender_mri: "8:orgid:aaaa".into(),
                    sender_name: "Alice".into(),
                    timestamp: "2026-01-02T03:04:05Z".into(),
                    content: "original".into(),
                    raw: "<p>original</p>".into(),
                    reactions: Vec::new(),
                    reply_to: None,
                    client_message_id: None,
                    edited: false,
                    is_own: false,
                    send_state: crate::model::SendState::Sent,
                }],
            )
            .unwrap();

        let mut rx = bus.subscribe();
        ost::event_hub::publish(
            serde_json::json!({
                "resourceType": "MessageUpdate",
                "resource": { "id": id, "conversationId": chat_id, "content": "<p>revised body</p>" }
            })
            .to_string(),
        );

        assert_eq!(collect_once("default", &store, &bus), 1);

        let stored = store.messages(&chat_id, 10).unwrap();
        assert_eq!(stored.len(), 1, "edit must not create a second row");
        assert_eq!(stored[0].content, "revised body");
        assert!(stored[0].edited, "the row is marked edited");
        // Sender / timestamp survive (the update is in place).
        assert_eq!(stored[0].sender_name, "Alice");
        assert_eq!(stored[0].timestamp, "2026-01-02T03:04:05Z");

        assert!(matches!(
            rx.try_recv().expect("edit event"),
            RealtimeEvent::MessageEdited { .. }
        ));
    }

    #[test]
    fn collect_once_applies_a_delete_to_an_existing_row() {
        let _guard = hub_guard();
        ost::event_hub::drain(usize::MAX);

        let store = Store::open_in_memory().unwrap();
        let bus = EventBus::default();

        let chat_id = format!("19:m4-del-{}@unq.gbl.spaces", uuid::Uuid::new_v4());
        let id = format!("m4-{}", uuid::Uuid::new_v4());
        store
            .upsert_messages(
                "default",
                &[Message {
                    id: id.clone(),
                    chat_id: chat_id.clone(),
                    sender_mri: String::new(),
                    sender_name: "Alice".into(),
                    timestamp: "t".into(),
                    content: "bye".into(),
                    raw: "bye".into(),
                    reactions: Vec::new(),
                    reply_to: None,
                    client_message_id: None,
                    edited: false,
                    is_own: false,
                    send_state: crate::model::SendState::Sent,
                }],
            )
            .unwrap();

        ost::event_hub::publish(
            serde_json::json!({
                "resourceType": "MessageDelete",
                "resource": { "id": id, "conversationId": chat_id }
            })
            .to_string(),
        );

        assert_eq!(collect_once("default", &store, &bus), 1);
        assert!(store.messages(&chat_id, 10).unwrap().is_empty());
    }

    #[test]
    fn resync_is_surfaced_on_the_bus() {
        let _guard = hub_guard();
        ost::event_hub::drain(usize::MAX);

        let store = Store::open_in_memory().unwrap();
        let bus = EventBus::default();
        let mut rx = bus.subscribe();

        ost::event_hub::publish(
            serde_json::json!({ "resourceType": "MessageLoss", "resource": {} }).to_string(),
        );

        assert_eq!(collect_once("default", &store, &bus), 1);
        assert!(matches!(
            rx.try_recv().expect("resync event"),
            RealtimeEvent::Resync { .. }
        ));
    }

    #[test]
    fn typing_and_read_state_are_bus_only() {
        let _guard = hub_guard();
        ost::event_hub::drain(usize::MAX);

        let store = Store::open_in_memory().unwrap();
        let bus = EventBus::default();
        let mut rx = bus.subscribe();

        let chat_id = format!("19:m4-bus-{}@unq.gbl.spaces", uuid::Uuid::new_v4());
        ost::event_hub::publish(
            serde_json::json!({
                "resourceType": "Typing",
                "resource": { "conversationId": chat_id, "mri": "8:orgid:eeee", "imdisplayname": "Eve" }
            })
            .to_string(),
        );
        ost::event_hub::publish(
            serde_json::json!({
                "resourceType": "MessageSeen",
                "resource": { "conversationId": chat_id, "mri": "8:orgid:eeee",
                              "consumptionhorizon": "1;1;m1" }
            })
            .to_string(),
        );

        assert_eq!(collect_once("default", &store, &bus), 2);
        assert!(matches!(
            rx.try_recv().unwrap(),
            RealtimeEvent::Typing { .. }
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            RealtimeEvent::ReadState { .. }
        ));
        // Nothing was written for these bus-only events.
        assert_eq!(store.counts().unwrap().messages, 0);
    }

    #[test]
    fn collect_once_on_an_empty_hub_is_a_noop() {
        let _guard = hub_guard();
        ost::event_hub::drain(usize::MAX);
        let store = Store::open_in_memory().unwrap();
        let bus = EventBus::default();
        assert_eq!(collect_once("default", &store, &bus), 0);
    }
}
