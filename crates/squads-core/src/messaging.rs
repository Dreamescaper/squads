//! Offline-first conversation service (milestone **M5**).
//!
//! This is the layer the timeline/composer drive. It ties together the three
//! pieces built in earlier milestones:
//!
//! ```text
//!          UI
//!           │  open / load_older / send_text / …
//!           ▼
//!      Messaging ──▶ Arc<Store>      (M3: instant, offline reads)
//!           └────▶ Arc<ApiClient>    (M2: typed protocol calls)
//! ```
//!
//! ## Offline-first reads
//!
//! [`Messaging::cached`] returns whatever the local [`Store`] already holds —
//! no network, so the UI renders immediately. [`Messaging::open`] then fetches
//! the newest page, maps and upserts it, and returns the **merged** newest page;
//! [`Messaging::load_older`] walks backward through the server's
//! `_metadata.backwardLink`.
//!
//! ## Optimistic sends
//!
//! [`Messaging::send_text`] writes a [`SendState::Pending`] row to the store
//! *before* the POST, then reconciles it: on success the row becomes
//! [`SendState::Sent`] and is re-keyed to the server id (when the receipt named
//! one); on failure it becomes [`SendState::Failed`] and is **kept** so the UI
//! can retry. [`Messaging::retry`] reuses the same `client_message_id` so one
//! logical send can never become two server messages; before re-posting it asks
//! the server (`find_message_by_client_id`) whether the earlier attempt landed.
//! See ADR-014.
//!
//! ## Where the paging cursor lives
//!
//! The backward cursor is held **in memory**, per `chat_id`, inside this
//! service (a `Mutex<HashMap<String, Option<String>>>`). It is intentionally not
//! persisted: reloading history from the newest page after a restart is cheap
//! and already the offline-first path (see ADR-016). Consequence: a fresh
//! `Messaging` handle has no cursor until [`Messaging::open`] is called, and
//! `load_older` on such a handle returns nothing.
//!
//! ## Construction
//!
//! [`Messaging::new`] takes the profile, the shared [`Store`] and the shared
//! [`ApiClient`]. It needs no [`Session`](crate::Session), which is what makes
//! the whole service testable offline against a local stub; the session exposes
//! a convenience constructor via [`Session::messaging`](crate::Session::messaging).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ost::api::MessageInfo;

use crate::api::{ApiClient, ReadReceipt};
use crate::error::{CoreError, Result};
use crate::model::{Message, Reaction, SendState};
use crate::store::Store;

/// Messages fetched by [`Messaging::open`] (the newest page).
pub const OPEN_LIMIT: usize = 50;

/// Messages fetched by [`Messaging::load_older`] (one older page).
pub const OLDER_LIMIT: usize = 50;

/// One page of history returned by [`Messaging::open`] / [`Messaging::load_older`].
///
/// `messages` is **oldest-first** — the order a timeline renders top-to-bottom
/// (newest last). `backward_link` is the cursor for the next older page, or
/// `None` when history is exhausted / the server offered no link.
#[derive(Debug, Clone, Default)]
pub struct HistoryPage {
    /// Oldest-first.
    pub messages: Vec<Message>,
    /// Server `_metadata.backwardLink` for the next older page.
    pub backward_link: Option<String>,
}

/// The offline-first conversation service (see the module docs).
pub struct Messaging {
    profile: String,
    store: Arc<Store>,
    client: Arc<ApiClient>,
    /// The signed-in user's MRI (M13), used to mark server-loaded history as
    /// own. Initialized from the persisted `accounts` row (so a restart keeps
    /// it) and refreshed with [`Messaging::set_self_mri`] once whoami resolves.
    self_mri: Mutex<Option<String>>,
    /// Backward paging cursor per chat id: `Some(link)` = an older page is
    /// available; `None` = history exhausted. Absent = [`Messaging::open`] has
    /// not been called for that chat yet.
    cursors: Mutex<HashMap<String, Option<String>>>,
}

impl Messaging {
    /// Build the service over a shared store and protocol client.
    ///
    /// The self MRI is seeded from the persisted identity, when one exists, so
    /// a service built after a restart already marks own history correctly.
    pub fn new(profile: impl Into<String>, store: Arc<Store>, client: Arc<ApiClient>) -> Self {
        let profile = profile.into();
        let self_mri = store
            .get_account(&profile)
            .ok()
            .flatten()
            .and_then(|account| account.self_mri)
            .filter(|mri| !mri.trim().is_empty());
        Self {
            profile,
            store,
            client,
            self_mri: Mutex::new(self_mri),
            cursors: Mutex::new(HashMap::new()),
        }
    }

    /// Set the self MRI the service marks own messages with (M13).
    pub fn with_self_mri(self, self_mri: impl Into<String>) -> Self {
        self.set_self_mri(self_mri);
        self
    }

    /// Update the self MRI in place. Called when whoami resolves, so an
    /// already-built handle starts marking newly-loaded messages.
    pub fn set_self_mri(&self, self_mri: impl Into<String>) {
        let mri = self_mri.into();
        let mut guard = self.self_mri.lock().unwrap_or_else(|e| e.into_inner());
        if mri.trim().is_empty() {
            *guard = None;
        } else {
            *guard = Some(mri);
        }
    }

    /// The self MRI currently known, if any.
    pub fn self_mri(&self) -> Option<String> {
        self.self_mri
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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

    // ----- reads ----------------------------------------------------------

    /// The newest `limit` cached messages for `chat_id`, oldest-first. Purely
    /// local: never touches the network (the UI's instant first paint).
    pub fn cached(&self, chat_id: &str, limit: usize) -> Result<Vec<Message>> {
        self.store.messages(chat_id, limit)
    }

    /// Fetch the newest page of `chat_id`, merge it into the store and return
    /// the merged newest page plus the backward cursor.
    ///
    /// Call [`Messaging::cached`] first for an immediate render, then this for
    /// the refresh. The merged list is read back from the store, so it also
    /// includes any locally pending rows.
    pub async fn open(&self, chat_id: &str) -> Result<HistoryPage> {
        let page = self
            .client
            .read_messages_page(chat_id, OPEN_LIMIT, None)
            .await?;
        self.upsert_page(chat_id, &page.messages)?;
        self.set_cursor(chat_id, page.backward_link.clone());
        Ok(HistoryPage {
            messages: self.store.messages(chat_id, OPEN_LIMIT)?,
            backward_link: page.backward_link,
        })
    }

    /// Fetch the next older page using the stored cursor, merge it, and return
    /// that older batch (oldest-first) plus the next cursor.
    ///
    /// Returns an empty page when no cursor is known (either [`Messaging::open`]
    /// was never called or history is exhausted).
    pub async fn load_older(&self, chat_id: &str) -> Result<HistoryPage> {
        let Some(link) = self.next_cursor(chat_id) else {
            return Ok(HistoryPage::default());
        };
        let page = self
            .client
            .read_messages_page(chat_id, OLDER_LIMIT, Some(&link))
            .await?;
        let mapped = self.upsert_page(chat_id, &page.messages)?;
        self.set_cursor(chat_id, page.backward_link.clone());
        Ok(HistoryPage {
            messages: mapped,
            backward_link: page.backward_link,
        })
    }

    /// The backward cursor currently known for `chat_id`, if an older page is
    /// available. `None` means no known older page.
    pub fn cursor(&self, chat_id: &str) -> Option<String> {
        self.next_cursor(chat_id)
    }

    // ----- writes ---------------------------------------------------------

    /// Send `text` optimistically (see the module docs).
    ///
    /// (a) a [`SendState::Pending`] row with a fresh `client_message_id` is
    /// upserted first; (b) the POST runs; (c) on success the row is re-keyed to
    /// the server id and marked [`SendState::Sent`]; (d) on failure it is marked
    /// [`SendState::Failed`], retained, and the error is returned.
    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<Message> {
        let client_message_id = ost::api::new_client_message_id();
        let pending = self.pending_message(chat_id, text, &client_message_id, None);
        self.store
            .upsert_messages(&self.profile, std::slice::from_ref(&pending))?;

        match self
            .client
            .send_message(chat_id, text, &client_message_id)
            .await
        {
            Ok(sent) => self.finalize_send(&pending, sent.id),
            Err(err) => {
                self.mark_state(&pending, SendState::Failed)?;
                Err(err)
            }
        }
    }

    /// Retry a pending/failed send, reusing its `client_message_id`.
    ///
    /// Idempotent: before re-posting it asks the server whether the earlier
    /// attempt actually landed ([`ApiClient::find_message_by_client_id`]); if it
    /// did, the local row is reconciled to [`SendState::Sent`] instead of being
    /// posted twice. A message already [`SendState::Sent`] is returned as-is.
    pub async fn retry(&self, chat_id: &str, client_message_id: &str) -> Result<Message> {
        let existing = self
            .store
            .message_by_client_id(chat_id, client_message_id)?
            .ok_or_else(|| {
                CoreError::NotReady(format!(
                    "no local message with client id {client_message_id}"
                ))
            })?;
        if existing.send_state == SendState::Sent {
            return Ok(existing);
        }

        // Ambiguity guard: a POST that timed out may still have landed.
        if let Some(server) = self
            .client
            .find_message_by_client_id(chat_id, client_message_id)
            .await?
        {
            let mut sent = Message::from_info(chat_id, &server);
            sent.is_own = true;
            sent.send_state = SendState::Sent;
            if sent.id != existing.id {
                self.store
                    .delete_message(&self.profile, chat_id, &existing.id)?;
            }
            self.store
                .upsert_messages(&self.profile, std::slice::from_ref(&sent))?;
            return Ok(sent);
        }

        // Re-post with the SAME client message id (idempotency key).
        match self
            .client
            .send_message(chat_id, &existing.content, client_message_id)
            .await
        {
            Ok(sent) => self.finalize_send(&existing, sent.id),
            Err(err) => {
                self.mark_state(&existing, SendState::Failed)?;
                Err(err)
            }
        }
    }

    /// Post a quote reply to `parent_id`, embedding the parent's sender and a
    /// one-line snippet (resolved from the store, best effort), then cache the
    /// resulting message with its `reply_to` link.
    pub async fn reply(&self, chat_id: &str, parent_id: &str, text: &str) -> Result<Message> {
        let parent = self.store.message(chat_id, parent_id)?;
        let parent_sender = parent
            .as_ref()
            .map(|p| p.sender_name.clone())
            .unwrap_or_default();
        let parent_text = parent
            .as_ref()
            .map(|p| p.content.clone())
            .unwrap_or_default();

        let client_message_id = ost::api::new_client_message_id();
        let sent = self
            .client
            .reply_message(
                chat_id,
                parent_id,
                &parent_sender,
                &parent_text,
                text,
                &client_message_id,
            )
            .await?;

        let id = sent.id.unwrap_or_else(|| client_message_id.clone());
        let message = Message {
            id,
            chat_id: chat_id.to_string(),
            sender_mri: String::new(),
            sender_name: String::new(),
            timestamp: now_iso(),
            content: text.to_string(),
            raw: ost::api::build_reply_html(parent_id, &parent_sender, &parent_text, text),
            reactions: Vec::new(),
            reply_to: Some(parent_id.to_string()),
            client_message_id: Some(client_message_id),
            edited: false,
            is_own: true,
            send_state: SendState::Sent,
        };
        self.store
            .upsert_messages(&self.profile, std::slice::from_ref(&message))?;
        Ok(message)
    }

    /// Edit one of our own messages, updating the cached row's text in place.
    pub async fn edit(&self, chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        self.client.edit_message(chat_id, message_id, text).await?;
        self.store
            .update_message_edit(&self.profile, chat_id, message_id, text)?;
        Ok(())
    }

    /// Delete one of our own messages (soft delete) and drop the cached row.
    pub async fn delete(&self, chat_id: &str, message_id: &str) -> Result<()> {
        self.client.delete_message(chat_id, message_id).await?;
        self.store
            .delete_message(&self.profile, chat_id, message_id)?;
        Ok(())
    }

    /// Add or remove an emoji reaction, adjusting the cached row's counts.
    ///
    /// The local adjustment is deliberately coarse (count ± 1, no reactor
    /// identity — we do not know our own MRI here); the authoritative reactor
    /// list arrives with the next realtime update or history refresh.
    pub async fn react(
        &self,
        chat_id: &str,
        message_id: &str,
        emoji: &str,
        remove: bool,
    ) -> Result<()> {
        if remove {
            self.client
                .remove_reaction(chat_id, message_id, emoji)
                .await?;
        } else {
            self.client
                .send_reaction(chat_id, message_id, emoji)
                .await?;
        }
        if let Some(mut message) = self.store.message(chat_id, message_id)? {
            apply_reaction(&mut message.reactions, emoji, remove);
            self.store
                .upsert_messages(&self.profile, std::slice::from_ref(&message))?;
        }
        Ok(())
    }

    /// Mark `chat_id` read up to `message_id` and record the horizon locally.
    pub async fn mark_read(&self, chat_id: &str, message_id: &str) -> Result<()> {
        self.client.mark_read(chat_id, message_id).await?;
        self.store
            .set_read_state(&self.profile, chat_id, message_id)?;
        Ok(())
    }

    /// Peer read positions for a thread (read-only pass-through).
    pub async fn read_receipts(&self, thread_id: &str) -> Result<Vec<ReadReceipt>> {
        self.client.read_receipts(thread_id).await
    }

    // ----- internals ------------------------------------------------------

    /// Map and upsert a wire page; returns the mapped messages (oldest-first).
    ///
    /// Each message is marked own against the current self MRI (M13), so
    /// server-loaded history renders on the correct side once whoami has
    /// resolved.
    fn upsert_page(&self, chat_id: &str, infos: &[MessageInfo]) -> Result<Vec<Message>> {
        let self_mri = self.self_mri();
        let mapped: Vec<Message> = infos
            .iter()
            .map(|info| {
                let mut message = Message::from_info(chat_id, info);
                if let Some(mri) = &self_mri {
                    message.mark_own(mri);
                }
                message
            })
            .collect();
        self.store.upsert_messages(&self.profile, &mapped)?;
        Ok(mapped)
    }

    /// Build the locally-authored pending row for an outgoing message.
    fn pending_message(
        &self,
        chat_id: &str,
        text: &str,
        client_message_id: &str,
        reply_to: Option<&str>,
    ) -> Message {
        Message {
            // Until the server names the real id, the idempotency key stands in
            // for it (and is what the store row is keyed by).
            id: client_message_id.to_string(),
            chat_id: chat_id.to_string(),
            sender_mri: String::new(),
            sender_name: String::new(),
            timestamp: now_iso(),
            content: text.to_string(),
            raw: ost::api::build_message_html(text),
            reactions: Vec::new(),
            reply_to: reply_to.map(str::to_string),
            client_message_id: Some(client_message_id.to_string()),
            edited: false,
            is_own: true,
            send_state: SendState::Pending,
        }
    }

    /// Reconcile a successful send: re-key to the server id (when it differs)
    /// and mark [`SendState::Sent`].
    fn finalize_send(&self, pending: &Message, server_id: Option<String>) -> Result<Message> {
        let mut sent = pending.clone();
        sent.send_state = SendState::Sent;
        match server_id {
            Some(server_id) if server_id != pending.id => {
                // The row's primary key is (id, chat_id), so a new id is a new
                // row: replace the placeholder.
                self.store
                    .delete_message(&self.profile, &pending.chat_id, &pending.id)?;
                sent.id = server_id;
                self.store
                    .upsert_messages(&self.profile, std::slice::from_ref(&sent))?;
            }
            _ => {
                self.store.update_message_send_state(
                    &self.profile,
                    &pending.chat_id,
                    &pending.id,
                    SendState::Sent,
                )?;
            }
        }
        Ok(sent)
    }

    fn mark_state(&self, message: &Message, state: SendState) -> Result<()> {
        self.store.update_message_send_state(
            &self.profile,
            &message.chat_id,
            &message.id,
            state,
        )?;
        Ok(())
    }

    fn set_cursor(&self, chat_id: &str, link: Option<String>) {
        self.lock_cursors().insert(chat_id.to_string(), link);
    }

    fn next_cursor(&self, chat_id: &str) -> Option<String> {
        self.lock_cursors().get(chat_id).cloned().flatten()
    }

    fn lock_cursors(&self) -> std::sync::MutexGuard<'_, HashMap<String, Option<String>>> {
        self.cursors.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Adjust a reaction-count list locally: `+1` on add (inserting a new emoji
/// entry when absent), `-1` on remove (dropping the entry at zero).
fn apply_reaction(reactions: &mut Vec<Reaction>, emoji: &str, remove: bool) {
    if remove {
        if let Some(i) = reactions.iter().position(|r| r.emoji == emoji) {
            if reactions[i].count <= 1 {
                reactions.remove(i);
            } else {
                reactions[i].count -= 1;
            }
        }
    } else if let Some(entry) = reactions.iter_mut().find(|r| r.emoji == emoji) {
        entry.count += 1;
    } else {
        reactions.push(Reaction {
            emoji: emoji.to_string(),
            count: 1,
            reactors: Vec::new(),
        });
    }
}

/// Current UTC time as a Teams-shaped ISO-8601 string (millisecond precision).
///
/// Used only to order locally-authored rows: it makes a pending bubble sort as
/// the newest message until the server's real timestamp arrives.
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Redacting `Debug`: no message bodies, just counts and the chat the cursor
/// map knows about.
impl std::fmt::Debug for Messaging {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cursors = self.lock_cursors().len();
        f.debug_struct("Messaging")
            .field("profile", &self.profile)
            .field("has_self_mri", &self.self_mri().is_some())
            .field("cursors", &cursors)
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{StubResponse, http_stub, http_stub_with};
    use crate::auth::tokens::{StoredToken, TokenSet};

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

    fn test_messaging(base: &str) -> (Arc<Store>, Messaging) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let client = Arc::new(ApiClient::from_tokens(&tokens_for(base)));
        let messaging = Messaging::new("default", Arc::clone(&store), client);
        (store, messaging)
    }

    fn message(chat_id: &str, id: &str, sender: &str, content: &str) -> Message {
        Message {
            id: id.into(),
            chat_id: chat_id.into(),
            sender_mri: format!("8:orgid:{sender}"),
            sender_name: sender.into(),
            timestamp: format!("2026-01-01T00:00:{id:0>2}Z"),
            content: content.into(),
            raw: format!("<p>{content}</p>"),
            reactions: Vec::new(),
            reply_to: None,
            client_message_id: None,
            edited: false,
            is_own: false,
            send_state: SendState::Sent,
        }
    }

    fn ids(messages: &[Message]) -> Vec<&str> {
        messages.iter().map(|m| m.id.as_str()).collect()
    }

    #[tokio::test]
    async fn send_text_posts_richtext_with_client_message_id_and_captures_server_id() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok_location(
            "https://chat.example/v1/users/ME/conversations/19:c@unq.gbl.spaces/messages/1700000000000",
            "{}",
        )])
        .await;
        let (store, messaging) = test_messaging(&base);

        let sent = messaging
            .send_text("19:c@unq.gbl.spaces", "hello world")
            .await
            .expect("send");
        assert_eq!(sent.send_state, SendState::Sent);
        assert_eq!(sent.id, "1700000000000");
        assert!(sent.is_own);
        let cmid = sent.client_message_id.clone().expect("client message id");

        // The store holds exactly one Sent row keyed by the server id.
        let rows = store.messages("19:c@unq.gbl.spaces", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "1700000000000");
        assert_eq!(rows[0].send_state, SendState::Sent);
        assert!(rows[0].is_own);
        assert_eq!(rows[0].client_message_id.as_deref(), Some(cmid.as_str()));

        task.await.expect("stub drains");
        let req = seen.lock().unwrap_or_else(|e| e.into_inner())[0].clone();
        assert!(
            req.starts_with("POST /v1/users/ME/conversations/19:c@unq.gbl.spaces/messages "),
            "unexpected request line: {req}"
        );
        assert!(req.contains("\"content\":\"<p>hello world</p>\""), "{req}");
        assert!(req.contains("\"messagetype\":\"RichText/Html\""), "{req}");
        assert!(
            req.contains(&format!("\"clientmessageid\":\"{cmid}\"")),
            "the generated clientmessageid must be on the wire: {req}"
        );
    }

    #[tokio::test]
    async fn send_text_is_optimistic_pending_then_sent() {
        let (arrive_tx, arrive_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut response = StubResponse::ok_location(
            "https://chat.example/v1/users/ME/conversations/c1/messages/1700000000001",
            "{}",
        );
        response.arrive = Some(arrive_tx);
        response.release = Some(release_rx);
        let (base, _seen, task) = http_stub(vec![response]).await;
        let (store, messaging) = test_messaging(&base);

        let messaging = Arc::new(messaging);
        let send = {
            let messaging = Arc::clone(&messaging);
            tokio::spawn(async move { messaging.send_text("c1", "hi").await })
        };

        // The stub has read the request but not yet replied.
        arrive_rx.await.expect("request arrived");
        let rows = store.messages("c1", 10).unwrap();
        assert_eq!(rows.len(), 1, "the pending row exists before the reply");
        assert_eq!(rows[0].send_state, SendState::Pending);
        assert!(rows[0].is_own);
        assert_eq!(rows[0].content, "hi");
        assert!(rows[0].client_message_id.is_some());

        release_tx.send(()).expect("release the stub");
        let sent = send.await.expect("task joins").expect("send ok");
        assert_eq!(sent.send_state, SendState::Sent);
        assert_eq!(sent.id, "1700000000001");

        let rows = store.messages("c1", 10).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "the pending row was replaced, not duplicated"
        );
        assert_eq!(rows[0].id, "1700000000001");
        assert_eq!(rows[0].send_state, SendState::Sent);
        task.await.expect("stub drains");
    }

    #[tokio::test]
    async fn failed_send_is_retained_and_retry_reuses_client_message_id() {
        let (base, seen, task) = http_stub(vec![
            StubResponse::status(500),
            // The retry's ambiguity probe: a page with no matching cmid.
            StubResponse::ok("{\"messages\":[]}"),
            StubResponse::ok_location(
                "https://chat.example/v1/users/ME/conversations/c1/messages/42",
                "{}",
            ),
        ])
        .await;
        let (store, messaging) = test_messaging(&base);

        let err = messaging.send_text("c1", "retry me").await.unwrap_err();
        assert!(format!("{err}").contains("500"), "err = {err}");

        let failed = store.messages("c1", 10).unwrap();
        assert_eq!(failed.len(), 1, "the failed row is retained for retry");
        assert_eq!(failed[0].send_state, SendState::Failed);
        let cmid = failed[0].client_message_id.clone().expect("cmid");
        assert_eq!(failed[0].id, cmid, "the failed row keeps the client id");

        let sent = messaging.retry("c1", &cmid).await.expect("retry succeeds");
        assert_eq!(sent.send_state, SendState::Sent);
        assert_eq!(sent.id, "42");

        let rows = store.messages("c1", 10).unwrap();
        assert_eq!(rows.len(), 1, "retry must not duplicate the row");
        assert_eq!(rows[0].id, "42");
        assert_eq!(rows[0].send_state, SendState::Sent);
        assert_eq!(rows[0].client_message_id.as_deref(), Some(cmid.as_str()));

        task.await.expect("stub drains");
        let reqs = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(reqs.len(), 3, "POST, probe GET, POST");
        let needle = format!("\"clientmessageid\":\"{cmid}\"");
        assert!(
            reqs[0].starts_with("POST ") && reqs[0].contains(&needle),
            "{}",
            reqs[0]
        );
        assert!(
            reqs[1].starts_with("GET /v1/users/ME/conversations/c1/messages"),
            "{}",
            reqs[1]
        );
        assert!(
            reqs[2].starts_with("POST ") && reqs[2].contains(&needle),
            "the retry must reuse the same clientmessageid: {}",
            reqs[2]
        );
    }

    #[tokio::test]
    async fn open_then_load_older_pages_backward_without_duplicates() {
        let chat = "19:p@unq.gbl.spaces";
        let (base, seen, task) = http_stub_with(|base| {
            let link =
                format!("{base}/v1/users/ME/conversations/{chat}/messages?pageSize=2&older=1");
            let page1 = serde_json::json!({
                "messages": [
                    { "id": "m3", "content": "<p>three</p>", "messagetype": "RichText/Html",
                      "originalarrivaltime": "2026-01-03T00:00:00Z" },
                    { "id": "m2", "content": "<p>two</p>", "messagetype": "RichText/Html",
                      "originalarrivaltime": "2026-01-02T00:00:00Z" }
                ],
                "_metadata": { "backwardLink": link }
            });
            let page2 = serde_json::json!({
                "messages": [
                    { "id": "m1", "content": "<p>one</p>", "messagetype": "RichText/Html",
                      "originalarrivaltime": "2026-01-01T01:00:00Z" },
                    { "id": "m0", "content": "<p>zero</p>", "messagetype": "RichText/Html",
                      "originalarrivaltime": "2026-01-01T00:00:00Z" }
                ],
                "_metadata": { "backwardLink": serde_json::Value::Null }
            });
            vec![
                StubResponse::ok(page1.to_string()),
                StubResponse::ok(page2.to_string()),
            ]
        })
        .await;
        let (store, messaging) = test_messaging(&base);

        let first = messaging.open(chat).await.expect("open");
        assert_eq!(ids(&first.messages), vec!["m2", "m3"], "oldest-first");
        assert!(first.backward_link.is_some());
        assert_eq!(messaging.cursor(chat), first.backward_link);

        let older = messaging.load_older(chat).await.expect("load older");
        assert_eq!(ids(&older.messages), vec!["m0", "m1"]);
        assert!(older.backward_link.is_none(), "history exhausted");
        assert_eq!(messaging.cursor(chat), None);

        // The store holds the fully merged, ordered, de-duplicated history.
        let all = store.messages(chat, 10).unwrap();
        assert_eq!(ids(&all), vec!["m0", "m1", "m2", "m3"]);

        // A further load returns nothing (no cursor).
        assert!(
            messaging
                .load_older(chat)
                .await
                .unwrap()
                .messages
                .is_empty()
        );

        task.await.expect("stub drains");
        let reqs = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(reqs[0].contains("pageSize=50"), "{}", reqs[0]);
        assert!(
            reqs[1].starts_with("GET ") && reqs[1].contains("older=1"),
            "the second page must follow the backwardLink: {}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn edit_puts_new_text_and_marks_the_cached_row_edited() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok("{}")]).await;
        let (store, messaging) = test_messaging(&base);
        store
            .upsert_messages("default", &[message("c1", "m1", "Alice", "original")])
            .unwrap();

        messaging.edit("c1", "m1", "updated").await.expect("edit");

        let row = &store.messages("c1", 10).unwrap()[0];
        assert_eq!(row.content, "updated");
        assert!(row.edited);
        assert_eq!(row.sender_name, "Alice", "an edit keeps the sender");

        task.await.expect("stub drains");
        let req = seen.lock().unwrap_or_else(|e| e.into_inner())[0].clone();
        assert!(
            req.starts_with("PUT /v1/users/ME/conversations/c1/messages/m1 "),
            "{req}"
        );
        assert!(req.contains("\"content\":\"<p>updated</p>\""), "{req}");
        assert!(req.contains("\"skypeeditedid\":\"m1\""), "{req}");
    }

    #[tokio::test]
    async fn delete_sends_soft_delete_and_drops_the_row() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok("{}")]).await;
        let (store, messaging) = test_messaging(&base);
        store
            .upsert_messages("default", &[message("c1", "m1", "Alice", "bye")])
            .unwrap();

        messaging.delete("c1", "m1").await.expect("delete");
        assert!(store.messages("c1", 10).unwrap().is_empty());

        task.await.expect("stub drains");
        let req = seen.lock().unwrap_or_else(|e| e.into_inner())[0].clone();
        assert!(
            req.starts_with(
                "DELETE /v1/users/ME/conversations/c1/messages/m1?behavior=softDelete "
            ),
            "{req}"
        );
    }

    #[tokio::test]
    async fn reply_posts_a_quote_block_and_links_the_parent() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok_location(
            "https://chat.example/v1/users/ME/conversations/c1/messages/77",
            "{}",
        )])
        .await;
        let (store, messaging) = test_messaging(&base);
        store
            .upsert_messages("default", &[message("c1", "m0", "Alice", "the parent")])
            .unwrap();

        let reply = messaging
            .reply("c1", "m0", "sure thing")
            .await
            .expect("reply");
        assert_eq!(reply.reply_to.as_deref(), Some("m0"));
        assert_eq!(reply.id, "77");
        assert!(reply.raw.contains("<quote"), "raw = {}", reply.raw);

        let row = store.message("c1", "77").unwrap().unwrap();
        assert_eq!(row.reply_to.as_deref(), Some("m0"));
        assert!(row.raw.contains("the parent"), "the parent snippet is kept");

        task.await.expect("stub drains");
        let req = seen.lock().unwrap_or_else(|e| e.into_inner())[0].clone();
        assert!(
            req.starts_with("POST /v1/users/ME/conversations/c1/messages "),
            "{req}"
        );
        assert!(req.contains("<quote"), "{req}");
        assert!(req.contains("the parent"), "{req}");
        assert!(req.contains("sure thing"), "{req}");
        assert!(req.contains("guid=\\\"m0\\\""), "{req}");
    }

    #[tokio::test]
    async fn reactions_hit_the_reaction_endpoints_and_update_the_store() {
        let (base, seen, task) =
            http_stub(vec![StubResponse::ok("{}"), StubResponse::ok("{}")]).await;
        let (store, messaging) = test_messaging(&base);
        store
            .upsert_messages("default", &[message("c1", "m1", "Alice", "react to me")])
            .unwrap();

        messaging
            .react("c1", "m1", "👍", false)
            .await
            .expect("add reaction");
        let row = store.message("c1", "m1").unwrap().unwrap();
        assert_eq!(row.reactions.len(), 1);
        assert_eq!(row.reactions[0].emoji, "👍");
        assert_eq!(row.reactions[0].count, 1);

        messaging
            .react("c1", "m1", "👍", true)
            .await
            .expect("remove reaction");
        assert!(
            store
                .message("c1", "m1")
                .unwrap()
                .unwrap()
                .reactions
                .is_empty()
        );

        task.await.expect("stub drains");
        let reqs = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            reqs[0].starts_with("POST /v1/users/ME/conversations/c1/messages/m1/reactions "),
            "{}",
            reqs[0]
        );
        assert!(reqs[0].contains("\"reactionType\":\"like\""), "{}", reqs[0]);
        assert!(
            reqs[1].starts_with("DELETE /v1/users/ME/conversations/c1/messages/m1/reactions/like "),
            "{}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn mark_read_puts_the_consumption_horizon_and_records_it() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok("{}")]).await;
        let (store, messaging) = test_messaging(&base);

        messaging.mark_read("c1", "m5").await.expect("mark read");
        assert_eq!(
            store.read_state("default", "c1").unwrap().as_deref(),
            Some("m5")
        );

        task.await.expect("stub drains");
        let req = seen.lock().unwrap_or_else(|e| e.into_inner())[0].clone();
        assert!(
            req.starts_with(
                "PUT /v1/users/ME/conversations/c1/properties?name=consumptionhorizon "
            ),
            "{req}"
        );
        assert!(req.contains("\"consumptionhorizon\""), "{req}");
        assert!(req.contains(";m5\""), "{req}");
    }

    #[tokio::test]
    async fn read_receipts_returns_peer_horizons() {
        let body = serde_json::json!({
            "consumptionhorizons": [
                { "mri": "8:orgid:bbbb", "consumptionhorizon": "1;1;m7" }
            ]
        })
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let (_store, messaging) = test_messaging(&base);

        let receipts = messaging.read_receipts("c1").await.expect("receipts");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].message_id, "m7");
        assert_eq!(receipts[0].user, "8:orgid:bbbb");

        task.await.expect("stub drains");
        let req = seen.lock().unwrap_or_else(|e| e.into_inner())[0].clone();
        assert!(
            req.starts_with("GET /v1/threads/c1/consumptionhorizons "),
            "{req}"
        );
    }

    #[test]
    fn cached_reads_the_store_without_network() {
        // No stub: `cached` must never touch the network.
        let (store, messaging) = test_messaging("http://127.0.0.1:9");
        store
            .upsert_messages("default", &[message("c1", "m1", "Alice", "cached")])
            .unwrap();
        let rows = messaging.cached("c1", 10).unwrap();
        assert_eq!(ids(&rows), vec!["m1"]);
        assert_eq!(messaging.profile(), "default");
        assert_eq!(messaging.client().request_count(), 0, "no request was made");
    }

    #[test]
    fn messaging_is_send_and_sync_and_debug_is_redacting() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Messaging>();
        let (_store, messaging) = test_messaging("http://127.0.0.1:9");
        let dbg = format!("{messaging:?}");
        assert!(dbg.contains("Messaging"));
        assert!(dbg.contains("default"));
    }

    // ----- M13: own-message recognition -----------------------------------

    fn two_sender_page() -> String {
        serde_json::json!({
            "messages": [
                { "id": "m1", "content": "<p>mine</p>", "messagetype": "RichText/Html",
                  "originalarrivaltime": "2026-01-01T00:00:00Z",
                  "from": "https://teams.microsoft.com/api/v1/users/ME/contacts/8:orgid:self" },
                { "id": "m2", "content": "<p>theirs</p>", "messagetype": "RichText/Html",
                  "originalarrivaltime": "2026-01-02T00:00:00Z",
                  "from": "https://teams.microsoft.com/api/v1/users/ME/contacts/8:orgid:other" }
            ],
            "_metadata": { "backwardLink": serde_json::Value::Null }
        })
        .to_string()
    }

    #[tokio::test]
    async fn server_loaded_history_marks_own_messages_once_the_mri_is_known() {
        let chat = "19:self@unq.gbl.spaces";
        let (base, _seen, task) = http_stub(vec![
            StubResponse::ok(two_sender_page()),
            StubResponse::ok(two_sender_page()),
        ])
        .await;
        let (_store, messaging) = test_messaging(&base);

        // Without an MRI nothing can be recognised as own.
        assert!(messaging.self_mri().is_none());
        let unknown = messaging.open(chat).await.expect("open");
        assert!(
            unknown.messages.iter().all(|m| !m.is_own),
            "no MRI known: nothing is own"
        );

        // Once whoami has resolved, re-reading the page marks our message.
        messaging.set_self_mri("8:orgid:self");
        assert_eq!(messaging.self_mri().as_deref(), Some("8:orgid:self"));
        let page = messaging.open(chat).await.expect("re-open");
        assert_eq!(
            page.messages.iter().map(|m| m.is_own).collect::<Vec<_>>(),
            vec![true, false],
            "only our message is own"
        );
        task.await.expect("stub drains");
    }

    #[tokio::test]
    async fn a_bare_oid_self_mri_still_matches() {
        let chat = "19:self@unq.gbl.spaces";
        let (base, _seen, task) = http_stub(vec![StubResponse::ok(two_sender_page())]).await;
        let (_store, messaging) = test_messaging(&base);
        messaging.set_self_mri("self"); // bare OID form
        let page = messaging.open(chat).await.expect("open");
        assert!(page.messages[0].is_own);
        assert!(!page.messages[1].is_own);
        task.await.expect("stub drains");
    }

    #[test]
    fn a_new_handle_seeds_the_self_mri_from_the_persisted_account() {
        let (store, _messaging) = test_messaging("http://127.0.0.1:9");
        store
            .put_account(&crate::store::Account {
                profile: "default".into(),
                tenant: None,
                personal: false,
                display_name: None,
                self_mri: Some("8:orgid:self".into()),
                oid: Some("self".into()),
                created_at: 1,
            })
            .unwrap();
        let client = Arc::new(ApiClient::from_tokens(&tokens_for("http://127.0.0.1:9")));
        let fresh = Messaging::new("default", Arc::clone(&store), client);
        assert_eq!(fresh.self_mri().as_deref(), Some("8:orgid:self"));
    }
}
