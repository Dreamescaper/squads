//! Offline-first files service (milestone **M7**).
//!
//! This is the layer the M10 files browser drives. Like
//! [`Messaging`](crate::messaging) and [`Teams`](crate::teams), it ties the
//! shared [`Store`] (instant, offline reads) to the shared [`ApiClient`] (typed
//! Graph calls), and every method is testable offline against a local TCP stub.
//!
//! ```text
//!                    UI (M10)
//!                     │  list_chat_files / list_folder / recents / download /
//!                     │  upload / create_link / versions / rename / …
//!                     ▼
//!                   Files ──▶ Arc<Store>      (M3: cached driveItems)
//!                     └────▶ Arc<ApiClient>    (M2: typed Graph calls)
//! ```
//!
//! ## Everything is Graph
//!
//! Unlike chats/teams, files are **Graph throughout**: listing, download,
//! upload, sharing links, versions and the rename/move/copy/delete manage
//! actions all ride the Graph token and are keyed by `(drive_id, item_id)`.
//! - A **chat** Shared list is mined from chat-service history
//!   (`properties.files`), then each ref is resolved through Graph
//!   `/shares/{u!<base64url>}/driveItem`.
//! - A **channel** list rides the channel `filesFolder` (Graph).
//! - **Recents** is `/me/drive/recent`.
//!
//! ## Upload: simple PUT vs. resumable session
//!
//! [`Files::upload`] wraps the vendored uploader, which picks the transport by
//! size: files **≤ 4 MiB** go in a **single** `PUT …/content`; larger files
//! **create an upload session** and `PUT` sequential `<320 KiB-multiple>`
//! fragments, each carrying a `Content-Range` header. Either way the uploaded
//! driveItem is then posted into the chat as a **`reference` attachment
//! message** (chat service first, Graph fallback). The progress callback fires
//! once per fragment (simple PUT reports once at completion).
//!
//! ## Download progress is coarse
//!
//! The vendored downloader **buffers the whole response body** before writing
//! it, so there is no streaming here: [`Files::download`] reports only
//! `(0, size)` before the transfer and `(written, size)` after. `size` is the
//! cached driveItem size and may be `0` when unknown.
//!
//! ## The store cache
//!
//! Listed driveItems are upserted through [`Store::upsert_files`]; the cache is
//! **additive** (a later recents/folder listing never wipes a chat's attachment
//! id or a cached share link). Live list calls return the freshly mapped wire
//! order; [`Files::cached_chat_files`] is the offline read (modified-desc).

use std::sync::Arc;

use crate::api::ApiClient;
use crate::error::{CoreError, Result};
use crate::model::{FileEntry, FileVersion, SharedLink};
use crate::store::Store;

/// Files fetched by the chat/folder/recents listings.
pub const LIST_LIMIT: usize = 100;

/// The offline-first files service (see the module docs).
pub struct Files {
    profile: String,
    store: Arc<Store>,
    client: Arc<ApiClient>,
}

impl Files {
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

    // ----- reads ----------------------------------------------------------

    /// The files cached for one chat. Purely local: never touches the network
    /// (the UI's instant first paint).
    pub fn cached_chat_files(&self, chat_id: &str) -> Result<Vec<FileEntry>> {
        self.store.files_for_chat(&self.profile, chat_id)
    }

    /// Fetch the files shared in a chat or channel, cache them under
    /// `chat_id`, and return them in the wire order (newest shared first for a
    /// chat list).
    ///
    /// Folders are included so the browser can drill into them with
    /// [`Files::list_folder`]. Call [`Files::cached_chat_files`] first for an
    /// immediate render, then this for the refresh.
    pub async fn list_chat_files(&self, chat_id: &str) -> Result<Vec<FileEntry>> {
        let wire = self
            .client
            .list_chat_files(chat_id, LIST_LIMIT, true)
            .await?;
        let entries: Vec<FileEntry> = wire.iter().map(FileEntry::from_shared).collect();
        self.store
            .upsert_files(&self.profile, Some(chat_id), &entries)?;
        Ok(entries)
    }

    /// The files one message shares (timeline file chips), cached under the
    /// conversation.
    pub async fn message_files(
        &self,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<Vec<FileEntry>> {
        let wire = self
            .client
            .list_message_files(conversation_id, message_id)
            .await?;
        let entries: Vec<FileEntry> = wire.iter().map(FileEntry::from_shared).collect();
        self.store
            .upsert_files(&self.profile, Some(conversation_id), &entries)?;
        Ok(entries)
    }

    /// List one folder's children (files **and** subfolders) by drive + item
    /// id, cache them, and return them in wire order.
    pub async fn list_folder(&self, drive_id: &str, item_id: &str) -> Result<Vec<FileEntry>> {
        let wire = self
            .client
            .list_folder_children(drive_id, item_id, LIST_LIMIT)
            .await?;
        let entries: Vec<FileEntry> = wire.iter().map(FileEntry::from_shared).collect();
        self.store.upsert_files(&self.profile, None, &entries)?;
        Ok(entries)
    }

    /// The signed-in user's recently accessed files (Graph `/me/drive/recent`),
    /// cached and returned.
    pub async fn recents(&self) -> Result<Vec<FileEntry>> {
        let wire = self.client.list_drive_recents(LIST_LIMIT).await?;
        let entries: Vec<FileEntry> = wire.iter().map(FileEntry::from_shared).collect();
        self.store.upsert_files(&self.profile, None, &entries)?;
        Ok(entries)
    }

    // ----- transfer -------------------------------------------------------

    /// Download `file`'s content to `dest_path`; returns bytes written.
    ///
    /// `on_progress(bytes_done, bytes_total)` fires `(0, size)` before the
    /// transfer and `(written, size)` after. See the module docs: the vendored
    /// downloader buffers the whole body, so these are the only honest points
    /// and `size` (the cached driveItem size) may be `0` when unknown.
    ///
    /// A file with no `drive_id` (an unresolved chat attachment) cannot be
    /// downloaded and is rejected before any network.
    pub async fn download<F>(
        &self,
        file: &FileEntry,
        dest_path: &str,
        on_progress: F,
    ) -> Result<u64>
    where
        F: Fn(u64, u64) + Sync,
    {
        let drive_id = file.drive_id.as_deref().ok_or_else(|| {
            CoreError::NotReady(format!(
                "file {} has no drive id (unresolved attachment); open it in the browser instead",
                file.name
            ))
        })?;
        on_progress(0, file.size);
        let written = self
            .client
            .download_file(drive_id, &file.id, dest_path)
            .await?;
        on_progress(written, file.size);
        Ok(written)
    }

    /// Upload `local_path` to `chat_id`, posting it as a reference-attachment
    /// message, and return the uploaded file (cached under the chat).
    ///
    /// `on_progress(bytes_sent, bytes_total)` fires once per uploaded fragment
    /// (a simple PUT reports once at completion). See the module docs for the
    /// ≤4 MiB simple-PUT vs. resumable-session split and the reference post.
    pub async fn upload<F>(
        &self,
        chat_id: &str,
        local_path: &str,
        on_progress: F,
    ) -> Result<FileEntry>
    where
        F: Fn(u64, u64) + Sync,
    {
        let progress = |done: u64, total: u64| on_progress(done, total);
        let wire = self
            .client
            .upload_file(chat_id, local_path, Some(&progress))
            .await?;
        let entry = FileEntry::from_shared(&wire);
        self.store
            .upsert_files(&self.profile, Some(chat_id), std::slice::from_ref(&entry))?;
        Ok(entry)
    }

    // ----- sharing links & versions ---------------------------------------

    /// Create (or fetch the existing) view-only sharing link for one driveItem
    /// and cache the URL on the file row when it is already cached.
    pub async fn create_link(
        &self,
        drive_id: &str,
        item_id: &str,
        scope: &str,
    ) -> Result<SharedLink> {
        let link = self.client.create_link(drive_id, item_id, scope).await?;
        if let Some(mut file) = self.store.file(&self.profile, drive_id, item_id)? {
            file.share_url = Some(link.url.clone());
            self.store
                .upsert_files(&self.profile, None, std::slice::from_ref(&file))?;
        }
        Ok(link)
    }

    /// Version history for one driveItem, newest first.
    pub async fn versions(&self, drive_id: &str, item_id: &str) -> Result<Vec<FileVersion>> {
        let wire = self.client.list_file_versions(drive_id, item_id).await?;
        Ok(wire.iter().map(FileVersion::from_wire).collect())
    }

    /// Restore one version as the current item.
    pub async fn restore_version(
        &self,
        drive_id: &str,
        item_id: &str,
        version_id: &str,
    ) -> Result<()> {
        self.client
            .restore_file_version(drive_id, item_id, version_id)
            .await
    }

    // ----- manage ---------------------------------------------------------

    /// Rename one driveItem, updating the cached row from the write's answer.
    pub async fn rename(&self, drive_id: &str, item_id: &str, new_name: &str) -> Result<FileEntry> {
        let wire = self.client.rename_file(drive_id, item_id, new_name).await?;
        let entry = FileEntry::from_shared(&wire);
        self.store
            .upsert_files(&self.profile, None, std::slice::from_ref(&entry))?;
        Ok(entry)
    }

    /// Move one driveItem to another folder in the same drive, updating the
    /// cached row.
    pub async fn move_file(
        &self,
        drive_id: &str,
        item_id: &str,
        dest_folder_id: &str,
    ) -> Result<FileEntry> {
        let wire = self
            .client
            .move_file(drive_id, item_id, dest_folder_id)
            .await?;
        let entry = FileEntry::from_shared(&wire);
        self.store
            .upsert_files(&self.profile, None, std::slice::from_ref(&entry))?;
        Ok(entry)
    }

    /// Copy one driveItem to another folder in the same drive. Graph copies
    /// asynchronously; returns the monitor URL (`""` when the server omitted
    /// it).
    pub async fn copy_file(
        &self,
        drive_id: &str,
        item_id: &str,
        dest_folder_id: &str,
        new_name: Option<&str>,
    ) -> Result<String> {
        self.client
            .copy_file(drive_id, item_id, dest_folder_id, new_name)
            .await
    }

    /// Delete one driveItem and drop the cached row.
    pub async fn delete(&self, drive_id: &str, item_id: &str) -> Result<()> {
        self.client.delete_file(drive_id, item_id).await?;
        self.store.delete_file(&self.profile, drive_id, item_id)?;
        Ok(())
    }
}

/// Redacting `Debug`: no file names or URLs.
impl std::fmt::Debug for Files {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Files")
            .field("profile", &self.profile)
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{StubResponse, http_stub, http_stub_with};
    use crate::auth::tokens::{StoredToken, TokenSet};
    use std::sync::Mutex;

    fn tokens_for(base: &str) -> TokenSet {
        let future = crate::auth::tokens::now_unix() + 3600;
        let mk = |name: &str| StoredToken::with_expiry(name, future);
        TokenSet {
            refresh_token: "refresh-secret".into(),
            tenant: "contoso.onmicrosoft.com".into(),
            // The chat-service leg and the Graph leg both route at the stub, so
            // a test can assert which surface a call took.
            region_gtms: serde_json::json!({
                "graphBase": base,
                "chatService": base,
                "csaBase": base,
            }),
            personal: false,
            spaces: mk("spaces-token"),
            skype: mk("skype-token"),
            graph: mk("graph-token"),
            ic3: mk("ic3-token"),
            recorder: mk("recorder-token"),
        }
    }

    fn test_files(base: &str) -> (Arc<Store>, Files) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let client = Arc::new(ApiClient::from_tokens(&tokens_for(base)));
        let files = Files::new("default", Arc::clone(&store), client);
        (store, files)
    }

    fn requests(seen: &std::sync::Arc<Mutex<Vec<String>>>) -> Vec<String> {
        seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn file_entry(id: &str, drive: &str, name: &str) -> FileEntry {
        FileEntry {
            id: id.into(),
            drive_id: Some(drive.into()),
            name: name.into(),
            size: 11,
            mime: Some("text/plain".into()),
            web_url: Some(format!("https://sp/{id}")),
            download_url: Some(format!("https://dl/{id}")),
            is_folder: false,
            created: Some("2026-01-01T00:00:00Z".into()),
            modified: Some("2026-01-02T00:00:00Z".into()),
            sender: Some("Alice".into()),
            share_url: None,
            attachment_id: Some("att-1".into()),
        }
    }

    // ----- vendored pure helpers (exit criterion) -------------------------

    #[test]
    fn upload_chunk_ranges_boundaries() {
        // Empty file: no ranges (a simple PUT handles it).
        assert!(ost::api::upload_chunk_ranges(0, 5).is_empty());
        // Exactly one chunk.
        assert_eq!(ost::api::upload_chunk_ranges(5, 5), vec![(0, 4)]);
        // One chunk + 1 byte: a short tail.
        assert_eq!(ost::api::upload_chunk_ranges(6, 5), vec![(0, 4), (5, 5)]);
        // Several chunks + remainder, byte-exact.
        assert_eq!(
            ost::api::upload_chunk_ranges(12, 5),
            vec![(0, 4), (5, 9), (10, 11)]
        );
        // The chunk size is floored to 1 (never loops forever / panics).
        assert_eq!(
            ost::api::upload_chunk_ranges(3, 0),
            vec![(0, 0), (1, 1), (2, 2)]
        );
    }

    #[test]
    fn content_range_value_is_inclusive() {
        assert_eq!(ost::api::content_range_value(0, 4, 12), "bytes 0-4/12");
        assert_eq!(ost::api::content_range_value(10, 11, 12), "bytes 10-11/12");
    }

    #[test]
    fn upload_session_body_replaces_like_simple_put() {
        let body = ost::api::upload_session_body("a b.pdf");
        assert_eq!(body["item"]["@microsoft.graph.conflictBehavior"], "replace");
        assert_eq!(body["item"]["name"], "a b.pdf");
    }

    /// The vendored `files::encode_share_id` is **not** re-exported by
    /// `ost::api`, so this pins the exact Graph `shares/{u!<base64url-nopad>}`
    /// form it produces (the listing test below proves that form reaches the
    /// wire).
    #[test]
    fn encode_share_id_is_graph_url_safe_base64() {
        use base64::Engine as _;
        let url = "https://contoso.sharepoint.com/personal/a_b/Documents/file.docx";
        let encoded = format!(
            "u!{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(url)
        );
        assert_eq!(
            encoded,
            "u!aHR0cHM6Ly9jb250b3NvLnNoYXJlcG9pbnQuY29tL3BlcnNvbmFsL2FfYi9Eb2N1bWVudHMvZmlsZS5kb2N4"
        );
        // URL-safe alphabet, no padding.
        assert!(!encoded.contains('='));
        assert!(!encoded.contains('+') && !encoded.contains('/'));
        // A short known answer.
        assert_eq!(
            format!(
                "u!{}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("a")
            ),
            "u!YQ"
        );
    }

    // ----- listing --------------------------------------------------------

    #[tokio::test]
    async fn list_chat_files_reads_chat_history_and_resolves_share_ids() {
        let chat = "19:chat1@thread.v2";
        let object_url = "https://contoso.sharepoint.com/personal/a_b/Documents/file.docx";
        let share_b64 =
            "aHR0cHM6Ly9jb250b3NvLnNoYXJlcG9pbnQuY29tL3BlcnNvbmFsL2FfYi9Eb2N1bWVudHMvZmlsZS5kb2N4";
        let page = serde_json::json!({
            "messages": [{
                "id": "m1",
                "imdisplayname": "Alice",
                "originalarrivaltime": "2026-09-28T09:00:00Z",
                "properties": {
                    "files": serde_json::json!([{
                        "@type": "http://schema.skype.com/File",
                        "id": "att-1",
                        "objectUrl": object_url,
                        "fileName": "file.docx",
                        "fileType": "docx",
                        "state": "active"
                    }])
                    .to_string()
                }
            }],
            "_metadata": { "backwardLink": serde_json::Value::Null }
        })
        .to_string();
        let item = serde_json::json!({
            "id": "item-1",
            "name": "file.docx",
            "size": 10,
            "file": { "mimeType": "application/vnd.openxmlformats-officedocument.wordprocessingml.document" },
            "webUrl": "https://w/file.docx",
            "@microsoft.graph.downloadUrl": "https://d/file.docx",
            "parentReference": { "driveId": "D1" }
        })
        .to_string();
        let (base, seen, task) =
            http_stub(vec![StubResponse::ok(page), StubResponse::ok(item)]).await;
        let (store, files) = test_files(&base);

        let got = files.list_chat_files(chat).await.expect("list chat files");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "item-1");
        assert_eq!(got[0].drive_id.as_deref(), Some("D1"));
        assert_eq!(got[0].sender.as_deref(), Some("Alice"));
        assert_eq!(got[0].attachment_id.as_deref(), Some("att-1"));
        // Cached under the chat (offline read).
        assert_eq!(files.cached_chat_files(chat).unwrap().len(), 1);
        assert_eq!(store.files_for_chat("default", chat).unwrap().len(), 1);

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert!(
            reqs[0].starts_with(&format!(
                "GET /v1/users/ME/conversations/{chat}/messages?pageSize=200 "
            )),
            "unexpected request line: {}",
            reqs[0]
        );
        assert!(
            reqs[1].starts_with(&format!("GET /shares/u!{share_b64}/driveItem ")),
            "the share id must be Graph u!<base64url-nopad>: {}",
            reqs[1]
        );
        assert!(
            reqs[1].contains("authorization: Bearer graph-token"),
            "the resolve leg is Graph: {}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn list_folder_parses_files_and_folders_and_caches_them() {
        let body = serde_json::json!({"value": [
            {"id": "f1", "name": "a.pdf", "size": 12,
             "file": {"mimeType": "application/pdf"},
             "webUrl": "https://w/a", "@microsoft.graph.downloadUrl": "https://d/a",
             "parentReference": {"driveId": "D1"}},
            {"id": "dir1", "name": "sub", "folder": {}, "parentReference": {"driveId": "D1"}}
        ]})
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let (store, files) = test_files(&base);

        let got = files.list_folder("D1", "root").await.expect("list folder");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "f1");
        assert!(!got[0].is_folder);
        assert_eq!(got[0].mime.as_deref(), Some("application/pdf"));
        assert_eq!(got[1].id, "dir1");
        assert!(got[1].is_folder);
        // Cached and drive-scoped.
        assert_eq!(
            store.file("default", "D1", "f1").unwrap().unwrap().name,
            "a.pdf"
        );

        task.await.expect("stub drains");
        assert!(
            requests(&seen)[0].starts_with("GET /drives/D1/items/root/children?$top=100 "),
            "{}",
            requests(&seen)[0]
        );
    }

    #[tokio::test]
    async fn recents_parses_a_stub_response() {
        let body = serde_json::json!({"value": [
            {"id": "r1", "name": "qna.csv", "size": 99,
             "file": {"mimeType": "text/csv"},
             "lastModifiedDateTime": "2026-09-25T10:00:00Z",
             "parentReference": {"driveId": "D9"}}
        ]})
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let (_store, files) = test_files(&base);

        let got = files.recents().await.expect("recents");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "r1");
        assert_eq!(got[0].drive_id.as_deref(), Some("D9"));
        assert_eq!(got[0].modified.as_deref(), Some("2026-09-25T10:00:00Z"));

        task.await.expect("stub drains");
        assert!(
            requests(&seen)[0].starts_with("GET /me/drive/recent?$top=100 "),
            "{}",
            requests(&seen)[0]
        );
    }

    // ----- download -------------------------------------------------------

    #[tokio::test]
    async fn download_writes_the_file_and_reports_progress() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok("hello bytes")]).await;
        let (_store, files) = test_files(&base);

        let dest = std::env::temp_dir().join(format!("squads-dl-{}", uuid::Uuid::new_v4()));
        let dest_str = dest.to_string_lossy().to_string();
        let file = file_entry("I1", "D1", "greeting.txt");

        let progress = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
        let sink = Arc::clone(&progress);
        let written = files
            .download(&file, &dest_str, move |done, total| {
                sink.lock().unwrap().push((done, total));
            })
            .await
            .expect("download");

        assert_eq!(written, 11);
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello bytes");
        std::fs::remove_file(&dest).ok();
        assert_eq!(*progress.lock().unwrap(), vec![(0, 11), (11, 11)]);

        task.await.expect("stub drains");
        assert!(
            requests(&seen)[0].starts_with("GET /drives/D1/items/I1/content "),
            "{}",
            requests(&seen)[0]
        );
    }

    #[tokio::test]
    async fn download_without_a_drive_id_errors_before_network() {
        // Port 9 is unreachable; the guard must fire first.
        let (_store, files) = test_files("http://127.0.0.1:9");
        let mut file = file_entry("I1", "D1", "broken.txt");
        file.drive_id = None;
        let err = files
            .download(&file, "/tmp/never-written", |_, _| {})
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("no drive id"), "err = {err}");
        assert_eq!(files.client().request_count(), 0);
    }

    // ----- upload ---------------------------------------------------------

    #[tokio::test]
    async fn upload_small_file_does_a_single_put_to_content() {
        let chat = "19:chat1@thread.v2";
        let file_body = serde_json::json!({
            "id": "item-1",
            "name": "small.txt",
            "size": 11,
            "eTag": "\"c:{0F1E2D3C-AAAA-BBBB-CCCC-000000000001},2\"",
            "webUrl": "https://contoso-my.sharepoint.com/personal/a_contoso_com/Documents/Microsoft Teams Chat Files/small.txt"
        })
        .to_string();
        let (base, seen, task) =
            http_stub(vec![StubResponse::ok(file_body), StubResponse::status(201)]).await;
        let (store, files) = test_files(&base);

        // Well under the 4 MiB simple-PUT threshold. The upload naming uses the
        // local file's basename, so write it under its intended name.
        let dir =
            std::env::temp_dir().join(format!("squads-upload-small-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("small.txt");
        std::fs::write(&path, b"hello world").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let progress = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
        let sink = Arc::clone(&progress);
        let entry = files
            .upload(chat, &path_str, move |done, total| {
                sink.lock().unwrap().push((done, total));
            })
            .await
            .expect("upload");
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(entry.id, "item-1");
        assert_eq!(
            entry.attachment_id.as_deref(),
            Some("0F1E2D3C-AAAA-BBBB-CCCC-000000000001")
        );
        assert_eq!(store.files_for_chat("default", chat).unwrap().len(), 1);
        assert_eq!(*progress.lock().unwrap(), vec![(11, 11)]);

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert_eq!(reqs.len(), 2, "one PUT + one file-message POST");
        assert!(
            reqs[0].starts_with(
                "PUT /me/drive/root:/Microsoft%20Teams%20Chat%20Files/small.txt:/content "
            ),
            "unexpected request line: {}",
            reqs[0]
        );
        assert!(
            reqs[0].contains("hello world"),
            "the file bytes must be the PUT body: {}",
            reqs[0]
        );
        assert!(
            reqs[1].starts_with(&format!("POST /v1/users/ME/conversations/{chat}/messages ")),
            "{}",
            reqs[1]
        );
        assert!(
            reqs[1].contains("http://schema.skype.com/File") && reqs[1].contains("objectUrl"),
            "the post is a file-reference message: {}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn upload_large_file_creates_a_session_and_puts_chunks_with_content_range() {
        let chat = "19:chat1@thread.v2";
        // Just over the vendored 5 MiB fragment size -> two fragments.
        let size: usize = 5 * 1024 * 1024 + 1;
        let (base, seen, task) = http_stub_with(|base| {
            let session = serde_json::json!({"uploadUrl": format!("{base}/upload/s1")}).to_string();
            let file_body = serde_json::json!({
                "id": "item-big",
                "name": "big.bin",
                "size": size,
                "eTag": "\"c:{0F1E2D3C-AAAA-BBBB-CCCC-000000000002},2\"",
                "webUrl": "https://contoso-my.sharepoint.com/personal/a_contoso_com/Documents/Microsoft Teams Chat Files/big.bin"
            })
            .to_string();
            vec![
                StubResponse::ok(session),   // createUploadSession POST
                StubResponse::status(202),   // first fragment -> 202
                StubResponse::ok(file_body), // final fragment -> 200 driveItem
                StubResponse::status(201),   // file-message POST
            ]
        })
        .await;
        let (store, files) = test_files(&base);

        let dir = std::env::temp_dir().join(format!("squads-upload-big-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.bin");
        std::fs::write(&path, vec![7u8; size]).unwrap();
        let path_str = path.to_string_lossy().to_string();

        let progress = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
        let sink = Arc::clone(&progress);
        let entry = files
            .upload(chat, &path_str, move |done, total| {
                sink.lock().unwrap().push((done, total));
            })
            .await
            .expect("upload");
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(entry.id, "item-big");
        assert_eq!(store.files_for_chat("default", chat).unwrap().len(), 1);
        assert_eq!(
            *progress.lock().unwrap(),
            vec![(5 * 1024 * 1024, size as u64), (size as u64, size as u64)],
            "one report per fragment"
        );

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert_eq!(reqs.len(), 4, "session + 2 fragment PUTs + post: {reqs:#?}");
        assert!(
            reqs[0].starts_with(
                "POST /me/drive/root:/Microsoft%20Teams%20Chat%20Files/big.bin:/createUploadSession "
            ),
            "unexpected request line: {}",
            reqs[0]
        );
        assert!(
            reqs[0].contains("\"@microsoft.graph.conflictBehavior\":\"replace\""),
            "{}",
            reqs[0]
        );
        assert!(reqs[1].starts_with("PUT /upload/s1 "), "{}", reqs[1]);
        assert!(
            reqs[1]
                .to_lowercase()
                .contains("content-range: bytes 0-5242879/5242881"),
            "{}",
            reqs[1]
        );
        assert!(reqs[2].starts_with("PUT /upload/s1 "), "{}", reqs[2]);
        assert!(
            reqs[2]
                .to_lowercase()
                .contains("content-range: bytes 5242880-5242880/5242881"),
            "{}",
            reqs[2]
        );
        assert!(
            reqs[3].starts_with(&format!("POST /v1/users/ME/conversations/{chat}/messages ")),
            "{}",
            reqs[3]
        );
    }

    // ----- sharing links, versions & manage -------------------------------

    #[tokio::test]
    async fn create_link_posts_create_link_and_caches_the_share_url() {
        let body = serde_json::json!({
            "id": "perm-1",
            "link": {
                "type": "view",
                "scope": "organization",
                "webUrl": "https://contoso.sharepoint.com/:w:/x/ABC"
            }
        })
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let (store, files) = test_files(&base);
        store
            .upsert_files(
                "default",
                Some("c1"),
                &[file_entry("I1", "D1", "Plan.docx")],
            )
            .unwrap();

        let link = files
            .create_link("D1", "I1", "organization")
            .await
            .expect("create link");
        assert_eq!(link.url, "https://contoso.sharepoint.com/:w:/x/ABC");
        assert_eq!(link.scope.as_deref(), Some("organization"));
        // Cached onto the file row (and the chat association survives).
        let row = store.file("default", "D1", "I1").unwrap().unwrap();
        assert_eq!(
            row.share_url.as_deref(),
            Some("https://contoso.sharepoint.com/:w:/x/ABC")
        );
        assert_eq!(store.files_for_chat("default", "c1").unwrap().len(), 1);

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(
            req.starts_with("POST /drives/D1/items/I1/createLink "),
            "unexpected request line: {req}"
        );
        assert!(req.contains("\"type\":\"view\""), "{req}");
        assert!(req.contains("\"scope\":\"organization\""), "{req}");
    }

    #[tokio::test]
    async fn versions_parse_newest_first_and_map_the_editor() {
        let body = serde_json::json!({"value": [
            {"id": "3.0", "size": 48211,
             "lastModifiedDateTime": "2026-09-20T10:00:00Z",
             "lastModifiedBy": {"user": {"displayName": "Megan Harper"}}},
            {"id": "2.0"}
        ]})
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let (_store, files) = test_files(&base);

        let versions = files.versions("D1", "I1").await.expect("versions");
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].id, "3.0");
        assert_eq!(versions[0].size, 48211);
        assert_eq!(versions[0].modified_by.as_deref(), Some("Megan Harper"));
        assert_eq!(versions[1].size, 0, "sparse version defaults size 0");

        task.await.expect("stub drains");
        assert!(
            requests(&seen)[0].starts_with("GET /drives/D1/items/I1/versions "),
            "{}",
            requests(&seen)[0]
        );
    }

    #[tokio::test]
    async fn rename_and_delete_touch_the_drive_item_and_the_cache() {
        let renamed = serde_json::json!({
            "id": "I1",
            "name": "renamed.docx",
            "parentReference": {"driveId": "D1"}
        })
        .to_string();
        let (base, seen, task) =
            http_stub(vec![StubResponse::ok(renamed), StubResponse::status(204)]).await;
        let (store, files) = test_files(&base);
        store
            .upsert_files("default", Some("c1"), &[file_entry("I1", "D1", "old.docx")])
            .unwrap();

        let entry = files
            .rename("D1", "I1", "renamed.docx")
            .await
            .expect("rename");
        assert_eq!(entry.name, "renamed.docx");
        assert_eq!(
            store.file("default", "D1", "I1").unwrap().unwrap().name,
            "renamed.docx"
        );
        // The chat association survives the rename.
        assert_eq!(store.files_for_chat("default", "c1").unwrap().len(), 1);

        files.delete("D1", "I1").await.expect("delete");
        assert!(store.file("default", "D1", "I1").unwrap().is_none());

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert!(
            reqs[0].starts_with("PATCH /drives/D1/items/I1 "),
            "{}",
            reqs[0]
        );
        assert!(reqs[0].contains("\"name\":\"renamed.docx\""), "{}", reqs[0]);
        assert!(
            reqs[1].starts_with("DELETE /drives/D1/items/I1 "),
            "{}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn copy_returns_the_async_monitor_location() {
        let (base, seen, task) = http_stub(vec![StubResponse {
            status: 202,
            location: Some("https://graph.microsoft.com/monitor/op-1".into()),
            body: String::new(),
            arrive: None,
            release: None,
        }])
        .await;
        let (_store, files) = test_files(&base);

        let monitor = files
            .copy_file("D1", "I1", "F9", Some("copy.docx"))
            .await
            .expect("copy");
        assert_eq!(monitor, "https://graph.microsoft.com/monitor/op-1");

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(
            req.starts_with("POST /drives/D1/items/I1/copy "),
            "unexpected request line: {req}"
        );
        assert!(req.contains("\"driveId\":\"D1\""), "{req}");
        assert!(req.contains("\"id\":\"F9\""), "{req}");
        assert!(req.contains("\"name\":\"copy.docx\""), "{req}");
    }

    // ----- invariants -----------------------------------------------------

    #[test]
    fn files_is_send_and_sync_and_debug_is_redacting() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Files>();
        let (_store, files) = test_files("http://127.0.0.1:9");
        let dbg = format!("{files:?}");
        assert!(dbg.contains("Files"));
        assert!(dbg.contains("default"));
    }

    #[test]
    fn cached_chat_files_never_touch_the_network() {
        // Port 9 is unreachable; the cached read must not connect.
        let (store, files) = test_files("http://127.0.0.1:9");
        store
            .upsert_files("default", Some("c1"), &[file_entry("I1", "D1", "a.txt")])
            .unwrap();
        assert_eq!(files.cached_chat_files("c1").unwrap().len(), 1);
        assert_eq!(files.profile(), "default");
        assert_eq!(files.client().request_count(), 0);
    }

    #[test]
    fn file_entry_round_trips_through_json() {
        // The store persists plain columns, but the model is serde-friendly for
        // the UI event/command bridge.
        let file = file_entry("I1", "D1", "a.txt");
        let back: FileEntry = serde_json::from_str(&serde_json::to_string(&file).unwrap()).unwrap();
        assert_eq!(back, file);
    }
}
