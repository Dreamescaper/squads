//! Offline-first recordings & transcripts service (backlog item **B5**).
//!
//! This is the layer the conversation info pane drives. Like
//! [`Files`](crate::files) and [`Teams`](crate::teams), it ties the shared
//! [`Store`] (instant, offline reads) to the shared [`ApiClient`] (typed Graph
//! calls), and every method is testable offline against a local TCP stub.
//!
//! ```text
//!                UI (info pane / channel view)
//!                 │  recordings_for_chat / recordings_for_channel /
//!                 │  search_recordings / transcripts… / download
//!                 ▼
//!             Recordings ──▶ Arc<Store>      (M3: cached driveItems)
//!                 ├──────▶ Arc<ApiClient>    (M2: typed Graph calls)
//!                 └──────▶ Files             (M7: the download path)
//! ```
//!
//! ## Everything is Graph — and the vendored lane is **global**
//!
//! A meeting recording/transcript is a plain Graph **driveItem**: a
//! non-channel meeting's lives in the signed-in user's OneDrive `Recordings`
//! folder and a channel meeting's in the channel's SharePoint
//! `filesFolder/Recordings`. The vendored `list_recordings_data` /
//! `list_transcripts_data` are **global**: one call fans out over OneDrive
//! **and** every channel drive (a bounded `list_teams_data` scan plus
//! per-channel `filesFolder` lookups) and returns one merged, newest-first list.
//! There is **no** per-chat/per-channel variant and no paging cursor, and the
//! wire carries no chat/meeting association — only the **source** (OneDrive vs.
//! a channel, named by display name).
//!
//! So this service resolves a conversation to a *slice* of that one lane:
//!
//! * [`Recordings::recordings_for_chat`] keeps the **OneDrive** rows (a
//!   chat/meeting recording lives in the organizer's OneDrive) and caches them
//!   under the chat id.
//! * [`Recordings::recordings_for_channel`] keeps the **channel** rows, matched
//!   to the requested channel by its cached display name (the wire names no id),
//!   and caches them under the channel id.
//! * [`Recordings::search_recordings`] / [`Recordings::search_transcripts`] are
//!   the global drive-search (all sources).
//!
//! ## Download reuses the M7 path
//!
//! There is no separate transfer here: a recording/transcript is projected onto
//! a [`FileEntry`](crate::model::FileEntry) and handed to
//! [`Files::download`], so the buffered-transfer behaviour and the coarse
//! progress reporting are exactly the M7 ones (ADR-020). A transcript has no
//! pre-authenticated download URL on the wire, so its bytes always ride
//! `/drives/{d}/items/{i}/content`.

use std::sync::Arc;

use crate::api::ApiClient;
use crate::error::Result;
use crate::files::Files;
use crate::model::{Recording, RecordingSource, Transcript};
use crate::store::Store;

/// How many recordings/transcripts a listing requests (the vendored recordings
/// cap; the transcripts lane clamps to its own `1..=100` window).
pub const LIST_LIMIT: usize = 50;

/// The offline-first recordings & transcripts service (see the module docs).
pub struct Recordings {
    profile: String,
    store: Arc<Store>,
    client: Arc<ApiClient>,
    /// The M7 files service, reused for download (no separate transfer path).
    files: Files,
}

impl Recordings {
    /// Build the service over a shared store and protocol client.
    pub fn new(profile: impl Into<String>, store: Arc<Store>, client: Arc<ApiClient>) -> Self {
        let profile = profile.into();
        let files = Files::new(profile.clone(), Arc::clone(&store), Arc::clone(&client));
        Self {
            profile,
            store,
            client,
            files,
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

    // ----- local reads ----------------------------------------------------

    /// The recordings cached under one conversation (a chat or channel id).
    /// Purely local: never touches the network (the info pane's instant paint).
    pub fn cached_recordings(&self, scope: &str) -> Result<Vec<Recording>> {
        self.store.recordings_for_scope(&self.profile, scope)
    }

    /// The transcripts cached under one conversation. Purely local.
    pub fn cached_transcripts(&self, scope: &str) -> Result<Vec<Transcript>> {
        self.store.transcripts_for_scope(&self.profile, scope)
    }

    // ----- recordings -----------------------------------------------------

    /// Fetch the recordings relevant to a **chat/meeting** (the OneDrive
    /// `Recordings` set; the vendored lane is global and names no chat, see the
    /// module docs), cache them under `chat_id` and return them newest-first.
    pub async fn recordings_for_chat(&self, chat_id: &str, limit: usize) -> Result<Vec<Recording>> {
        let all = self.fetch_recordings(limit).await?;
        let rows: Vec<Recording> = all
            .into_iter()
            .filter(|recording| !recording.source.is_channel())
            .collect();
        self.store
            .upsert_recordings(&self.profile, Some(chat_id), &rows)?;
        self.cached_recordings(chat_id)
    }

    /// Fetch the recordings relevant to one **channel** (the channel's
    /// SharePoint `Recordings` set, matched to the requested channel by its
    /// cached display name), cache them under `channel_id` and return them
    /// newest-first.
    ///
    /// When the channel is not cached the name cannot be resolved, so **every**
    /// channel recording is returned (the vendored lane names no id); a caller
    /// that browses a channel first gets the precise slice.
    pub async fn recordings_for_channel(
        &self,
        team_id: &str,
        channel_id: &str,
        limit: usize,
    ) -> Result<Vec<Recording>> {
        let all = self.fetch_recordings(limit).await?;
        let (team, channel) = self.channel_names(team_id, channel_id);
        let rows: Vec<Recording> = all
            .into_iter()
            .filter(|recording| {
                keep_channel_source(&recording.source, team.as_deref(), channel.as_deref())
            })
            .collect();
        self.store
            .upsert_recordings(&self.profile, Some(channel_id), &rows)?;
        self.cached_recordings(channel_id)
    }

    /// Search every recording by name (the vendored global drive-search: all
    /// sources), cache the hits chatlessly and return them in wire order.
    pub async fn search_recordings(&self, query: &str, limit: usize) -> Result<Vec<Recording>> {
        let wire = self.client.search_recordings(query, limit).await?;
        let rows: Vec<Recording> = wire.iter().map(Recording::from).collect();
        self.store.upsert_recordings(&self.profile, None, &rows)?;
        Ok(rows)
    }

    // ----- transcripts ----------------------------------------------------

    /// Fetch the transcripts relevant to a **chat/meeting** (the OneDrive
    /// `Recordings` set), cache them under `chat_id` and return them
    /// newest-first.
    pub async fn transcripts_for_chat(
        &self,
        chat_id: &str,
        limit: usize,
    ) -> Result<Vec<Transcript>> {
        let all = self.fetch_transcripts(limit).await?;
        let rows: Vec<Transcript> = all
            .into_iter()
            .filter(|transcript| !transcript.source.is_channel())
            .collect();
        self.store
            .upsert_transcripts(&self.profile, Some(chat_id), &rows)?;
        self.cached_transcripts(chat_id)
    }

    /// Fetch the transcripts relevant to one **channel**, matched by the cached
    /// display name (see [`Recordings::recordings_for_channel`]), cache them
    /// under `channel_id` and return them newest-first.
    pub async fn transcripts_for_channel(
        &self,
        team_id: &str,
        channel_id: &str,
        limit: usize,
    ) -> Result<Vec<Transcript>> {
        let all = self.fetch_transcripts(limit).await?;
        let (team, channel) = self.channel_names(team_id, channel_id);
        let rows: Vec<Transcript> = all
            .into_iter()
            .filter(|transcript| {
                keep_channel_source(&transcript.source, team.as_deref(), channel.as_deref())
            })
            .collect();
        self.store
            .upsert_transcripts(&self.profile, Some(channel_id), &rows)?;
        self.cached_transcripts(channel_id)
    }

    /// Search every transcript by name (the vendored global drive-search),
    /// cache the hits chatlessly and return them in wire order.
    pub async fn search_transcripts(&self, query: &str, limit: usize) -> Result<Vec<Transcript>> {
        let wire = self.client.search_transcripts(query, limit).await?;
        let rows: Vec<Transcript> = wire.iter().map(Transcript::from).collect();
        self.store.upsert_transcripts(&self.profile, None, &rows)?;
        Ok(rows)
    }

    // ----- download (the M7 path) -----------------------------------------

    /// Download a recording's content to `dest_path` via the M7 files path;
    /// returns bytes written.
    ///
    /// `on_progress(bytes_done, bytes_total)` fires `(0, size)` before the
    /// transfer and `(written, size)` after — the M7 coarse reporting (the
    /// vendored downloader buffers the whole body; see [`Files::download`]).
    /// A recording with no `drive_id` cannot be downloaded and is rejected
    /// before any network.
    pub async fn download<F>(
        &self,
        recording: &Recording,
        dest_path: &str,
        on_progress: F,
    ) -> Result<u64>
    where
        F: Fn(u64, u64) + Sync,
    {
        self.files
            .download(&recording.as_file_entry(), dest_path, on_progress)
            .await
    }

    /// Download a transcript's content to `dest_path` via the M7 files path
    /// (the `.vtt`/`.docx` has no pre-authenticated URL, so this is the only
    /// transfer). Returns bytes written.
    pub async fn download_transcript<F>(
        &self,
        transcript: &Transcript,
        dest_path: &str,
        on_progress: F,
    ) -> Result<u64>
    where
        F: Fn(u64, u64) + Sync,
    {
        self.files
            .download(&transcript.as_file_entry(), dest_path, on_progress)
            .await
    }

    // ----- internals ------------------------------------------------------

    /// Fetch and map the global recordings lane.
    async fn fetch_recordings(&self, limit: usize) -> Result<Vec<Recording>> {
        let wire = self.client.list_recordings(limit).await?;
        Ok(wire.iter().map(Recording::from).collect())
    }

    /// Fetch and map the global transcripts lane.
    async fn fetch_transcripts(&self, limit: usize) -> Result<Vec<Transcript>> {
        let wire = self.client.list_transcripts(limit).await?;
        Ok(wire.iter().map(Transcript::from).collect())
    }

    /// Resolve a channel's team/channel **display names** from the cache (the
    /// wire names no id; see the module docs). Each is `None` when the
    /// team/channel is not cached.
    fn channel_names(&self, team_id: &str, channel_id: &str) -> (Option<String>, Option<String>) {
        let team = self
            .store
            .list_teams(&self.profile)
            .ok()
            .and_then(|teams| teams.into_iter().find(|team| team.id == team_id))
            .map(|team| team.name);
        let channel = self
            .store
            .channel(team_id, channel_id)
            .ok()
            .flatten()
            .map(|channel| channel.name);
        (team, channel)
    }
}

/// Whether a recording/transcript source belongs to the requested channel.
///
/// A channel row is kept when it is a channel source and matches every name
/// that could be resolved (team and/or channel); with no resolvable name, every
/// channel row is kept (the wire names no id — documented on
/// [`Recordings::recordings_for_channel`]).
fn keep_channel_source(
    source: &RecordingSource,
    team: Option<&str>,
    channel: Option<&str>,
) -> bool {
    if !source.is_channel() {
        return false;
    }
    if let Some(channel) = channel
        && source.channel_name() != Some(channel)
    {
        return false;
    }
    if let Some(team) = team
        && source.team_name() != Some(team)
    {
        return false;
    }
    true
}

/// Redacting `Debug`: no names or URLs.
impl std::fmt::Debug for Recordings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recordings")
            .field("profile", &self.profile)
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{StubResponse, http_stub};
    use crate::auth::tokens::{StoredToken, TokenSet};
    use crate::model::{Channel, Team};
    use std::sync::Mutex;

    fn tokens_for(base: &str) -> TokenSet {
        let future = crate::auth::tokens::now_unix() + 3600;
        let mk = |name: &str| StoredToken::with_expiry(name, future);
        TokenSet {
            refresh_token: "refresh-secret".into(),
            tenant: "contoso.onmicrosoft.com".into(),
            region_gtms: serde_json::json!({ "graphBase": base }),
            personal: false,
            spaces: mk("spaces-token"),
            skype: mk("skype-token"),
            graph: mk("graph-token"),
            ic3: mk("ic3-token"),
            recorder: mk("recorder-token"),
        }
    }

    fn test_recordings(base: &str) -> (Arc<Store>, Recordings) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let client = Arc::new(ApiClient::from_tokens(&tokens_for(base)));
        let svc = Recordings::new("default", Arc::clone(&store), client);
        (store, svc)
    }

    fn requests(seen: &std::sync::Arc<Mutex<Vec<String>>>) -> Vec<String> {
        seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn onedrive_recording_json() -> serde_json::Value {
        serde_json::json!({
            "id": "r-onedrive", "name": "Weekly Sync-20260924.mp4", "size": 48211,
            "webUrl": "https://x/r1",
            "@microsoft.graph.downloadUrl": "https://dl/r1",
            "createdDateTime": "2026-09-24T09:00:00Z",
            "lastModifiedDateTime": "2026-09-24T10:00:00Z",
            "file": {"mimeType": "video/mp4"},
            "video": {"duration": 3723000},
            "parentReference": {"driveId": "D-OD"}
        })
    }

    fn channel_recording_json() -> serde_json::Value {
        serde_json::json!({
            "id": "r-channel", "name": "Channel Standup-20260925.mp4", "size": 999,
            "webUrl": "https://x/rc",
            "createdDateTime": "2026-09-25T09:00:00Z",
            "lastModifiedDateTime": "2026-09-25T11:00:00Z",
            "file": {"mimeType": "video/mp4"},
            "parentReference": {"driveId": "D-CH"}
        })
    }

    /// The response list for the full global recordings lane: OneDrive, the
    /// teams scan, the channel's filesFolder, its children and the Recordings
    /// folder's children.
    fn recordings_list_responses() -> Vec<StubResponse> {
        vec![
            // 1. OneDrive Recordings children.
            StubResponse::ok(
                serde_json::json!({ "value": [onedrive_recording_json()] }).to_string(),
            ),
            // 2. joinedTeams.
            StubResponse::ok(
                serde_json::json!({ "value": [{"id": "team-1", "displayName": "Engineering"}] })
                    .to_string(),
            ),
            // 3. the team's channels.
            StubResponse::ok(
                serde_json::json!({ "value": [
                    {"id": "19:ch@thread.tacv2", "displayName": "general"}
                ]})
                .to_string(),
            ),
            // 4. the channel filesFolder (resolves the drive + folder).
            StubResponse::ok(
                serde_json::json!({
                    "id": "files-folder",
                    "parentReference": {"driveId": "D-CH"}
                })
                .to_string(),
            ),
            // 5. the filesFolder children (find the `Recordings` dir).
            StubResponse::ok(
                serde_json::json!({ "value": [
                    {"id": "rec-folder", "name": "Recordings", "folder": {}}
                ]})
                .to_string(),
            ),
            // 6. the Recordings folder children.
            StubResponse::ok(
                serde_json::json!({ "value": [channel_recording_json()] }).to_string(),
            ),
        ]
    }

    // ----- pure helpers (exit criterion) ----------------------------------

    #[test]
    fn pure_helpers_pin_the_paths_limits_and_predicates() {
        // Recordings paths/limits (the vendored re-exports).
        assert_eq!(
            crate::api::recordings_children_path(999),
            "/me/drive/root:/Recordings:/children?$top=50"
        );
        assert_eq!(
            crate::api::recordings_search_path(None, "q3 review", 999),
            "/me/drive/search(q='q3%20review')?$top=50"
        );
        assert_eq!(crate::api::recordings_clamp_limit(0), 1);
        assert_eq!(crate::api::RECORDINGS_MAX_LIMIT, 50);
        assert!(crate::api::is_recording_video("a.mp4", None));
        assert!(!crate::api::is_recording_video("notes.pdf", None));

        // Transcript paths/limits/predicates.
        assert_eq!(
            crate::api::transcripts_children_path(999),
            "/me/drive/root:/Recordings:/children?$top=100"
        );
        assert_eq!(
            crate::api::transcripts_search_path(Some("d9"), "tom's", 10),
            "/drives/d9/root/search(q='tom%27%27s')?$top=10"
        );
        assert_eq!(crate::api::transcripts_clamp_limit(999), 100);
        assert_eq!(crate::api::TRANSCRIPTS_MAX_LIMIT, 100);
        assert!(crate::api::is_transcript("a.vtt", None));
        assert!(crate::api::is_transcript("Weekly transcript.docx", None));
        assert!(!crate::api::is_transcript("notes.docx", None));
    }

    #[test]
    fn keep_channel_source_matches_resolved_names() {
        let channel = RecordingSource::Channel {
            team: "Engineering".into(),
            channel: "general".into(),
        };
        // No name resolvable: every channel row is kept, OneDrive is not.
        assert!(keep_channel_source(&channel, None, None));
        assert!(!keep_channel_source(&RecordingSource::OneDrive, None, None));
        // A resolved name must match.
        assert!(keep_channel_source(
            &channel,
            Some("Engineering"),
            Some("general")
        ));
        assert!(!keep_channel_source(
            &channel,
            Some("Engineering"),
            Some("random")
        ));
        assert!(!keep_channel_source(
            &channel,
            Some("Design"),
            Some("general")
        ));
        // A resolved team alone still filters.
        assert!(keep_channel_source(&channel, Some("Engineering"), None));
        assert!(!keep_channel_source(&channel, Some("Design"), None));
    }

    // ----- api wrappers against a stub ------------------------------------

    #[tokio::test]
    async fn list_recordings_hits_the_onedrive_and_channel_paths() {
        let (base, seen, task) = http_stub(recordings_list_responses()).await;
        let client = ApiClient::from_tokens(&tokens_for(&base));

        let rows = client.list_recordings(50).await.expect("list recordings");
        assert_eq!(rows.len(), 2);
        // Newest-first merge: the channel item (2026-09-25) sorts before the
        // OneDrive one (2026-09-24).
        assert_eq!(rows[0].id, "r-channel");
        assert_eq!(rows[1].id, "r-onedrive");
        assert_eq!(rows[1].duration_ms, Some(3723000));

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert_eq!(reqs.len(), 6, "OneDrive + teams scan + channel fan-out");
        assert!(
            reqs[0].starts_with("GET /me/drive/root:/Recordings:/children?$top=50 "),
            "unexpected request: {}",
            reqs[0]
        );
        assert!(
            reqs[0].contains("authorization: Bearer graph-token"),
            "{}",
            reqs[0]
        );
        assert!(reqs[1].starts_with("GET /me/joinedTeams "), "{}", reqs[1]);
        assert!(
            reqs[2].starts_with("GET /teams/team-1/channels "),
            "{}",
            reqs[2]
        );
        assert!(
            reqs[3].starts_with("GET /teams/team-1/channels/19:ch@thread.tacv2/filesFolder "),
            "unexpected request: {}",
            reqs[3]
        );
        assert!(
            reqs[4].starts_with("GET /drives/D-CH/items/files-folder/children?$top=50 "),
            "unexpected request: {}",
            reqs[4]
        );
        assert!(
            reqs[5].starts_with("GET /drives/D-CH/items/rec-folder/children?$top=50 "),
            "unexpected request: {}",
            reqs[5]
        );
    }

    #[tokio::test]
    async fn search_recordings_hits_the_drive_search() {
        let body = serde_json::json!({ "value": [onedrive_recording_json()] }).to_string();
        // OneDrive search answers, then the teams scan (no teams -> no channel
        // searches).
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(body),
            StubResponse::ok(serde_json::json!({ "value": [] }).to_string()),
        ])
        .await;
        let client = ApiClient::from_tokens(&tokens_for(&base));

        let rows = client
            .search_recordings("weekly", 50)
            .await
            .expect("search");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "r-onedrive");

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert!(
            reqs[0].starts_with("GET /me/drive/search(q='weekly')?$top=50 "),
            "unexpected request: {}",
            reqs[0]
        );
    }

    #[tokio::test]
    async fn list_transcripts_hits_the_onedrive_and_channel_paths() {
        let vtt = serde_json::json!({
            "id": "t-onedrive", "name": "Weekly Sync.vtt", "size": 4211,
            "file": {"mimeType": "text/vtt"},
            "lastModifiedDateTime": "2026-09-24T10:00:00Z",
            "parentReference": {"driveId": "D-OD"}
        });
        let channel_vtt = serde_json::json!({
            "id": "t-channel", "name": "Standup transcript.vtt", "size": 11,
            "file": {"mimeType": "text/vtt"},
            "lastModifiedDateTime": "2026-09-25T11:00:00Z",
            "parentReference": {"driveId": "D-CH"}
        });
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(serde_json::json!({ "value": [vtt] }).to_string()),
            StubResponse::ok(
                serde_json::json!({ "value": [{"id": "team-1", "displayName": "Engineering"}] })
                    .to_string(),
            ),
            StubResponse::ok(
                serde_json::json!({ "value": [
                    {"id": "19:ch@thread.tacv2", "displayName": "general"}
                ]})
                .to_string(),
            ),
            StubResponse::ok(
                serde_json::json!({"id": "files-folder", "parentReference": {"driveId": "D-CH"}})
                    .to_string(),
            ),
            StubResponse::ok(
                serde_json::json!({ "value": [
                    {"id": "rec-folder", "name": "Recordings", "folder": {}}
                ]})
                .to_string(),
            ),
            StubResponse::ok(serde_json::json!({ "value": [channel_vtt] }).to_string()),
        ])
        .await;
        let client = ApiClient::from_tokens(&tokens_for(&base));

        let rows = client
            .list_transcripts(100)
            .await
            .expect("list transcripts");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "t-channel", "newest first");
        assert_eq!(rows[1].id, "t-onedrive");

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert!(
            reqs[0].starts_with("GET /me/drive/root:/Recordings:/children?$top=100 "),
            "unexpected request: {}",
            reqs[0]
        );
        assert!(
            reqs[5].starts_with("GET /drives/D-CH/items/rec-folder/children?$top=100 "),
            "unexpected request: {}",
            reqs[5]
        );
    }

    #[tokio::test]
    async fn search_transcripts_rejects_a_blank_query_before_network() {
        let (base, seen, task) = http_stub(vec![]).await;
        let client = ApiClient::from_tokens(&tokens_for(&base));

        let err = client
            .search_transcripts("   ", 50)
            .await
            .err()
            .expect("blank query refused");
        assert!(format!("{err:#}").contains("empty query"), "{err:#}");

        task.await.expect("stub drains");
        assert!(requests(&seen).is_empty());
    }

    // ----- service: source split + caching --------------------------------

    #[tokio::test]
    async fn recordings_for_chat_keeps_onedrive_and_caches_under_the_chat() {
        let (base, seen, task) = http_stub(recordings_list_responses()).await;
        let (store, svc) = test_recordings(&base);

        let rows = svc
            .recordings_for_chat("19:chat@thread.v2", 50)
            .await
            .expect("for chat");
        assert_eq!(rows.len(), 1, "only the OneDrive recording is for a chat");
        assert_eq!(rows[0].id, "r-onedrive");
        assert_eq!(rows[0].source, RecordingSource::OneDrive);
        // Cached under the chat (offline read), and the channel row is not.
        assert_eq!(svc.cached_recordings("19:chat@thread.v2").unwrap().len(), 1);
        assert_eq!(
            store
                .recording("default", "r-onedrive")
                .unwrap()
                .unwrap()
                .id,
            "r-onedrive"
        );
        assert!(
            svc.cached_recordings("19:ch@thread.tacv2")
                .unwrap()
                .is_empty()
        );

        task.await.expect("stub drains");
        assert!(!requests(&seen).is_empty());
    }

    #[tokio::test]
    async fn recordings_for_channel_keeps_the_matching_channel_and_caches_it() {
        let (base, _seen, task) = http_stub(recordings_list_responses()).await;
        let (store, svc) = test_recordings(&base);
        // Cache the team + channel so the display names resolve.
        store
            .upsert_teams(
                "default",
                &[Team {
                    id: "team-1".into(),
                    name: "Engineering".into(),
                }],
            )
            .unwrap();
        store
            .upsert_channels(
                "default",
                &[Channel {
                    id: "19:ch@thread.tacv2".into(),
                    team_id: "team-1".into(),
                    name: "general".into(),
                    description: None,
                    membership_type: None,
                    web_url: None,
                    is_channel: true,
                }],
            )
            .unwrap();

        let rows = svc
            .recordings_for_channel("team-1", "19:ch@thread.tacv2", 50)
            .await
            .expect("for channel");
        assert_eq!(
            rows.len(),
            1,
            "only the channel recording is for the channel"
        );
        assert_eq!(rows[0].id, "r-channel");
        assert_eq!(rows[0].source.label(), "Engineering > #general");
        assert_eq!(
            svc.cached_recordings("19:ch@thread.tacv2").unwrap().len(),
            1
        );

        task.await.expect("stub drains");
    }

    #[tokio::test]
    async fn recordings_for_channel_with_an_unknown_name_keeps_every_channel_row() {
        let (base, _seen, task) = http_stub(recordings_list_responses()).await;
        let (_store, svc) = test_recordings(&base);
        // No team/channel cached: the name cannot be resolved, so every channel
        // row is kept (never the OneDrive one).
        let rows = svc
            .recordings_for_channel("team-x", "19:other@thread.tacv2", 50)
            .await
            .expect("for channel");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "r-channel");
        task.await.expect("stub drains");
    }

    #[tokio::test]
    async fn transcripts_for_chat_and_channel_split_by_source() {
        let vtt = serde_json::json!({
            "id": "t-onedrive", "name": "Weekly Sync.vtt", "size": 4211,
            "file": {"mimeType": "text/vtt"},
            "parentReference": {"driveId": "D-OD"}
        });
        let channel_vtt = serde_json::json!({
            "id": "t-channel", "name": "Standup transcript.vtt", "size": 11,
            "file": {"mimeType": "text/vtt"},
            "parentReference": {"driveId": "D-CH"}
        });
        let responses = || {
            vec![
                StubResponse::ok(serde_json::json!({ "value": [vtt] }).to_string()),
                StubResponse::ok(
                    serde_json::json!({ "value": [{"id": "team-1", "displayName": "Engineering"}] })
                        .to_string(),
                ),
                StubResponse::ok(
                    serde_json::json!({ "value": [
                        {"id": "19:ch@thread.tacv2", "displayName": "general"}
                    ]})
                    .to_string(),
                ),
                StubResponse::ok(
                    serde_json::json!({"id": "files-folder", "parentReference": {"driveId": "D-CH"}})
                        .to_string(),
                ),
                StubResponse::ok(
                    serde_json::json!({ "value": [
                        {"id": "rec-folder", "name": "Recordings", "folder": {}}
                    ]})
                    .to_string(),
                ),
                StubResponse::ok(serde_json::json!({ "value": [channel_vtt] }).to_string()),
            ]
        };
        let (base, _seen, task) =
            http_stub(responses().into_iter().chain(responses()).collect()).await;
        let (store, svc) = test_recordings(&base);
        store
            .upsert_teams(
                "default",
                &[Team {
                    id: "team-1".into(),
                    name: "Engineering".into(),
                }],
            )
            .unwrap();
        store
            .upsert_channels(
                "default",
                &[Channel {
                    id: "19:ch@thread.tacv2".into(),
                    team_id: "team-1".into(),
                    name: "general".into(),
                    description: None,
                    membership_type: None,
                    web_url: None,
                    is_channel: true,
                }],
            )
            .unwrap();

        let chat = svc
            .transcripts_for_chat("19:chat@thread.v2", 100)
            .await
            .unwrap();
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].id, "t-onedrive");
        let channel = svc
            .transcripts_for_channel("team-1", "19:ch@thread.tacv2", 100)
            .await
            .unwrap();
        assert_eq!(channel.len(), 1);
        assert_eq!(channel[0].id, "t-channel");

        task.await.expect("stub drains");
    }

    #[tokio::test]
    async fn search_recordings_caches_chatlessly() {
        let body = serde_json::json!({ "value": [onedrive_recording_json()] }).to_string();
        let (base, _seen, task) = http_stub(vec![
            StubResponse::ok(body),
            StubResponse::ok(serde_json::json!({ "value": [] }).to_string()),
        ])
        .await;
        let (store, svc) = test_recordings(&base);

        let rows = svc.search_recordings("weekly", 50).await.expect("search");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "r-onedrive");
        // Cached, but not under any conversation scope.
        assert_eq!(
            store
                .recording("default", "r-onedrive")
                .unwrap()
                .unwrap()
                .id,
            "r-onedrive"
        );
        assert!(
            svc.cached_recordings("19:chat@thread.v2")
                .unwrap()
                .is_empty()
        );

        task.await.expect("stub drains");
    }

    // ----- service: download reuses the M7 path ---------------------------

    #[tokio::test]
    async fn download_reuses_the_files_path_and_reports_progress() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok("recording bytes")]).await;
        let (_store, svc) = test_recordings(&base);

        let dest = std::env::temp_dir().join(format!("squads-rec-{}", uuid::Uuid::new_v4()));
        let dest_str = dest.to_string_lossy().to_string();
        let recording = Recording {
            id: "R1".into(),
            name: "sync.mp4".into(),
            size: 15,
            mime: Some("video/mp4".into()),
            web_url: None,
            download_url: None,
            drive_id: Some("D1".into()),
            created: None,
            modified: None,
            duration_ms: None,
            is_video: true,
            source: RecordingSource::OneDrive,
        };
        let progress = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
        let sink = Arc::clone(&progress);
        let written = svc
            .download(&recording, &dest_str, move |done, total| {
                sink.lock().unwrap().push((done, total));
            })
            .await
            .expect("download");
        assert_eq!(written, 15);
        assert_eq!(std::fs::read(&dest).unwrap(), b"recording bytes");
        std::fs::remove_file(&dest).ok();
        assert_eq!(*progress.lock().unwrap(), vec![(0, 15), (15, 15)]);

        task.await.expect("stub drains");
        assert!(
            requests(&seen)[0].starts_with("GET /drives/D1/items/R1/content "),
            "{}",
            requests(&seen)[0]
        );
    }

    #[tokio::test]
    async fn download_without_a_drive_id_errors_before_network() {
        let (_store, svc) = test_recordings("http://127.0.0.1:9");
        let recording = Recording {
            id: "R1".into(),
            name: "broken.mp4".into(),
            size: 0,
            mime: None,
            web_url: None,
            download_url: None,
            drive_id: None,
            created: None,
            modified: None,
            duration_ms: None,
            is_video: true,
            source: RecordingSource::OneDrive,
        };
        let err = svc
            .download(&recording, "/tmp/never", |_, _| {})
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("no drive id"), "err = {err}");
        assert_eq!(svc.client().request_count(), 0);
    }

    #[tokio::test]
    async fn download_transcript_uses_the_drive_content_path() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok("WEBVTT")]).await;
        let (_store, svc) = test_recordings(&base);
        let transcript = Transcript {
            id: "T1".into(),
            name: "sync.vtt".into(),
            size: 6,
            mime: Some("text/vtt".into()),
            web_url: None,
            drive_id: Some("D1".into()),
            created: None,
            modified: None,
            source: RecordingSource::OneDrive,
        };
        let dest = std::env::temp_dir().join(format!("squads-vtt-{}", uuid::Uuid::new_v4()));
        let dest_str = dest.to_string_lossy().to_string();
        let written = svc
            .download_transcript(&transcript, &dest_str, |_, _| {})
            .await
            .expect("download transcript");
        assert_eq!(written, 6);
        assert_eq!(std::fs::read(&dest).unwrap(), b"WEBVTT");
        std::fs::remove_file(&dest).ok();

        task.await.expect("stub drains");
        assert!(
            requests(&seen)[0].starts_with("GET /drives/D1/items/T1/content "),
            "{}",
            requests(&seen)[0]
        );
    }

    // ----- invariants -----------------------------------------------------

    #[test]
    fn recordings_is_send_and_sync_and_debug_is_redacting() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Recordings>();
        let (_store, svc) = test_recordings("http://127.0.0.1:9");
        let dbg = format!("{svc:?}");
        assert!(dbg.contains("Recordings"));
        assert!(dbg.contains("default"));
    }

    #[test]
    fn cached_reads_never_touch_the_network() {
        let (store, svc) = test_recordings("http://127.0.0.1:9");
        store
            .upsert_recordings(
                "default",
                Some("c1"),
                &[Recording {
                    id: "R1".into(),
                    name: "a.mp4".into(),
                    size: 1,
                    mime: None,
                    web_url: None,
                    download_url: None,
                    drive_id: Some("D1".into()),
                    created: None,
                    modified: None,
                    duration_ms: None,
                    is_video: true,
                    source: RecordingSource::OneDrive,
                }],
            )
            .unwrap();
        assert_eq!(svc.cached_recordings("c1").unwrap().len(), 1);
        assert_eq!(svc.profile(), "default");
        assert_eq!(svc.client().request_count(), 0);
    }
}
