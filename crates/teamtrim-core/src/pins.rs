//! Pinned-messages service (backlog item **B2b**).
//!
//! The service behind the conversation info pane's **pinned messages** panel:
//! read a chat's server-side pins, cache them, resolve each pin against the
//! cached message, and unpin.
//!
//! ```text
//!          UI (pinned-messages panel)
//!           │  pinned / cached_pinned / unpin
//!           ▼
//!         Pins ──▶ Arc<Store>      (M3: pinned_messages + messages)
//!           └────▶ Arc<ApiClient>  (M2: the chat-service thread read)
//! ```
//!
//! ## Where pins come from
//!
//! The vendored [`ApiClient::pinned_messages`] reads the **chat-service** thread
//! properties (skypetoken) and returns chat-service pins; the Graph
//! `pinnedMessages` fallback needs `Chat.Read`, which the Teams token lacks
//! (expected 403 live). **Unpinning** is a Graph `DELETE` the Teams token is not
//! granted, so [`ApiClient::unpin_message`] refuses before any network; like
//! B2a's server mute, [`Pins::unpin`] therefore drops the pin from the local
//! cache and treats the server write as best-effort. See ADR-039.
//!
//! ## Resolution
//!
//! The store keeps only a pin's **membership metadata** (sender, preview, time,
//! pinned-at/by, Graph pin id); the message body is resolved from the store on
//! read, so a pin whose message is outside the cached window still lists (its
//! preview comes from the wire) with [`PinnedMessage::message`] `None`.

use std::sync::Arc;

use crate::api::ApiClient;
use crate::error::Result;
use crate::model::PinnedMessage;
use crate::store::Store;

/// The pinned-messages service (see the module docs).
pub struct Pins {
    profile: String,
    store: Arc<Store>,
    client: Arc<ApiClient>,
}

impl Pins {
    /// Build the service over a shared store and protocol client.
    pub fn new(profile: impl Into<String>, store: Arc<Store>, client: Arc<ApiClient>) -> Self {
        Self {
            profile: profile.into(),
            store,
            client,
        }
    }

    /// The account profile this service reads/writes under.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The shared local store.
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// The shared protocol client.
    pub fn client(&self) -> &Arc<ApiClient> {
        &self.client
    }

    /// The **cached** pins of `chat_id`, newest message first, each with its
    /// [`PinnedMessage::message`] resolved from the store when present
    /// (local-only; never touches the network).
    pub fn cached_pinned(&self, chat_id: &str) -> Result<Vec<PinnedMessage>> {
        let pins = self.store.pinned_messages(&self.profile, chat_id)?;
        Ok(self.resolve(chat_id, pins))
    }

    /// Fetch `chat_id`'s server-side pins, cache them and return them with each
    /// message resolved against the store.
    pub async fn pinned(&self, chat_id: &str) -> Result<Vec<PinnedMessage>> {
        let (_source, refs) = self.client.pinned_messages(chat_id).await?;
        let pins: Vec<PinnedMessage> = refs
            .iter()
            .map(|pin| PinnedMessage::from_wire(chat_id, pin))
            .collect();
        self.store
            .replace_chat_pins(&self.profile, chat_id, &pins)?;
        Ok(self.resolve(chat_id, pins))
    }

    /// Unpin one message: drop it from the local cache and attempt the server
    /// write (best-effort — the Teams sign-in cannot unpin, so the server call
    /// always fails today and is logged).
    pub async fn unpin(&self, chat_id: &str, message_id: &str) -> Result<()> {
        if let Err(error) = self.client.unpin_message(chat_id, message_id).await {
            tracing::warn!(
                chat_id,
                message_id,
                %error,
                "server unpin failed; the local pin is removed anyway"
            );
        }
        self.store.remove_pin(&self.profile, chat_id, message_id)?;
        Ok(())
    }

    /// Attach the cached message to each pin that has one.
    fn resolve(&self, chat_id: &str, mut pins: Vec<PinnedMessage>) -> Vec<PinnedMessage> {
        for pin in &mut pins {
            if let Ok(Some(message)) = self.store.message(chat_id, &pin.message_id) {
                pin.message = Some(message);
            }
        }
        pins
    }
}

/// Redacting `Debug`: shows the profile, never a pin or a message body.
impl std::fmt::Debug for Pins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pins")
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{StubResponse, http_stub};
    use crate::auth::tokens::{StoredToken, TokenSet};
    use crate::model::Message;

    fn tokens_for(base: &str) -> TokenSet {
        let future = crate::auth::tokens::now_unix() + 3600;
        let mk = |name: &str| StoredToken::with_expiry(name, future);
        TokenSet {
            refresh_token: "refresh-secret".into(),
            tenant: "contoso.onmicrosoft.com".into(),
            region_gtms: serde_json::json!({ "chatService": base }),
            personal: false,
            spaces: mk("spaces-token"),
            skype: mk("skype-token"),
            graph: mk("graph-token"),
            ic3: mk("ic3-token"),
            recorder: mk("recorder-token"),
        }
    }

    fn service(base: &str, store: Arc<Store>) -> Pins {
        let client = Arc::new(ApiClient::from_tokens(&tokens_for(base)));
        Pins::new("default", store, client)
    }

    fn seed_message(store: &Store, chat_id: &str, id: &str, content: &str) {
        store
            .upsert_messages(
                "default",
                &[Message::from_info(
                    chat_id,
                    &ost::api::MessageInfo {
                        id: id.into(),
                        sender_mri: "8:orgid:peer".into(),
                        sender: "Ava".into(),
                        timestamp: "2026-01-02T03:04:05Z".into(),
                        content: content.into(),
                        raw: format!("<p>{content}</p>"),
                        reactions: Vec::new(),
                        reply_to: None,
                        client_message_id: None,
                    },
                )],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn pinned_fetches_caches_and_resolves_the_message() {
        let body = serde_json::json!({
            "properties": {
                "pinnedMessages": [
                    {
                        "messageId": "m1",
                        "sender": "Ava",
                        "content": "<p>ship it</p>",
                        "originalarrivaltime": "2026-01-02T03:04:05Z"
                    }
                ]
            }
        })
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let store = Arc::new(Store::open_in_memory().unwrap());
        seed_message(&store, "c1", "m1", "ship it");
        let pins = service(&base, Arc::clone(&store));

        let list = pins.pinned("c1").await.expect("pins");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].message_id, "m1");
        assert_eq!(list[0].sender.as_deref(), Some("Ava"));
        assert_eq!(list[0].preview.as_deref(), Some("ship it"));
        // The message is resolved from the store.
        assert!(list[0].message.is_some());
        assert_eq!(list[0].preview_text(), "ship it");

        // It is cached: a local read returns it (with the message resolved).
        let cached = pins.cached_pinned("c1").unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(
            cached[0].message.as_ref().map(|m| m.id.as_str()),
            Some("m1")
        );

        task.await.expect("stub drains");
        let reqs = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(reqs.len(), 1, "preview present, so no fill request");
        assert!(
            reqs[0].starts_with("GET /v1/threads/c1?view=msnp24Equivalent "),
            "{}",
            reqs[0]
        );
    }

    #[tokio::test]
    async fn pinned_fills_a_bare_pin_from_its_message() {
        // The thread names only the id (a bare chat-service arrival-time id);
        // the vendored pass fills sender/preview from the per-message GET.
        let thread = serde_json::json!({ "properties": { "pinnedMessageIds": "1700000000000" } })
            .to_string();
        let message = serde_json::json!({
            "id": "1700000000000",
            "imdisplayname": "Ava",
            "content": "<p>hello <b>there</b></p>",
            "originalarrivaltime": "2026-01-02T03:04:05Z"
        })
        .to_string();
        let (base, seen, task) =
            http_stub(vec![StubResponse::ok(thread), StubResponse::ok(message)]).await;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let pins = service(&base, Arc::clone(&store));

        let list = pins.pinned("c1").await.expect("pins");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].message_id, "1700000000000");
        assert_eq!(list[0].sender.as_deref(), Some("Ava"));
        assert_eq!(list[0].preview.as_deref(), Some("hello there"));
        // No cached message: `message` stays None but the wire preview lists.
        assert!(list[0].message.is_none());

        task.await.expect("stub drains");
        let reqs = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(reqs.len(), 2, "thread read, then the fill GET");
        assert!(
            reqs[1].starts_with("GET /v1/users/ME/conversations/c1/messages/1700000000000 "),
            "{}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn unpin_removes_the_local_pin_even_though_the_server_refuses() {
        let (base, _seen, task) = http_stub(vec![]).await;
        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .replace_chat_pins(
                "default",
                "c1",
                &[PinnedMessage {
                    chat_id: "c1".into(),
                    message_id: "m1".into(),
                    sender: Some("Ava".into()),
                    preview: Some("hi".into()),
                    time: None,
                    pinned_at: None,
                    pinned_by: None,
                    graph_pin_id: None,
                    message: None,
                }],
            )
            .unwrap();
        let pins = service(&base, Arc::clone(&store));

        pins.unpin("c1", "m1").await.expect("unpin");
        assert!(store.pinned_messages("default", "c1").unwrap().is_empty());
        assert!(pins.cached_pinned("c1").unwrap().is_empty());

        task.await.expect("stub drains");
    }

    #[test]
    fn pins_is_send_and_sync_and_debug_is_redacting() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Pins>();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let dbg = format!("{:?}", service("http://127.0.0.1:9", store));
        assert!(dbg.contains("Pins"));
        assert!(dbg.contains("default"));
    }
}
