//! Chat folders service (backlog item **B2b**).
//!
//! The service behind the chat list's **folder tabs**: read the signed-in
//! user's chat folders (Favorites + user-created; the server-generated views
//! are dropped by the vendored parser), cache them, and move a chat into (or
//! out of) a folder.
//!
//! ```text
//!          UI (folder tabs / "Move to folder")
//!           │  folders / cached_folders / move_to_folder
//!           ▼
//!        Folders ──▶ Arc<Store>      (M3: chat_folders + chats.folder_id)
//!           └────▶ Arc<ApiClient>    (M2: the CSA folder read/move)
//! ```
//!
//! ## Where the folder lane writes
//!
//! The folder list is a **CSA** read ([`ApiClient::conversation_folders`], the
//! skype token as a Bearer) and a move is a hierarchy GET + a pure-edit POST
//! ([`ApiClient::move_chat_to_folder`]). Both are standalone `reqwest` calls in
//! the vendored crate against a fixed CSA URL, so — unlike the chat-service
//! lanes — they cannot be pointed at a local stub; see `docs/BLOCKERS.md`
//! (U-012). The service is therefore **local-first**: [`Folders::move_to_folder`]
//! files the chat in the store immediately (so the tabs update offline) and
//! treats the server write as best-effort, re-caching the server's folder state
//! on success and logging on failure.
//!
//! ## Membership model
//!
//! A chat is in **at most one** movable folder — the vendored
//! [`folder_move_actions`](ost::api::folder_move_actions) removes it from every
//! other folder when it adds it to the target. The store therefore keeps the
//! membership as a single nullable `chats.folder_id` column rather than a join
//! table (a join table would model an impossible state); each folder's
//! `item_ids` are rebuilt from that column on read.

use std::sync::Arc;

use crate::api::ApiClient;
use crate::error::Result;
use crate::model::ChatFolder;
use crate::store::Store;

/// The chat-folders service (see the module docs).
pub struct Folders {
    profile: String,
    store: Arc<Store>,
    client: Arc<ApiClient>,
}

impl Folders {
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

    /// The **cached** folders, in server order, each with its `item_ids`
    /// rebuilt from the store (local-only; never touches the network).
    pub fn cached_folders(&self) -> Result<Vec<ChatFolder>> {
        self.store.chat_folders(&self.profile)
    }

    /// Fetch the signed-in user's folders, cache them and return them.
    ///
    /// The fetch rides the CSA host with the skype token (see the module docs);
    /// the whole list is cached with [`Store::replace_chat_folders`], so the
    /// membership is rebuilt from the server's `item_ids`.
    pub async fn folders(&self) -> Result<Vec<ChatFolder>> {
        let wire = self.client.conversation_folders().await?;
        let folders: Vec<ChatFolder> = wire.iter().map(ChatFolder::from_wire).collect();
        self.store.replace_chat_folders(&self.profile, &folders)?;
        Ok(folders)
    }

    /// Resolve a folder **id or type** to a cached folder id, if it names one.
    ///
    /// The UI's "Move to folder" offers a folder id; a caller that only has a
    /// type (`"Favorites"`) is resolved case-insensitively against the cached
    /// folders. An unknown value is returned trimmed as-is (the caller may
    /// still be naming a folder the cache has not seen yet).
    pub fn resolve_folder(&self, folder_or_type: &str) -> Result<String> {
        let needle = folder_or_type.trim();
        if needle.is_empty() {
            return Ok(String::new());
        }
        for folder in self.store.chat_folders(&self.profile)? {
            if folder.id == needle || folder.folder_type.eq_ignore_ascii_case(needle) {
                return Ok(folder.id);
            }
        }
        Ok(needle.to_string())
    }

    /// Move `chat_id` into `folder` (a folder id or type; blank = out of every
    /// folder), **local-first**.
    ///
    /// The membership is written to the store immediately (so the tabs update
    /// offline); the server move is best-effort. On success the server's folder
    /// state is re-cached and returned; on failure the local move is kept and
    /// the cached folders are returned (see the module docs and ADR-039).
    pub async fn move_to_folder(&self, chat_id: &str, folder: &str) -> Result<Vec<ChatFolder>> {
        let target = self.resolve_folder(folder)?;
        let folder_id = if target.is_empty() {
            None
        } else {
            Some(target.as_str())
        };
        self.store
            .set_chat_folder(&self.profile, chat_id, folder_id)?;
        match self.client.move_chat_to_folder(chat_id, &target).await {
            Ok(wire) => {
                let folders: Vec<ChatFolder> = wire.iter().map(ChatFolder::from_wire).collect();
                self.store.replace_chat_folders(&self.profile, &folders)?;
                Ok(folders)
            }
            Err(error) => {
                tracing::warn!(
                    chat_id,
                    folder = target,
                    %error,
                    "server folder move failed; the local membership is kept"
                );
                self.cached_folders()
            }
        }
    }
}

/// Redacting `Debug`: shows the profile, never a folder or a chat id.
impl std::fmt::Debug for Folders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Folders")
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Chat;

    fn chat(id: &str, name: &str) -> Chat {
        Chat {
            id: id.into(),
            name: name.into(),
            is_group: false,
            last_message_time: Some("2026-01-02T00:00:00Z".into()),
            last_message_sender: None,
            last_message_preview: None,
            unread: false,
            pinned: false,
        }
    }

    fn folder(id: &str, name: &str, folder_type: &str, items: &[&str]) -> ChatFolder {
        ChatFolder {
            id: id.into(),
            name: name.into(),
            folder_type: folder_type.into(),
            is_system: false,
            item_ids: items.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// A client with **no** skype token: the folder lane is refused before any
    /// network, so the offline tests never reach a socket. (The folder calls are
    /// standalone `reqwest` calls against a fixed CSA URL and cannot be stubbed
    /// — see `docs/BLOCKERS.md` U-012.)
    fn offline_service(store: Arc<Store>) -> Folders {
        let client = Arc::new(ApiClient::from_config(ost::config::Config::default()));
        Folders::new("default", store, client)
    }

    #[test]
    fn cached_folders_reads_the_store() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .upsert_chats("default", &[chat("c1", "One"), chat("c2", "Two")])
            .unwrap();
        store
            .replace_chat_folders(
                "default",
                &[
                    folder("fav", "Favorites", "Favorites", &["c1"]),
                    folder("work", "Work", "UserCreated", &["c2"]),
                ],
            )
            .unwrap();
        let folders = offline_service(Arc::clone(&store));

        let cached = folders.cached_folders().unwrap();
        assert_eq!(cached.len(), 2);
        assert_eq!(cached[0].id, "fav");
        assert_eq!(cached[0].item_ids, vec!["c1"]);
        assert_eq!(cached[1].item_ids, vec!["c2"]);
    }

    #[test]
    fn resolve_folder_accepts_an_id_or_a_type() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .replace_chat_folders(
                "default",
                &[folder("t~u~Favorites", "Favorites", "Favorites", &[])],
            )
            .unwrap();
        let folders = offline_service(store);

        assert_eq!(
            folders.resolve_folder("t~u~Favorites").unwrap(),
            "t~u~Favorites"
        );
        // A type resolves case-insensitively to the folder id.
        assert_eq!(
            folders.resolve_folder("favorites").unwrap(),
            "t~u~Favorites"
        );
        // Unknown values pass through trimmed; blank clears membership.
        assert_eq!(folders.resolve_folder("  other  ").unwrap(), "other");
        assert_eq!(folders.resolve_folder("").unwrap(), "");
    }

    #[tokio::test]
    async fn move_to_folder_is_local_first_even_when_the_server_is_unreachable() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.upsert_chats("default", &[chat("c1", "One")]).unwrap();
        store
            .replace_chat_folders("default", &[folder("fav", "Favorites", "Favorites", &[])])
            .unwrap();
        let folders = offline_service(Arc::clone(&store));

        // The server move is refused (no token), but the local membership is
        // written and the returned folders reflect it.
        let after = folders.move_to_folder("c1", "fav").await.unwrap();
        assert_eq!(after[0].item_ids, vec!["c1"]);
        assert_eq!(
            store.chat_folder_id("default", "c1").unwrap().as_deref(),
            Some("fav")
        );

        // Moving out (blank target) clears it.
        let after = folders.move_to_folder("c1", "").await.unwrap();
        assert!(after[0].item_ids.is_empty());
        assert_eq!(store.chat_folder_id("default", "c1").unwrap(), None);
    }

    #[tokio::test]
    async fn folders_fetch_without_a_token_is_an_error_and_changes_nothing() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let folders = offline_service(Arc::clone(&store));
        assert!(folders.folders().await.is_err());
        assert!(store.chat_folders("default").unwrap().is_empty());
    }

    #[test]
    fn folders_is_send_and_sync_and_debug_is_redacting() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Folders>();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let dbg = format!("{:?}", offline_service(store));
        assert!(dbg.contains("Folders"));
        assert!(dbg.contains("default"));
    }
}
