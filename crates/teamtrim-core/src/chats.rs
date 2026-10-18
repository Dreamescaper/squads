//! Chat management service (backlog item **B2a**).
//!
//! The service the app's "New chat" flow and chat context menu drive: create a
//! 1:1 or group chat, leave one, write the per-user **server** mute/hide state,
//! and read a chat's roster.
//!
//! ```text
//!          UI (New chat / context menu)
//!           │  new_one_to_one / new_group / leave / set_muted / set_hidden / members
//!           ▼
//!         Chats ──▶ Arc<Store>      (M3: the chat list + flags)
//!           └────▶ Arc<ApiClient>    (M2: the chat-service writes)
//! ```
//!
//! ## Where creation/leave write
//!
//! 1:1 and group creation ride the **chat service** (`POST /v1/threads`): Graph
//! `POST /me/chats` answers `405` and Graph `POST /chats` needs `Chat.Create`,
//! which the Teams first-party token lacks (see [`ApiClient`]). A created chat
//! is mapped into the owned [`Chat`] and upserted so the chat list shows it
//! immediately; the caller then refreshes + opens it. Leaving removes the own
//! MRI from the thread roster **and** drops the cached chat (and its messages)
//! so it disappears from the list.
//!
//! ## The server-vs-local mute reconciliation (ADR-038)
//!
//! M8 already persists a **local** per-chat mute (`chats.muted`) that is the
//! **notification-routing source of truth** ([`crate::notifications::route`]
//! honours it). B2a adds the **server** mute (the `alerts` conversation
//! property) and the server **hide** (`unpinnedTime`/`historyHiddenTime`).
//!
//! [`Chats::set_muted`] sets the **local** flag first (immediate, works
//! offline, keeps routing correct) and then attempts the server write. On
//! success it records the server's state in `chats.server_muted`; on failure it
//! logs and **keeps the local flag** — the server write is best-effort and a
//! failed offline write leaves `server_muted` `NULL` ("unknown") rather than
//! lying. [`Chats::set_hidden`] mirrors this with `chats.hidden`. Both methods
//! return `Ok(())` once the local state is written, so an offline toggle still
//! takes effect locally (the documented trade-off).

use std::sync::Arc;

use crate::api::ApiClient;
use crate::error::Result;
use crate::model::{Chat, ChatMember};
use crate::store::Store;

/// The chat-management service (see the module docs).
pub struct Chats {
    profile: String,
    store: Arc<Store>,
    client: Arc<ApiClient>,
}

impl Chats {
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

    /// Create (or re-open) the 1:1 chat with `user_id` (a Graph/AAD id or UPN)
    /// and cache the returned [`Chat`].
    ///
    /// The vendored 1:1 call returns an **empty** name (Graph carries no topic
    /// for a 1:1); the caller names the thread after the peer, and the next
    /// chat-list refresh resolves the mate name. The caller is expected to
    /// refresh the list and open the chat.
    pub async fn new_one_to_one(&self, user_id: &str) -> Result<Chat> {
        let info = self.client.create_one_to_one_chat(user_id).await?;
        let chat = Chat::from_info(&info);
        self.store
            .upsert_chats(&self.profile, std::slice::from_ref(&chat))?;
        Ok(chat)
    }

    /// Create a group chat with `member_ids` (Graph/AAD ids or UPNs) plus the
    /// signed-in user, with an optional `topic`, and cache the returned
    /// [`Chat`].
    pub async fn new_group(&self, topic: Option<&str>, member_ids: &[String]) -> Result<Chat> {
        let info = self.client.create_group_chat(topic, member_ids).await?;
        let chat = Chat::from_info(&info);
        self.store
            .upsert_chats(&self.profile, std::slice::from_ref(&chat))?;
        Ok(chat)
    }

    /// Leave `chat_id`: remove the own MRI from the thread roster, then drop the
    /// cached chat (and its messages) so it leaves the chat list.
    pub async fn leave(&self, chat_id: &str) -> Result<()> {
        self.client.leave_chat(chat_id).await?;
        self.store.delete_chat(chat_id, &self.profile)?;
        Ok(())
    }

    /// Mute or unmute `chat_id`: set the **local** routing flag (immediate,
    /// offline) and attempt the **server** `alerts` write (best-effort). See the
    /// module docs for the reconciliation.
    pub async fn set_muted(&self, chat_id: &str, muted: bool) -> Result<()> {
        let _ = self.store.set_chat_muted(&self.profile, chat_id, muted);
        match self.client.set_chat_muted(chat_id, muted).await {
            Ok(()) => {
                let _ = self
                    .store
                    .set_chat_server_muted(&self.profile, chat_id, Some(muted));
            }
            Err(error) => {
                tracing::warn!(
                    chat_id,
                    muted,
                    %error,
                    "server mute write failed; the local routing flag is kept"
                );
            }
        }
        Ok(())
    }

    /// Hide or unhide `chat_id`: set the **local** flag (immediate, offline) and
    /// attempt the **server** visibility write (best-effort). See the module
    /// docs.
    pub async fn set_hidden(&self, chat_id: &str, hidden: bool) -> Result<()> {
        let _ = self.store.set_chat_hidden(&self.profile, chat_id, hidden);
        if let Err(error) = self.client.set_chat_hidden(chat_id, hidden).await {
            tracing::warn!(
                chat_id,
                hidden,
                %error,
                "server hide write failed; the local flag is kept"
            );
        }
        Ok(())
    }

    /// The roster of `chat_id`, mapped into the owned [`ChatMember`]s.
    pub async fn members(&self, chat_id: &str) -> Result<Vec<ChatMember>> {
        let wire = self.client.chat_members(chat_id).await?;
        Ok(wire.iter().map(ChatMember::from_info).collect())
    }
}

/// Redacting `Debug`: shows the profile and store/client presence, never a
/// chat id or a roster.
impl std::fmt::Debug for Chats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chats")
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
    use std::sync::Mutex;

    const ME_GUID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    const PEER_GUID: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

    fn tokens_for(base: &str) -> TokenSet {
        let future = crate::auth::tokens::now_unix() + 3600;
        let mk = |name: &str| StoredToken::with_expiry(name, future);
        TokenSet {
            refresh_token: "refresh-secret".into(),
            tenant: "contoso.onmicrosoft.com".into(),
            region_gtms: serde_json::json!({ "graphBase": base, "chatService": base }),
            personal: false,
            spaces: mk("spaces-token"),
            skype: mk("skype-token"),
            graph: mk("graph-token"),
            ic3: mk("ic3-token"),
            recorder: mk("recorder-token"),
        }
    }

    fn service(base: &str, store: Arc<Store>) -> Chats {
        let client = Arc::new(ApiClient::from_tokens(&tokens_for(base)));
        Chats::new("default", store, client)
    }

    fn requests(seen: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn seed_chat(store: &Store, id: &str) {
        store
            .upsert_chats(
                "default",
                &[Chat {
                    id: id.into(),
                    name: "Team".into(),
                    is_group: true,
                    last_message_time: None,
                    last_message_sender: None,
                    last_message_preview: None,
                    unread: false,
                    pinned: false,
                }],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn new_one_to_one_caches_the_created_chat() {
        let (base, _seen, task) = http_stub(vec![
            StubResponse::ok(serde_json::json!({ "id": ME_GUID }).to_string()),
            StubResponse::ok("{}"),
        ])
        .await;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let chats = service(&base, Arc::clone(&store));

        let chat = chats.new_one_to_one(PEER_GUID).await.expect("create 1:1");
        assert_eq!(chat.id, format!("19:{ME_GUID}_{PEER_GUID}@unq.gbl.spaces"));
        // It is in the chat list now (cached).
        assert!(store.chat(&chat.id, "default").unwrap().is_some());

        task.await.expect("stub drains");
    }

    #[tokio::test]
    async fn new_group_caches_the_created_chat() {
        let location = format!("/v1/threads/19%3A{ME_GUID}_{PEER_GUID}%40thread.v2?x=1");
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(serde_json::json!({ "id": ME_GUID }).to_string()),
            StubResponse::ok_location(location, "{}"),
        ])
        .await;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let chats = service(&base, Arc::clone(&store));

        let chat = chats
            .new_group(Some("Launch"), &[PEER_GUID.to_string()])
            .await
            .expect("create group");
        assert!(chat.is_group);
        assert_eq!(chat.name, "Launch");
        assert!(store.chat(&chat.id, "default").unwrap().is_some());

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert!(reqs[1].starts_with("POST /v1/threads "), "{}", reqs[1]);
        assert!(reqs[1].contains("\"topic\":\"Launch\""), "{}", reqs[1]);
    }

    #[tokio::test]
    async fn leave_removes_the_chat_and_its_messages_from_the_store() {
        let (base, _seen, task) = http_stub(vec![
            StubResponse::ok(serde_json::json!({ "id": ME_GUID }).to_string()),
            StubResponse::status(204),
        ])
        .await;
        let store = Arc::new(Store::open_in_memory().unwrap());
        seed_chat(&store, "19:team@thread.v2");
        store
            .upsert_messages(
                "default",
                &[Message::from_info(
                    "19:team@thread.v2",
                    &ost::api::MessageInfo {
                        id: "m1".into(),
                        sender_mri: "8:orgid:peer".into(),
                        sender: "Ava".into(),
                        timestamp: "2026-01-01T00:00:00Z".into(),
                        content: "hi".into(),
                        raw: "<p>hi</p>".into(),
                        reactions: Vec::new(),
                        reply_to: None,
                        client_message_id: None,
                    },
                )],
            )
            .unwrap();
        let chats = service(&base, Arc::clone(&store));

        chats.leave("19:team@thread.v2").await.expect("leave");

        assert!(
            store
                .chat("19:team@thread.v2", "default")
                .unwrap()
                .is_none()
        );
        assert!(
            store.messages("19:team@thread.v2", 10).unwrap().is_empty(),
            "the chat's messages are dropped too"
        );
        task.await.expect("stub drains");
    }

    #[tokio::test]
    async fn set_muted_sets_the_local_flag_and_records_the_server_state() {
        let (base, seen, task) = http_stub(vec![StubResponse::status(200)]).await;
        let store = Arc::new(Store::open_in_memory().unwrap());
        seed_chat(&store, "19:team@thread.v2");
        let chats = service(&base, Arc::clone(&store));

        chats
            .set_muted("19:team@thread.v2", true)
            .await
            .expect("mute");

        // The local routing flag (M8) is set and the server state is recorded.
        assert!(store.is_chat_muted("default", "19:team@thread.v2").unwrap());
        assert_eq!(
            store.server_muted("default", "19:team@thread.v2").unwrap(),
            Some(true)
        );

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert_eq!(reqs.len(), 1);
        assert!(
            reqs[0].starts_with(
                "PUT /v1/users/ME/conversations/19:team@thread.v2/properties?name=alerts "
            ),
            "{}",
            reqs[0]
        );
        assert!(reqs[0].contains("\"alerts\":\"false\""), "{}", reqs[0]);
    }

    #[tokio::test]
    async fn set_muted_keeps_the_local_flag_when_the_server_is_unreachable() {
        // Port 9 is unreachable: the local flag must still take effect (offline
        // routing), and the server state stays unknown.
        let store = Arc::new(Store::open_in_memory().unwrap());
        seed_chat(&store, "19:team@thread.v2");
        let chats = service("http://127.0.0.1:9", Arc::clone(&store));

        chats
            .set_muted("19:team@thread.v2", true)
            .await
            .expect("offline mute still succeeds locally");

        assert!(store.is_chat_muted("default", "19:team@thread.v2").unwrap());
        assert_eq!(
            store.server_muted("default", "19:team@thread.v2").unwrap(),
            None,
            "a failed write leaves the server state unknown, never a lie"
        );
    }

    #[tokio::test]
    async fn set_hidden_sets_the_local_flag_and_writes_the_server() {
        let (base, seen, task) =
            http_stub(vec![StubResponse::status(200), StubResponse::status(200)]).await;
        let store = Arc::new(Store::open_in_memory().unwrap());
        seed_chat(&store, "19:team@thread.v2");
        let chats = service(&base, Arc::clone(&store));

        chats
            .set_hidden("19:team@thread.v2", true)
            .await
            .expect("hide");
        assert!(
            store
                .is_chat_hidden("default", "19:team@thread.v2")
                .unwrap()
        );

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert_eq!(reqs.len(), 2, "hide writes two properties");
        assert!(reqs[0].contains("name=unpinnedTime"), "{}", reqs[0]);
        assert!(reqs[1].contains("name=historyHiddenTime"), "{}", reqs[1]);
    }

    #[tokio::test]
    async fn members_maps_the_roster() {
        let body = serde_json::json!({
            "members": [
                {"id": format!("8:orgid:{ME_GUID}"), "role": "Admin", "friendlyName": "Me"},
                {"id": format!("8:orgid:{PEER_GUID}"), "role": "User", "friendlyName": "Ava"}
            ]
        })
        .to_string();
        let (base, _seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let chats = service(&base, Arc::clone(&store));

        let members = chats.members("19:team@thread.v2").await.expect("members");
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].mri, format!("8:orgid:{ME_GUID}"));
        assert!(members[0].is_owner);
        assert_eq!(members[1].label(), "Ava");

        task.await.expect("stub drains");
    }

    #[test]
    fn chats_is_send_and_sync_and_debug_is_redacting() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Chats>();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let chats = service("http://127.0.0.1:9", store);
        let dbg = format!("{chats:?}");
        assert!(dbg.contains("Chats"));
        assert!(dbg.contains("default"));
    }
}
