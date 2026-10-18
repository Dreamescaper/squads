//! Protocol clients (milestone **M2**).
//!
//! This module is the thin, UI-free facade over the vendored protocol crate
//! (ADR-001): we **wrap** `ost` rather than reimplement it. Two things live
//! here:
//!
//! - [`tokens_to_config`] — the pure bridge from our M1 [`TokenSet`] into the
//!   vendored `ost::config::Config` the client is built from.
//! - [`ApiClient`] — owns an `ost::api::client::TeamsClient` built with
//!   `TeamsClient::from_config` (ADR-010), exposing typed calls
//!   ([`ApiClient::list_chats`], [`ApiClient::read_messages`]), region-routing
//!   accessors and a light request counter.
//!
//! Constructing an [`ApiClient`] never touches the network: `from_config` only
//! stores the in-memory tokens and the shared HTTP pool is lazy. Network access
//! happens on the first `list_chats` / `read_messages` call, which is why the
//! exit-criterion test drives the real code path against a local TCP stub.
//!
//! ## Which surface serves which call (M5/M6)
//!
//! Teams spreads one logical operation across three hosts, and the wrappers
//! keep that split explicit (see `docs/DECISIONS.md`, channel routing):
//!
//! - **Chat service** (`region_gtms.chatService`, `skypetoken`) — messages in
//!   chats **and** channels: [`ApiClient::send_message`],
//!   [`ApiClient::read_messages_page`], [`ApiClient::thread_reply`], reactions
//!   and read receipts. Channel conversation ids (`19:…@thread.tacv2`) are
//!   accepted by the same endpoints; a channel *thread reply* targets the
//!   derived conversation `<channel>;messageid=<root>`.
//! - **Graph** (`region_gtms.graphBase`, Graph token) — metadata reads
//!   ([`ApiClient::list_teams`], [`ApiClient::list_team_members`],
//!   [`ApiClient::team_thread_id`]) and channel **creation**
//!   ([`ApiClient::create_channel`]).
//! - **Middle tier** (`region_gtms.middleTier`, AAD bearer + `X-Skypetoken`) —
//!   the writes Graph refuses: channel edit/delete
//!   ([`ApiClient::update_channel`], [`ApiClient::delete_channel`]) and team
//!   creation ([`ApiClient::create_team`]).
//!
//! **Files (M7)** are **Graph** throughout, keyed by `(drive_id, item_id)`:
//! listing ([`ApiClient::list_chat_files`], [`ApiClient::list_folder_children`],
//! [`ApiClient::list_drive_recents`], [`ApiClient::list_message_files`]),
//! download/upload, sharing links, versions and the rename/move/copy/delete
//! manage actions. Chat file listings are mined from chat-service history
//! (`properties.files`) and each ref is resolved through Graph
//! `/shares/{u!<base64url>}/driveItem`; channel listings ride the channel
//! `filesFolder`. See `crate::files`.
//!
//! Channel reactions and read receipts are **best-effort**: they ride separate
//! Graph paths that are not live-verified, and the chat-service consumption
//! horizon 403s on `tacv2` threads (see `docs/BLOCKERS.md`).
//!
//! **Identity (M13)** is Graph `GET /me` ([`ApiClient::whoami`]); **inline media
//! (M13)** is [`ApiClient::fetch_media`] plus the pure host rule
//! ([`media_needs_auth`] / [`media_auth_headers`]). The cached, one-fetch
//! variant lives in [`crate::MediaCache`].
//!
//! **File & people search (B3)** is **Graph** too, and **single-window**:
//! [`ApiClient::search_files`] wraps `GET /me/drive/root/search(q='…')` and
//! [`ApiClient::search_people`] wraps `GET /users?$search="displayName:…"`
//! (which sends `ConsistencyLevel: eventual`). The vendored helpers expose no
//! paging cursor, so a limit is clamped to Graph's `1..=25` window
//! ([`FIND_MAX_LIMIT`]). [`crate::Files::search_files`] and
//! [`crate::People::search`] map the wire rows into
//! [`FileEntry`](crate::model::FileEntry) / [`Person`](crate::model::Person).
//!
//! **Chat management (B2a)** creates/leaves chats and writes the per-user
//! mute/hide properties. 1:1 and group creation ride the **chat service**
//! ([`ApiClient::create_one_to_one_chat`], [`ApiClient::create_group_chat`]) —
//! Graph `POST /me/chats` answers `405` and Graph `POST /chats` needs
//! `Chat.Create`, which the Teams token lacks. Leaving
//! ([`ApiClient::leave_chat`]) removes the own MRI from the thread roster;
//! mute/hide ([`ApiClient::set_chat_muted`], [`ApiClient::set_chat_hidden`])
//! write conversation properties; the roster ([`ApiClient::chat_members`]) is
//! the chat-service thread members. See [`crate::Chats`].
//!
//! **Folders & pinned messages (B2b)** are two more lanes. Chat **folders**
//! ([`ApiClient::conversation_folders`], [`ApiClient::move_chat_to_folder`])
//! ride the **CSA** host with the skype token as a Bearer; the move is a
//! hierarchy GET + a pure-edit POST. **Pins** ([`ApiClient::pinned_messages`])
//! read the **chat-service** thread properties (the Graph `pinnedMessages`
//! fallback needs `Chat.Read` and is expected to 403 live); unpinning
//! ([`ApiClient::unpin_message`]) is a Graph `DELETE` the Teams token is not
//! granted, so it refuses before any network. See [`crate::Folders`] /
//! [`crate::Pins`].
//!
//! **Team administration (B4)** adds the rest of the teams surface: public-team
//! discovery + join ([`ApiClient::search_public_teams`],
//! [`ApiClient::join_team`]), member admin ([`ApiClient::add_team_member`],
//! [`ApiClient::remove_team_member`]) and team settings
//! ([`ApiClient::team_settings`]) — all **Graph**. Channel message **reactions**
//! ([`ApiClient::set_channel_reaction`] / [`ApiClient::unset_channel_reaction`])
//! use the dedicated **Graph** `…/messages/{id}/(set|unset)Reaction` lane
//! (including the `replies/{reply}` variant), because the chat-service reaction
//! endpoint 404s on channel threads. See [`crate::Teams`].
//!
//! **Recordings & transcripts (B5)** are Graph **driveItems** too: a non-channel
//! meeting's recording/transcript lives in the signed-in user's OneDrive
//! `Recordings` folder and a channel meeting's in the channel's SharePoint
//! `filesFolder`. [`ApiClient::list_recordings`] / [`ApiClient::search_recordings`]
//! and [`ApiClient::list_transcripts`] / [`ApiClient::search_transcripts`] wrap
//! the vendored **global** lanes (one call fans out over OneDrive and every
//! channel drive; there is no per-chat/per-channel variant). See
//! [`crate::Recordings`].

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use ost::api::ChatInfo;
use ost::api::MessageInfo;
use ost::api::client::TeamsClient;
use ost::config::Config;

/// Protocol types surfaced by the M5 chat calls, re-exported so the UI and
/// `messaging` do not need to reach into the vendored crate.
pub use ost::api::{MessagesPage, REACTION_EMOJI, ReadReceipt, SentMessage};

/// Protocol types surfaced by the M6 teams/channel calls.
pub use ost::api::{ChannelInfo, TeamCreateResult, TeamInfo, TeamMemberInfo};

/// Protocol types surfaced by the M7 file calls. `SharedLink` is deliberately
/// **not** re-exported here: the vendored crate does not expose it, so
/// [`ApiClient::create_link`] maps the vendored value into
/// [`crate::model::SharedLink`].
pub use ost::api::{FileVersion, SharedFile, UploadProgress};

/// Protocol type surfaced by the M8 Graph presence call.
pub use ost::api::PresenceInfo;

/// Protocol type surfaced by the M13 `whoami` call (the signed-in identity).
pub use ost::api::UserInfo;

/// Inline-media types surfaced by the M13 media wrapper: fetched bytes + the
/// vendored size cap.
pub use ost::api::{MAX_BYTES, MediaBytes};

/// Protocol types surfaced by the M12 calendar calls, re-exported so the UI and
/// `calendar` do not reach into the vendored crate. The **pure** helpers
/// ([`parse_join_url`], [`normalize_join_meeting_id`], [`calendar_view_path`],
/// [`calweek_view_path`], [`parse_calendar_view`], [`validate_schedule`],
/// [`schedule_event_body`], [`parse_created_event`], [`lobby_next`]) are
/// re-exported too, so callers can pin their shapes without a network.
pub use ost::api::{
    JoinIdResolve, JoinTarget, LobbyEvent, LobbyState, MeetingInfo, ResolvedJoinId,
    calendar_view_path, calweek_view_path, lobby_next, normalize_join_meeting_id,
    parse_calendar_view, parse_created_event, parse_join_url, schedule_event_body,
    validate_schedule,
};

/// Protocol types and pure helpers surfaced by the **B1** server message search
/// (Graph `/search/query`). Re-exported so `search` and the tests can pin the
/// request/response shapes without reaching into the vendored crate.
pub use ost::api::{
    SEARCH_MAX_SIZE, SearchHitInfo, SearchPage, clamp_size, next_from, parse_search_response,
    search_body,
};

/// Pure helpers surfaced by the **B3** file & people search (Graph drive search
/// + `/users` `$search`). Re-exported so [`Files`](crate::Files) /
/// [`People`](crate::People) and the tests can pin the request shapes without
/// reaching into the vendored crate. Both searches are **single-window**: the
/// vendored helpers expose no paging cursor, so a limit is clamped to Graph's
/// `1..=25` window ([`FIND_MAX_LIMIT`]) and one call returns at most that many
/// rows.
pub use ost::api::{
    FIND_MAX_LIMIT, clamp_limit, drive_search_path, parse_drive_search_response,
    parse_people_search_response, people_search_path,
};

/// Protocol types and pure helpers surfaced by the **B2a** chat management
/// (create 1:1 / group, leave, server mute/hide, roster). Re-exported so
/// [`Chats`](crate::Chats) and the tests can pin the request/response shapes
/// without reaching into the vendored crate. The write shapes are the vendored
/// ones: 1:1/group creation ride the **chat service** (`POST /v1/threads`),
/// leave removes the own MRI from the thread roster, and mute/hide write the
/// per-user conversation properties (`alerts`, `unpinnedTime`).
pub use ost::api::{
    ChatMemberInfo, RosterSource, alerts_body, alerts_muted, alerts_url, chat_members_path,
    conversation_property_url, created_thread_id, group_chat_create_body, group_chat_members,
    hide_chat_properties, leave_member_url, one_to_one_create_body, one_to_one_create_path,
    parse_created_chat, parse_graph_chat_members, parse_thread_members, thread_create_url,
};

/// Protocol types and pure helpers surfaced by the **B2b** folders & pinned
/// messages (chat folders + server-side pins). Re-exported so
/// [`Folders`](crate::Folders) / [`Pins`](crate::Pins) and the tests can pin the
/// request/response shapes without reaching into the vendored crate.
///
/// **Folders** ride the CSA host (`conversation_folders_url`) with the skype
/// token as a Bearer (the same host/token pair `csa_get` uses); the move is a
/// GET (folder hierarchy version + membership) then a POST of the pure
/// [`folder_move_actions`] / [`folder_move_body`] edits. **Pins** read the
/// chat-service thread properties (`thread_pins_url`) and fall back to Graph
/// `pinnedMessages` ([`graph_pins_path`]); the Graph read needs `Chat.Read`,
/// which the Teams token lacks, so it is expected to 403 live. Unpinning is
/// Graph `DELETE` ([`graph_unpin_path`]) and is **not granted** to the Teams
/// token — [`chat_unpin_message_with_client`] refuses before any network.
pub use ost::api::{
    CHATSVCAGG_SCOPE, ConversationFolder, PinSource, PinnedRef, SYSTEM_FOLDER_TYPES,
    conversation_folders_url, fill_pin_from_message, folder_move_actions, folder_move_body,
    graph_pins_path, graph_unpin_path, parse_conversation_folders, parse_graph_pins,
    parse_thread_pins, pick_pins, thread_pins_url,
};

/// Protocol types and pure helpers surfaced by the **B4** team administration
/// (public-team discovery + join, member add/remove, team settings, and the
/// channel message-reaction lanes). Re-exported so [`Teams`](crate::Teams) and
/// the tests can pin the request/response shapes without reaching into the
/// vendored crate.
///
/// **Public-team discovery** is Graph (`/groups` `$search`, falling back to
/// `/teams` `startswith`) and single-window ([`PUBLIC_TEAMS_MAX`]); a hit is
/// joined with [`join_team_data`] (a `POST /teams/{id}/members` for self).
/// **Member admin** ([`add_team_member_data`] / [`remove_team_member_data`])
/// is Graph too. **Team settings** ([`team_settings_data`]) is two Graph GETs.
/// **Channel reactions** ([`set_channel_reaction_data`] /
/// [`unset_channel_reaction_data`]) are the dedicated Graph lane
/// (`…/messages/{id}/(set|unset)Reaction`, with a `replies/{reply}` variant):
/// the chat-service reaction endpoint 404s on channel threads, which is why
/// this is separate from the chat reaction path (see `docs/DECISIONS.md`,
/// ADR-040, and U-003).
pub use ost::api::{
    PUBLIC_TEAMS_MAX, PublicTeamInfo, PublicTeamsSource, TeamMemberSettings, add_member_body,
    add_team_member_data, channel_react_body, channel_reply_set_reaction_path,
    channel_reply_unset_reaction_path, channel_set_reaction_path, channel_unset_reaction_path,
    join_team_data, member_path, members_path, parse_public_teams, public_teams_groups_path,
    public_teams_list_path, remove_team_member_data, search_public_teams_data,
    set_channel_reaction_data, team_settings_data, unset_channel_reaction_data,
};

/// Protocol types and pure helpers surfaced by the **B5** recordings &
/// transcripts lanes. Both are Graph **driveItems**: a non-channel meeting's
/// recording/transcript lives in the signed-in user's OneDrive `Recordings`
/// folder and a channel meeting's in the channel's SharePoint `filesFolder`
/// `Recordings` folder, so list/search reuse the M7 files-stack shapes.
///
/// The vendored list/search functions are **global** — one call fans out over
/// OneDrive **and** every channel drive (a bounded `list_teams_data` scan plus
/// per-channel `filesFolder` lookups) and returns one merged, newest-first list.
/// There is no per-chat/per-channel variant and no paging cursor; a limit is
/// clamped by the vendored path builder ([`RECORDINGS_MAX_LIMIT`] /
/// [`TRANSCRIPTS_MAX_LIMIT`]). The pure helpers
/// ([`recordings_children_path`], [`recordings_search_path`],
/// [`is_recording_video`], [`sort_recordings_newest`],
/// [`recordings_clamp_limit`], [`parse_recordings_response`] and the
/// transcript equivalents) are re-exported so their shapes are pinned without a
/// network.
pub use ost::api::{
    RECORDINGS_MAX_LIMIT, RecordingInfo, RecordingSource, is_recording_video,
    parse_recordings_response, recordings_children_path, recordings_clamp_limit,
    recordings_search_path, sort_recordings_newest,
};
pub use ost::api::{
    TRANSCRIPTS_MAX_LIMIT, TranscriptInfo, TranscriptSource, is_transcript,
    parse_transcripts_response, sort_transcripts_newest, transcripts_children_path,
    transcripts_clamp_limit, transcripts_search_path,
};

use crate::auth::tokens::{StoredToken, TokenSet};
use crate::error::Result;
use crate::model::SharedLink;

/// Map our M1 [`TokenSet`] into the vendored [`ost::config::Config`].
///
/// Pure and offline. Each token carries our **absolute** `expires_at` straight
/// through (we deliberately do not use `StoredToken::new`, which would recompute
/// the deadline from a lifetime and shift it by the call time). `region_gtms`
/// is stored as its JSON string (the vendored `Config` keeps it as a string for
/// TOML compatibility) and is `None` when our blob is JSON `null`.
pub fn tokens_to_config(tokens: &TokenSet) -> Config {
    // Absolute-deadline conversion: `ost::auth::StoredToken` also has a
    // five-minute skew in `is_expired`, matching our own expiry rule.
    let stored = |t: &StoredToken| ost::auth::StoredToken {
        token: t.token.clone(),
        expires_at: Some(t.expires_at),
    };

    Config {
        access_token: Some(stored(&tokens.spaces)),
        refresh_token: Some(tokens.refresh_token.clone()),
        tenant_id: Some(tokens.tenant.clone()),
        skype_token: Some(stored(&tokens.skype)),
        graph_token: Some(stored(&tokens.graph)),
        ic3_token: Some(stored(&tokens.ic3)),
        recorder_token: Some(stored(&tokens.recorder)),
        region_gtms: if tokens.region_gtms.is_null() {
            None
        } else {
            Some(tokens.region_gtms.to_string())
        },
        teams_partition: None,
        teams_region: None,
        teams_ring: None,
    }
}

/// Own roster MRI for an Entra object id (`8:orgid:<oid>`) (M13).
///
/// A thin, tested re-export of the vendored [`ost::api::own_member_mri`], used
/// when mapping whoami into an [`Identity`](crate::model::Identity).
pub fn own_member_mri(oid: &str) -> String {
    ost::api::own_member_mri(oid)
}

/// Whether an inline media URL needs the Skype token (M13).
///
/// A thin re-export of the vendored host rule: only Microsoft media hosts
/// (`*.msg.teams.microsoft.com`, `*.asm.skype.com`, …) authenticate; everything
/// else fetches unauthenticated so the token never leaks to third parties.
pub fn media_needs_auth(url: &str) -> bool {
    ost::api::media::needs_auth(url)
}

/// Token headers for one inline media URL (M13), as `(name, value)` pairs.
///
/// Empty for public hosts; the vendored [`ost::api::media::auth_headers`] picks
/// the object-store vs. chat-service scheme per host family.
pub fn media_auth_headers(url: &str, token: &str) -> Vec<(&'static str, String)> {
    ost::api::media::auth_headers(url, token)
}

/// A typed, instrumented facade over the vendored Teams client.
///
/// Holds one `ost::api::client::TeamsClient` (built from an in-memory config)
/// plus the `regionGtms` routing blob and a request counter. `Debug` is
/// redacting: it never reveals the tokens the client is holding.
pub struct ApiClient {
    client: TeamsClient,
    region_gtms: serde_json::Value,
    /// The skype token this client was built with, when one was available.
    ///
    /// The folder lane (B2b) posts to the CSA host with the skype token as a
    /// Bearer (the vendored `conversation_folders_data` takes the bearer as a
    /// parameter because it does not use the client transport); retaining it
    /// here lets [`ApiClient::conversation_folders`] keep a no-argument shape.
    skype_token: Option<String>,
    /// Number of high-level calls issued (list_chats etc.).
    requests: AtomicU64,
}

impl ApiClient {
    /// Build a client from our token set. Offline: no request is made here.
    pub fn from_tokens(tokens: &TokenSet) -> Self {
        Self::from_config(tokens_to_config(tokens))
    }

    /// Build a client over an explicit vendored [`Config`] (tests, dynamic
    /// region overrides). Offline: no request is made here.
    pub fn from_config(config: Config) -> Self {
        let region_gtms = config.get_region_gtms().unwrap_or(serde_json::Value::Null);
        let skype_token = config.get_skype_token().map(|token| token.token);
        Self {
            client: TeamsClient::from_config(config),
            region_gtms,
            skype_token,
            requests: AtomicU64::new(0),
        }
    }

    /// The `regionGtms` routing blob this client was built with (`null` when
    /// the token set carried none).
    pub fn region_gtms(&self) -> &serde_json::Value {
        &self.region_gtms
    }

    /// Resolved chat-service base URL (`region_gtms.chatService` or the
    /// regional default). Handy for diagnostics and later routing work.
    pub fn chat_service_url(&self) -> String {
        self.client.chat_service_url()
    }

    /// Resolved chat-service aggregator base URL.
    pub fn chatsvcagg_url(&self) -> String {
        self.client.chatsvcagg_url()
    }

    /// Resolved apps-platform middle-tier base URL.
    pub fn middle_tier_url(&self) -> String {
        self.client.middle_tier_url()
    }

    /// How many high-level calls this client has served (cheap instrumentation;
    /// the per-request observer lives in the vendored crate).
    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    fn counted(&self) -> u64 {
        self.requests.fetch_add(1, Ordering::Relaxed)
    }

    /// List recent chats (native CSA → chatsvcagg → chat-service fallback,
    /// exactly as the vendored crate implements it).
    pub async fn list_chats(&self, limit: usize) -> Result<Vec<ChatInfo>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.list_chats", limit, call);
        let chats = ost::api::list_chats_data(&self.client, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = chats.len(), call, "listed chats");
        Ok(chats)
    }

    /// Read the newest page of messages for `chat_id`.
    pub async fn read_messages(&self, chat_id: &str, limit: usize) -> Result<Vec<MessageInfo>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span =
            tracing::debug_span!(target: "squads_core::api", "api.read_messages", limit, call);
        let messages = ost::api::read_messages_data(&self.client, chat_id, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = messages.len(), call, "read messages");
        Ok(messages)
    }

    /// Read **one page** of history and its backward cursor (M5).
    ///
    /// `page_url` is the previous page's `backward_link` for older history, or
    /// `None` for the newest page. Unlike [`ApiClient::read_messages`], this
    /// keeps the `_metadata.backwardLink` cursor, which is what backward paging
    /// walks.
    pub async fn read_messages_page(
        &self,
        chat_id: &str,
        limit: usize,
        page_url: Option<&str>,
    ) -> Result<MessagesPage> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.read_messages_page",
            limit,
            paged = page_url.is_some(),
            call
        );
        let page = ost::api::read_messages_page(&self.client, chat_id, limit, page_url)
            .instrument(span)
            .await?;
        tracing::debug!(
            count = page.messages.len(),
            has_more = page.backward_link.is_some(),
            call,
            "read message page"
        );
        Ok(page)
    }

    /// Send a message body with a caller-owned `client_message_id` (M5).
    ///
    /// The `client_message_id` is the idempotency key: a retry reuses the same
    /// value so one logical send never becomes two server messages. The server's
    /// assigned id (if the receipt named one) comes back on [`SentMessage`].
    pub async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        client_message_id: &str,
    ) -> Result<SentMessage> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.send_message",
            len = text.len(),
            call
        );
        let sent =
            ost::api::send_message_with_client_id(&self.client, chat_id, text, client_message_id)
                .instrument(span)
                .await?;
        tracing::debug!(has_id = sent.id.is_some(), call, "sent message");
        Ok(sent)
    }

    /// Post a quote reply with a caller-owned `client_message_id` (M5).
    ///
    /// The parent's sender and text are embedded in the wire `<quote>` block;
    /// the parent id is recovered on read by the vendored `split_reply_quote`.
    #[allow(clippy::too_many_arguments)]
    pub async fn reply_message(
        &self,
        chat_id: &str,
        parent_id: &str,
        parent_sender: &str,
        parent_text: &str,
        text: &str,
        client_message_id: &str,
    ) -> Result<SentMessage> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.reply_message",
            len = text.len(),
            call
        );
        let sent = ost::api::reply_message_with_client_id(
            &self.client,
            chat_id,
            parent_id,
            parent_sender,
            parent_text,
            text,
            client_message_id,
        )
        .instrument(span)
        .await?;
        tracing::debug!(has_id = sent.id.is_some(), call, "sent reply");
        Ok(sent)
    }

    /// Verify whether a message carrying `client_message_id` exists on the
    /// server, by reading the newest page. Used by retry before re-posting
    /// (M5): a timed-out POST that actually landed must not be sent twice.
    pub async fn find_message_by_client_id(
        &self,
        chat_id: &str,
        client_message_id: &str,
    ) -> Result<Option<MessageInfo>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.find_message_by_client_id",
            call
        );
        let found = ost::api::find_message_by_client_id(&self.client, chat_id, client_message_id)
            .instrument(span)
            .await?;
        tracing::debug!(found = found.is_some(), call, "verified send by client id");
        Ok(found)
    }

    /// Edit one of our own messages (PUT with `skypeeditedid`).
    pub async fn edit_message(&self, chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.edit_message", call);
        ost::api::edit_message_with_client(&self.client, chat_id, message_id, text)
            .instrument(span)
            .await?;
        tracing::debug!(call, "edited message");
        Ok(())
    }

    /// Delete one of our own messages (`?behavior=softDelete`).
    pub async fn delete_message(&self, chat_id: &str, message_id: &str) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.delete_message", call);
        ost::api::delete_message_with_client(&self.client, chat_id, message_id)
            .instrument(span)
            .await?;
        tracing::debug!(call, "deleted message");
        Ok(())
    }

    /// Add an emoji reaction to a message. Unsupported emoji is rejected before
    /// any request (see [`ost::api::REACTION_EMOJI`]).
    pub async fn send_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.send_reaction", call);
        ost::api::send_reaction_with_client(&self.client, chat_id, message_id, emoji)
            .instrument(span)
            .await?;
        tracing::debug!(call, "added reaction");
        Ok(())
    }

    /// Remove an emoji reaction from a message.
    pub async fn remove_reaction(
        &self,
        chat_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.remove_reaction", call);
        ost::api::remove_reaction_with_client(&self.client, chat_id, message_id, emoji)
            .instrument(span)
            .await?;
        tracing::debug!(call, "removed reaction");
        Ok(())
    }

    /// Mark a conversation read up to `message_id` (consumption horizon PUT).
    pub async fn mark_read(&self, chat_id: &str, message_id: &str) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.mark_read", call);
        ost::api::mark_read_with_client(&self.client, chat_id, message_id)
            .instrument(span)
            .await?;
        tracing::debug!(call, "marked read");
        Ok(())
    }

    /// Peer read positions for a thread (read-only).
    pub async fn read_receipts(&self, thread_id: &str) -> Result<Vec<ReadReceipt>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.read_receipts",
            call
        );
        let receipts = ost::api::read_receipts_data(&self.client, thread_id)
            .instrument(span)
            .await?;
        tracing::debug!(count = receipts.len(), call, "read receipts");
        Ok(receipts)
    }

    // ----- M6: teams & channels -------------------------------------------

    /// List the joined teams and their channels (Graph `/me/joinedTeams`, then
    /// `GET /teams/{id}/channels` per team — one call to this method issues one
    /// request per team plus the listing).
    ///
    /// Channel-fetch errors degrade to an empty channel list per team (the
    /// vendored behaviour), so a single inaccessible team never fails the list.
    pub async fn list_teams(&self) -> Result<Vec<TeamInfo>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.list_teams", call);
        let teams = ost::api::list_teams_data(&self.client)
            .instrument(span)
            .await?;
        tracing::debug!(count = teams.len(), call, "listed teams");
        Ok(teams)
    }

    /// List one team's roster (Graph `GET /teams/{id}/members`).
    pub async fn list_team_members(&self, team_id: &str) -> Result<Vec<TeamMemberInfo>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.list_team_members",
            call
        );
        let members = ost::api::list_team_members_data(&self.client, team_id)
            .instrument(span)
            .await?;
        tracing::debug!(count = members.len(), call, "listed team members");
        Ok(members)
    }

    /// The team thread id from a Graph `team` answer's `internalId` (pure pass
    /// through to the vendored helper). `None` when the field is absent or not a
    /// thread id.
    pub fn team_internal_id(&self, value: &serde_json::Value) -> Option<String> {
        ost::api::team_internal_id(value)
    }

    /// Resolve a team's **Teams thread id** (`19:…@thread…`) from the Graph
    /// group id the listing uses. Thread ids pass through; group ids read Graph
    /// `GET /teams/{id}?$select=internalId` (the vendored `team_thread_id`).
    ///
    /// This is the id the middle tier keys teams/channels by, so it is the
    /// routing value the channel writes need.
    pub async fn team_thread_id(&self, team_id: &str) -> Result<String> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.team_thread_id",
            call
        );
        let thread = ost::api::team_thread_id(&self.client, team_id)
            .instrument(span)
            .await?;
        tracing::debug!(call, "resolved team thread id");
        Ok(thread)
    }

    /// Create a channel in `team_id` (Graph `POST /teams/{id}/channels`).
    ///
    /// **Routing note:** the vendored `create_channel_data` posts to **Graph**;
    /// unlike channel edit/delete it is not on the middle tier (see the module
    /// docs and `docs/DECISIONS.md`, channel routing). The id may be a Graph
    /// group id or a team thread id — the vendored call uses it verbatim.
    pub async fn create_channel(
        &self,
        team_id: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<ChannelInfo> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.create_channel",
            call
        );
        let channel = ost::api::create_channel_data(&self.client, team_id, name, description)
            .instrument(span)
            .await?;
        tracing::debug!(call, "created channel");
        Ok(channel)
    }

    /// Rename a channel and/or change its description through the **middle
    /// tier** (`PATCH …/beta/teams/{thread}/channels/{channel}`). `name`/
    /// `description` are `Option` (only the given fields change); an empty
    /// change is rejected by the vendored call before any network.
    pub async fn update_channel(
        &self,
        team_id: &str,
        channel_id: &str,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.update_channel",
            call
        );
        ost::api::update_channel_data(&self.client, team_id, channel_id, name, description)
            .instrument(span)
            .await?;
        tracing::debug!(call, "updated channel");
        Ok(())
    }

    /// Delete one channel through the **middle tier**
    /// (`DELETE …/beta/teams/{thread}/channels/{channel}`). The General channel
    /// is refused by the vendored call before any network.
    pub async fn delete_channel(&self, team_id: &str, channel_id: &str) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.delete_channel",
            call
        );
        ost::api::delete_channel_data(&self.client, team_id, channel_id)
            .instrument(span)
            .await?;
        tracing::debug!(call, "deleted channel");
        Ok(())
    }

    /// Create one (private) team through the **middle tier**
    /// (`POST …/beta/teams/create`), then read it back from Graph. Returns the
    /// created team with its channels plus the vendored poll telemetry.
    pub async fn create_team(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<TeamCreateResult> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.create_team", call);
        let result = ost::api::create_team_data(&self.client, name, description)
            .instrument(span)
            .await?;
        tracing::debug!(call, "created team");
        Ok(result)
    }

    /// Post a reply into a channel's thread (reply chain under `root_id`).
    ///
    /// This is **not** the quote-reply path: the vendored
    /// `thread_reply_with_client_id` posts the plain send body (carrying
    /// `client_message_id`) to the thread conversation
    /// `<channel_id>;messageid=<root_id>` on the chat service. Chat/group ids
    /// are rejected by the vendored call before any network.
    pub async fn thread_reply(
        &self,
        channel_id: &str,
        root_id: &str,
        text: &str,
        client_message_id: &str,
    ) -> Result<SentMessage> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.thread_reply",
            len = text.len(),
            call
        );
        let sent = ost::api::thread_reply_with_client_id(
            &self.client,
            channel_id,
            root_id,
            text,
            client_message_id,
        )
        .instrument(span)
        .await?;
        tracing::debug!(has_id = sent.id.is_some(), call, "sent thread reply");
        Ok(sent)
    }

    // ----- M7: files ------------------------------------------------------

    /// List the files shared in a chat or a channel (Graph driveItems).
    ///
    /// `include_folders = false` matches the vendored default (files only);
    /// `true` keeps folder driveItems so a browser can drill into them with
    /// [`ApiClient::list_folder_children`]. Folder entries carry
    /// `SharedFile::is_folder`.
    pub async fn list_chat_files(
        &self,
        chat_id: &str,
        limit: usize,
        include_folders: bool,
    ) -> Result<Vec<SharedFile>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.list_chat_files",
            limit,
            include_folders,
            call
        );
        let files =
            ost::api::list_chat_files_data_opts(&self.client, chat_id, limit, include_folders)
                .instrument(span)
                .await?;
        tracing::debug!(count = files.len(), call, "listed chat files");
        Ok(files)
    }

    /// The files one message shares (timeline file chips).
    ///
    /// Chat ids read the chat service (`properties.files`); channel posts
    /// (`19:…@thread.tacv2`) read the channel message under their team. See the
    /// vendored `list_message_files_data` for the exact routing.
    pub async fn list_message_files(
        &self,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<Vec<SharedFile>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.list_message_files",
            call
        );
        let files = ost::api::list_message_files_data(&self.client, conversation_id, message_id)
            .instrument(span)
            .await?;
        tracing::debug!(count = files.len(), call, "listed message files");
        Ok(files)
    }

    /// List one folder's children by drive + item id (files **and**
    /// subfolders; each carries `is_folder`).
    pub async fn list_folder_children(
        &self,
        drive_id: &str,
        item_id: &str,
        limit: usize,
    ) -> Result<Vec<SharedFile>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.list_folder_children",
            limit,
            call
        );
        let files = ost::api::list_folder_children_data(&self.client, drive_id, item_id, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = files.len(), call, "listed folder children");
        Ok(files)
    }

    /// The signed-in user's recently accessed driveItems (files only; folders
    /// are filtered out by the vendored call).
    pub async fn list_drive_recents(&self, limit: usize) -> Result<Vec<SharedFile>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.list_drive_recents",
            limit,
            call
        );
        let files = ost::api::list_drive_recents_data(&self.client, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = files.len(), call, "listed recent files");
        Ok(files)
    }

    /// Version history for one driveItem, newest first.
    pub async fn list_file_versions(
        &self,
        drive_id: &str,
        item_id: &str,
    ) -> Result<Vec<FileVersion>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.list_file_versions",
            call
        );
        let versions = ost::api::list_file_versions_data(&self.client, drive_id, item_id)
            .instrument(span)
            .await?;
        tracing::debug!(count = versions.len(), call, "listed file versions");
        Ok(versions)
    }

    /// Restore one file version as the current item (Graph `restoreVersion`).
    pub async fn restore_file_version(
        &self,
        drive_id: &str,
        item_id: &str,
        version_id: &str,
    ) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.restore_file_version",
            call
        );
        ost::api::restore_file_version_data(&self.client, drive_id, item_id, version_id)
            .instrument(span)
            .await?;
        tracing::debug!(call, "restored file version");
        Ok(())
    }

    /// Download one driveItem's content to `dest_path`; returns bytes written.
    ///
    /// The vendored helper buffers the whole body before writing, so there is no
    /// fine-grained progress here — see
    /// [`Files::download`](crate::Files::download), which reports around the
    /// transfer.
    pub async fn download_file(
        &self,
        drive_id: &str,
        item_id: &str,
        dest_path: &str,
    ) -> Result<u64> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.download_file", call);
        let written = ost::api::download_file_data(&self.client, drive_id, item_id, dest_path)
            .instrument(span)
            .await?;
        tracing::debug!(written, call, "downloaded file");
        Ok(written)
    }

    /// Upload a local file to a chat or channel and post it as a `reference`
    /// message. `progress` receives `(bytes_sent, bytes_total)` after each
    /// fragment (a simple PUT reports once at completion).
    ///
    /// See [`upload_file_data_with_progress`](ost::api::upload_file_data_with_progress)
    /// for the ≤4 MiB simple-PUT vs. resumable-session split.
    pub async fn upload_file(
        &self,
        chat_id: &str,
        local_path: &str,
        progress: Option<&UploadProgress<'_>>,
    ) -> Result<SharedFile> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.upload_file", call);
        let file =
            ost::api::upload_file_data_with_progress(&self.client, chat_id, local_path, progress)
                .instrument(span)
                .await?;
        tracing::debug!(size = file.size, call, "uploaded file");
        Ok(file)
    }

    /// Create (or fetch the existing) view-only sharing link for one driveItem.
    /// `scope` is normalized by the vendored call
    /// (`anonymous`/`anyone` → anyone-with-link, anything else → organization).
    pub async fn create_link(
        &self,
        drive_id: &str,
        item_id: &str,
        scope: &str,
    ) -> Result<SharedLink> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.create_link", call);
        let link = ost::api::create_link_data(&self.client, drive_id, item_id, scope)
            .instrument(span)
            .await?;
        tracing::debug!(call, "created sharing link");
        Ok(SharedLink {
            url: link.url,
            scope: link.scope,
        })
    }

    /// Rename one driveItem (PATCH `name`). Returns the updated item.
    pub async fn rename_file(
        &self,
        drive_id: &str,
        item_id: &str,
        new_name: &str,
    ) -> Result<SharedFile> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.rename_file", call);
        let file = ost::api::rename_file_data(&self.client, drive_id, item_id, new_name)
            .instrument(span)
            .await?;
        tracing::debug!(call, "renamed file");
        Ok(file)
    }

    /// Move one driveItem to another folder in the same drive. Returns the
    /// updated item.
    pub async fn move_file(
        &self,
        drive_id: &str,
        item_id: &str,
        dest_folder_id: &str,
    ) -> Result<SharedFile> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.move_file", call);
        let file = ost::api::move_file_data(&self.client, drive_id, item_id, dest_folder_id)
            .instrument(span)
            .await?;
        tracing::debug!(call, "moved file");
        Ok(file)
    }

    /// Copy one driveItem to another folder in the same drive. Graph copies
    /// asynchronously (`202` + a `Location` monitor URL); returns that URL, or
    /// `""` when the server omitted it.
    pub async fn copy_file(
        &self,
        drive_id: &str,
        item_id: &str,
        dest_folder_id: &str,
        new_name: Option<&str>,
    ) -> Result<String> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.copy_file", call);
        let monitor =
            ost::api::copy_file_data(&self.client, drive_id, item_id, dest_folder_id, new_name)
                .instrument(span)
                .await?;
        tracing::debug!(call, "copying file");
        Ok(monitor)
    }

    /// Delete one driveItem.
    pub async fn delete_file(&self, drive_id: &str, item_id: &str) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.delete_file", call);
        ost::api::delete_file_data(&self.client, drive_id, item_id)
            .instrument(span)
            .await?;
        tracing::debug!(call, "deleted file");
        Ok(())
    }

    // ----- M8: presence (Graph) -------------------------------------------

    /// `GET /me/presence` — the signed-in user's own presence.
    ///
    /// **Live this 403s**: the Teams first-party token does not carry the
    /// `Presence.Read*` scopes, which is why [`Presence`](crate::Presence)
    /// defaults to the unified backend (see [`crate::presence`]). The wrapper is
    /// kept for a suitably-scoped token and for the offline stub tests.
    pub async fn get_presence(&self) -> Result<PresenceInfo> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.get_presence", call);
        let info = ost::api::get_presence_data(&self.client)
            .instrument(span)
            .await?;
        tracing::debug!(call, "read own presence");
        Ok(info)
    }

    /// `POST /me/presence/setUserPreferredPresence` — set own presence.
    ///
    /// `expiration_duration` is an ISO-8601 duration (`PT1H`); the body shape
    /// matches the vendored `set_presence` helper (which builds its own client,
    /// so we reproduce the body and post through this client).
    pub async fn set_user_preferred_presence(
        &self,
        availability: &str,
        activity: &str,
        expiration_duration: &str,
    ) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.set_user_preferred_presence",
            call
        );
        let body = serde_json::json!({
            "sessionId": "squads-core",
            "availability": availability,
            "activity": activity,
            "expirationDuration": expiration_duration,
        });
        self.client
            .graph_post("/me/presence/setUserPreferredPresence", &body)
            .instrument(span)
            .await?;
        tracing::debug!(call, "set own presence");
        Ok(())
    }

    // ----- M12: calendar (Graph) ------------------------------------------

    /// Fetch the signed-in user's upcoming meetings (Graph `calendarView` over
    /// the next 7 days, soonest first). Each event carries its Teams join URL
    /// when it has one.
    pub async fn list_upcoming_meetings(&self, limit: usize) -> Result<Vec<MeetingInfo>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.list_upcoming_meetings",
            limit,
            call
        );
        let meetings = ost::api::list_upcoming_meetings_data(&self.client, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = meetings.len(), call, "listed upcoming meetings");
        Ok(meetings)
    }

    /// Fetch the meetings in the explicit window `[start_secs, start_secs+days]`
    /// (Graph `calendarView`; `days` must be `1..=31`, checked before any
    /// network), soonest first. Backs the calendar **week view**.
    pub async fn list_week_meetings(
        &self,
        start_secs: u64,
        days: u64,
        limit: usize,
    ) -> Result<Vec<MeetingInfo>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.list_week_meetings",
            days,
            limit,
            call
        );
        let meetings = ost::api::list_week_meetings_data(&self.client, start_secs, days, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = meetings.len(), call, "listed week meetings");
        Ok(meetings)
    }

    /// Schedule a meeting (`POST /me/calendar/events`). `online` requests a
    /// Teams meeting (`isOnlineMeeting` + `teamsForBusiness`), so Graph
    /// provisions the join URL. Inputs are validated by the vendored call
    /// before any network.
    pub async fn schedule_meeting(
        &self,
        subject: &str,
        start: &str,
        end: &str,
        time_zone: &str,
        online: bool,
    ) -> Result<MeetingInfo> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.schedule_meeting",
            online,
            call
        );
        let meeting =
            ost::api::schedule_meeting_data(&self.client, subject, start, end, time_zone, online)
                .instrument(span)
                .await?;
        tracing::debug!(call, "scheduled meeting");
        Ok(meeting)
    }

    /// Cancel one meeting (`DELETE /me/calendar/events/{id}`; Graph answers
    /// `204`, no body). A malformed id is rejected before any network.
    pub async fn cancel_meeting(&self, event_id: &str) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.cancel_meeting", call);
        ost::api::cancel_meeting_data(&self.client, event_id)
            .instrument(span)
            .await?;
        tracing::debug!(call, "cancelled meeting");
        Ok(())
    }

    /// Resolve a typed numeric meeting id + passcode to its join URL via Graph
    /// (`OnlineMeetings.Read`). A malformed id is rejected before any network.
    pub async fn resolve_join_meeting_id(
        &self,
        meeting_id: &str,
        passcode: &str,
    ) -> Result<JoinIdResolve> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.resolve_join_meeting_id",
            call
        );
        let resolved = ost::api::resolve_join_meeting_id_data(&self.client, meeting_id, passcode)
            .instrument(span)
            .await?;
        tracing::debug!(call, "resolved join meeting id");
        Ok(resolved)
    }

    // ----- M13: identity & inline media -----------------------------------

    /// `GET /me` — the signed-in user's profile (M13).
    ///
    /// This is what lets server-loaded history and realtime messages be marked
    /// own: the caller maps the returned [`UserInfo`] into an
    /// [`Identity`](crate::model::Identity) (MRI `8:orgid:<id>`) and threads it
    /// into [`Messaging`](crate::Messaging) / the realtime collector.
    pub async fn whoami(&self) -> Result<UserInfo> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.whoami", call);
        let info = ost::api::whoami_data(&self.client).instrument(span).await?;
        tracing::debug!(call, "read whoami");
        Ok(info)
    }

    /// Download one inline media URL (a chat `<img src>`) (M13).
    ///
    /// The vendored helper attaches the Skype token **only** on Microsoft media
    /// hosts ([`media_needs_auth`]) and never to third parties, and rejects
    /// non-`https` URLs and over-cap payloads. Callers that want caching should
    /// use [`MediaCache`](crate::MediaCache) instead.
    pub async fn fetch_media(&self, url: &str) -> Result<MediaBytes> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.fetch_media", call);
        let bytes = ost::api::fetch_media_data(&self.client, url)
            .instrument(span)
            .await?;
        tracing::debug!(len = bytes.data.len(), call, "fetched inline media");
        Ok(bytes)
    }

    // ----- B1: message search (Graph) -------------------------------------

    /// Search the signed-in user's messages through Graph
    /// (`POST /search/query`, entity `chatMessage`).
    ///
    /// **Optional secondary path.** This needs the `Chat.Read` delegated scope,
    /// which the Teams first-party sign-in token does **not** carry, so it is
    /// expected to 403 live (the vendored crate's own notes flag it as
    /// unproven). The local-first [`Search`](crate::Search) over the cached
    /// store is the primary path (see ADR-036); this wrapper is kept for a
    /// suitably-scoped token and for the offline stub tests. `limit` is clamped
    /// to Graph's `1..=25` window by the vendored call.
    pub async fn search_messages(&self, query: &str, limit: usize) -> Result<SearchPage> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span =
            tracing::debug_span!(target: "squads_core::api", "api.search_messages", limit, call);
        let page = ost::api::search_messages_data(&self.client, query, 0, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = page.hits.len(), call, "searched messages");
        Ok(page)
    }

    // ----- B3: file & people search (Graph) -------------------------------

    /// Search the signed-in user's OneDrive by file name/content (Graph
    /// `GET /me/drive/root/search(q='…')`).
    ///
    /// **Single window.** The vendored path builder clamps `limit` to Graph's
    /// `1..=25` window ([`FIND_MAX_LIMIT`]) and there is no paging cursor, so
    /// one call returns at most that many driveItems. Hits are driveItems, so
    /// [`Files::search_files`](crate::Files::search_files) maps them into
    /// [`FileEntry`](crate::model::FileEntry).
    pub async fn search_files(&self, query: &str, limit: usize) -> Result<Vec<SharedFile>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span =
            tracing::debug_span!(target: "squads_core::api", "api.search_files", limit, call);
        let files = ost::api::search_files_data(&self.client, query, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = files.len(), call, "searched files");
        Ok(files)
    }

    /// Search the directory by display name (Graph
    /// `GET /users?$search="displayName:…"`).
    ///
    /// **Single window**, like [`ApiClient::search_files`]. The vendored helper
    /// sends `ConsistencyLevel: eventual` (Graph 400s without it for `$search`).
    /// Rows are the vendored `TeamMemberInfo` projection;
    /// [`People::search`](crate::People::search) maps them into
    /// [`Person`](crate::model::Person).
    pub async fn search_people(&self, query: &str, limit: usize) -> Result<Vec<TeamMemberInfo>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span =
            tracing::debug_span!(target: "squads_core::api", "api.search_people", limit, call);
        let people = ost::api::search_people_data(&self.client, query, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = people.len(), call, "searched people");
        Ok(people)
    }

    // ----- B2a: chat management (create / leave / mute / hide / roster) ----

    /// Create (or re-open) the **1:1** chat with `user_id` (a Graph/AAD id or
    /// UPN).
    ///
    /// Wraps the vendored `create_one_to_one_chat_data`: both AAD ids resolve to
    /// the derived `19:<lo>_<hi>@unq.gbl.spaces` thread id (OstMac §106), an
    /// existing thread re-opens after a probe `GET`, and a first contact creates
    /// the unique-roster 1:1 on the **chat service** (`POST /v1/threads`).
    /// Graph `POST /me/chats` answers `405` and Graph `POST /chats` needs
    /// `Chat.Create`, which the Teams token lacks. A blank ref, or the
    /// signed-in user, is rejected before any create.
    pub async fn create_one_to_one_chat(&self, user_id: &str) -> Result<ChatInfo> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.create_one_to_one_chat",
            call
        );
        let chat = ost::api::create_one_to_one_chat_data(&self.client, user_id)
            .instrument(span)
            .await?;
        tracing::debug!(call, "created/re-opened 1:1 chat");
        Ok(chat)
    }

    /// Create a **group** chat with `member_ids` (Graph/AAD ids or UPNs) plus
    /// the signed-in user, with an optional `topic`, on the **chat service**
    /// (`POST /v1/threads`, skypetoken).
    ///
    /// Wraps the vendored `create_group_chat_data`: UPNs resolve to object ids
    /// through Graph (`User.ReadBasic.All`), the creator is added first and
    /// every peer `Admin`, a blank topic is omitted, and at least one peer is
    /// required (checked before any network). The new thread id comes from the
    /// response `Location` header.
    pub async fn create_group_chat(
        &self,
        topic: Option<&str>,
        member_ids: &[String],
    ) -> Result<ChatInfo> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.create_group_chat",
            members = member_ids.len(),
            call
        );
        let chat = ost::api::create_group_chat_data(&self.client, member_ids, topic)
            .instrument(span)
            .await?;
        tracing::debug!(call, "created group chat");
        Ok(chat)
    }

    /// Leave `chat_id` by removing the signed-in user from the thread roster
    /// (`DELETE .../v1/threads/{id}/members/{mri}`, chat service).
    ///
    /// Wraps the vendored `leave_chat_with_client`: the own MRI resolves through
    /// whoami (`8:orgid:<oid>`); an empty chat id is rejected before any
    /// network. Best-effort server-side — see [`Chats::leave`](crate::Chats::leave)
    /// for the store effect.
    pub async fn leave_chat(&self, chat_id: &str) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.leave_chat", call);
        ost::api::leave_chat_with_client(&self.client, chat_id)
            .instrument(span)
            .await?;
        tracing::debug!(call, "left chat");
        Ok(())
    }

    /// Mute or unmute `chat_id` for the signed-in user (the server-side
    /// `alerts` conversation property: `"false"` = muted).
    ///
    /// This is the **server** half of B2a's mute; the local routing flag stays
    /// [`Store::set_chat_muted`](crate::Store::set_chat_muted) (see
    /// [`Chats::set_muted`](crate::Chats::set_muted)).
    pub async fn set_chat_muted(&self, chat_id: &str, muted: bool) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span =
            tracing::debug_span!(target: "squads_core::api", "api.set_chat_muted", muted, call);
        ost::api::set_chat_muted_with_client(&self.client, chat_id, muted)
            .instrument(span)
            .await?;
        tracing::debug!(call, "set chat mute");
        Ok(())
    }

    /// Hide or unhide `chat_id` for the signed-in user (the `unpinnedTime` /
    /// `historyHiddenTime` conversation properties the web client writes; hide
    /// stamps the current time, unhide writes `null`).
    ///
    /// This is the **server** half of B2a's hide; the local flag stays
    /// [`Store::set_chat_hidden`](crate::Store::set_chat_hidden).
    pub async fn set_chat_hidden(&self, chat_id: &str, hidden: bool) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span =
            tracing::debug_span!(target: "squads_core::api", "api.set_chat_hidden", hidden, call);
        ost::api::set_chat_hidden_with_client(&self.client, chat_id, hidden)
            .instrument(span)
            .await?;
        tracing::debug!(call, "set chat hidden");
        Ok(())
    }

    /// The roster of `chat_id`: the chat-service thread members (MRIs,
    /// friendly names when the service has them, `Admin`/`User` roles), with
    /// blank names filled from Graph.
    ///
    /// Wraps the vendored `list_chat_members_data` and drops its roster-source
    /// tag (the vendored Graph `GET /chats/{id}/members` lane is gone: the
    /// Teams token lacks `ChatMember.Read`). A blank id is rejected before any
    /// network; a failure surfaces the chat-service error rather than an empty
    /// roster.
    pub async fn chat_members(&self, chat_id: &str) -> Result<Vec<ChatMemberInfo>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.chat_members", call);
        let (_source, members) = ost::api::list_chat_members_data(&self.client, chat_id)
            .instrument(span)
            .await?;
        tracing::debug!(count = members.len(), call, "listed chat members");
        Ok(members)
    }

    // ----- B2b: folders & pinned messages ---------------------------------

    /// The signed-in user's chat **folders** (Favorites + user-created; the
    /// server-generated views are dropped by the vendored parser).
    ///
    /// Wraps the vendored `conversation_folders_data`, which posts to the CSA
    /// host ([`conversation_folders_url`]) with the **skype token** as a Bearer
    /// (the same host/token pair `csa_get` uses for the chat list). It is a
    /// standalone `reqwest` call in the vendored crate, so this needs the token
    /// retained on the client ([`ApiClient::skype_token`]); a client built
    /// without one is refused before any network. See [`Folders`](crate::Folders).
    pub async fn conversation_folders(&self) -> Result<Vec<ConversationFolder>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.conversation_folders",
            call
        );
        let bearer = self
            .skype_token
            .as_deref()
            .ok_or_else(|| crate::error::CoreError::NotSignedIn)?;
        let folders = ost::api::conversation_folders_data(bearer)
            .instrument(span)
            .await?;
        tracing::debug!(count = folders.len(), call, "listed conversation folders");
        Ok(folders)
    }

    /// Move `chat_id` into `target` (a folder id; blank = out of every folder)
    /// and return the folders after the move.
    ///
    /// Wraps the vendored `conversation_folder_move_with_client`: a fresh GET of
    /// the folder hierarchy, one POST of the pure [`folder_move_actions`] /
    /// [`folder_move_body`] edits, then a re-read. A blank chat id or a missing
    /// skype token is refused before any network; the vendored call checks that
    /// the answer shows the chat where it was asked to go.
    pub async fn move_chat_to_folder(
        &self,
        chat_id: &str,
        target: &str,
    ) -> Result<Vec<ConversationFolder>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.move_chat_to_folder",
            call
        );
        let bearer = self
            .skype_token
            .as_deref()
            .ok_or(crate::error::CoreError::NotSignedIn)?;
        let folders =
            ost::api::conversation_folder_move_with_client(&self.client, bearer, chat_id, target)
                .instrument(span)
                .await?;
        tracing::debug!(count = folders.len(), call, "moved chat to folder");
        Ok(folders)
    }

    /// The server-side **pinned messages** of `chat_id`, with where they came
    /// from.
    ///
    /// Wraps the vendored `chat_pinned_messages_data`: it reads the chat-service
    /// thread properties (skypetoken) and returns chat-service pins. The Graph
    /// `pinnedMessages` fallback needs `Chat.Read`, which the Teams token lacks
    /// (expected 403 live), so in practice [`PinSource::ChatService`] is what
    /// comes back. A blank/unsafe chat id is rejected before any network.
    pub async fn pinned_messages(&self, chat_id: &str) -> Result<(PinSource, Vec<PinnedRef>)> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.pinned_messages", call);
        let (source, pins) = ost::api::chat_pinned_messages_data(&self.client, chat_id)
            .instrument(span)
            .await?;
        tracing::debug!(count = pins.len(), ?source, call, "listed pinned messages");
        Ok((source, pins))
    }

    /// Unpin one message from `chat_id` on the server.
    ///
    /// Wraps the vendored `chat_unpin_message_with_client`, which **refuses
    /// before any network**: Graph `DELETE /chats/{id}/pinnedMessages/{pinId}`
    /// needs `Chat.ReadWrite`, which the Teams sign-in does not carry, and the
    /// chat-service thread-property write has no verified unpin shape. A
    /// blank/unsafe chat or pin id is rejected first. [`Pins::unpin`](crate::Pins::unpin)
    /// therefore treats the server call as best-effort and still drops the pin
    /// from the local cache.
    pub async fn unpin_message(&self, chat_id: &str, message_id: &str) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.unpin_message", call);
        ost::api::chat_unpin_message_with_client(&self.client, chat_id, message_id)
            .instrument(span)
            .await?;
        tracing::debug!(call, "unpinned message");
        Ok(())
    }

    // ----- B4: team administration ----------------------------------------

    /// Search the directory for public (joinable) teams by name (Graph
    /// `/groups` `$search`, falling back to `/teams` `startswith`).
    ///
    /// **Single window**: the vendored path builder clamps `limit` to Graph's
    /// `1..=PUBLIC_TEAMS_MAX` window and there is no paging cursor, so one call
    /// returns at most that many rows. A blank query is rejected by the vendored
    /// call before any network. The returned source names which Graph surface
    /// answered; the vendored parser keeps only `visibility == public` rows and
    /// does **not** filter out teams already joined (callers mark membership
    /// against their own list — see [`Teams::search_public`](crate::Teams::search_public)).
    pub async fn search_public_teams(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<(PublicTeamsSource, Vec<PublicTeamInfo>)> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.search_public_teams",
            limit,
            call
        );
        let (source, teams) = ost::api::search_public_teams_data(&self.client, query, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = teams.len(), ?source, call, "searched public teams");
        Ok((source, teams))
    }

    /// Join one team by id as the signed-in user (Graph
    /// `POST /teams/{id}/members` for self, resolved through `/me`).
    ///
    /// Returns the trimmed team id on success. A blank/path-breaking id is
    /// rejected before any network. Join-by-invite-code is not Graph and is out
    /// of scope (see the vendored `join_team_data`).
    pub async fn join_team(&self, team_id: &str) -> Result<String> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.join_team", call);
        let id = ost::api::join_team_data(&self.client, team_id)
            .instrument(span)
            .await?;
        tracing::debug!(call, "joined team");
        Ok(id)
    }

    /// Add one user (a Graph/AAD id or UPN) to a team; `owner` grants the
    /// owner role (Graph `POST /teams/{id}/members`).
    ///
    /// Returns the created membership. A blank/path-breaking team id or a blank
    /// user is rejected before any network.
    pub async fn add_team_member(
        &self,
        team_id: &str,
        user_id: &str,
        owner: bool,
    ) -> Result<TeamMemberInfo> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.add_team_member",
            owner,
            call
        );
        let member = ost::api::add_team_member_data(&self.client, team_id, user_id, owner)
            .instrument(span)
            .await?;
        tracing::debug!(call, "added team member");
        Ok(member)
    }

    /// Remove one membership from a team (Graph
    /// `DELETE /teams/{team}/members/{member}`). `membership_id` is the roster
    /// **membership** id, not the user id; a blank/path-breaking id is rejected
    /// before any network.
    pub async fn remove_team_member(&self, team_id: &str, membership_id: &str) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.remove_team_member",
            call
        );
        ost::api::remove_team_member_data(&self.client, team_id, membership_id)
            .instrument(span)
            .await?;
        tracing::debug!(call, "removed team member");
        Ok(())
    }

    /// Read a team's member permissions and General (primary) channel id (two
    /// Graph GETs: `GET /teams/{id}` and `GET /teams/{id}/primaryChannel`).
    ///
    /// The vendored call fails only when **both** reads fail; a single failure
    /// leaves its fields `None` (unknown, never a guessed `false`).
    pub async fn team_settings(&self, team_id: &str) -> Result<TeamMemberSettings> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.team_settings", call);
        let settings = ost::api::team_settings_data(&self.client, team_id)
            .instrument(span)
            .await?;
        tracing::debug!(call, "read team settings");
        Ok(settings)
    }

    /// Set one picker-emoji reaction on a **channel** message.
    ///
    /// This is the dedicated **Graph** channel-reaction lane
    /// (`POST /teams/{team}/channels/{channel}/messages/{message}/setReaction`,
    /// or the `…/replies/{reply}/setReaction` variant when `reply_id` is
    /// `Some`): the chat-service reaction endpoint 404s on channel threads, so
    /// this is *not* the chat path ([`ApiClient::send_reaction`]). The vendored
    /// call needs the **team** id (a Graph group id or a thread id) as well as
    /// the channel, rejects unknown emoji and path-breaking ids before any
    /// network, and answers `204` with no body. `reply_id` targets a thread
    /// reply; `None` targets the root message. **Not verified live** (U-003).
    #[allow(clippy::too_many_arguments)]
    pub async fn set_channel_reaction(
        &self,
        team_id: &str,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
        reply_id: Option<&str>,
    ) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.set_channel_reaction",
            is_reply = reply_id.is_some(),
            call
        );
        ost::api::set_channel_reaction_data(
            &self.client,
            team_id,
            channel_id,
            message_id,
            emoji,
            reply_id,
        )
        .instrument(span)
        .await?;
        tracing::debug!(call, "set channel reaction");
        Ok(())
    }

    /// Remove one picker-emoji reaction from a **channel** message (the Graph
    /// `…/unsetReaction` lane; same arg rules as
    /// [`ApiClient::set_channel_reaction`]).
    #[allow(clippy::too_many_arguments)]
    pub async fn unset_channel_reaction(
        &self,
        team_id: &str,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
        reply_id: Option<&str>,
    ) -> Result<()> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.unset_channel_reaction",
            is_reply = reply_id.is_some(),
            call
        );
        ost::api::unset_channel_reaction_data(
            &self.client,
            team_id,
            channel_id,
            message_id,
            emoji,
            reply_id,
        )
        .instrument(span)
        .await?;
        tracing::debug!(call, "unset channel reaction");
        Ok(())
    }

    // ----- B5: recordings & transcripts (Graph drives) --------------------

    /// List every meeting **recording** the signed-in user can access, newest
    /// first (the vendored global lane: OneDrive `Recordings` + every channel's
    /// SharePoint `Recordings`, merged and clamped to
    /// [`RECORDINGS_MAX_LIMIT`]).
    ///
    /// Wraps the vendored `list_recordings_data`: the OneDrive leg is one
    /// request; each channel contributes a bounded `list_teams_data` scan plus
    /// `filesFolder`/children lookups. A missing `Recordings` folder (404) and a
    /// per-channel failure read as empty; only a total failure (no drive
    /// answered) errors. There is **no** per-chat/per-channel variant in the
    /// vendored crate — [`crate::Recordings`] filters the merged list by source.
    pub async fn list_recordings(&self, limit: usize) -> Result<Vec<RecordingInfo>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.list_recordings",
            limit,
            call
        );
        let rows = ost::api::list_recordings_data(&self.client, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = rows.len(), call, "listed recordings");
        Ok(rows)
    }

    /// Search every meeting **recording** by name (the vendored global lane:
    /// the OneDrive drive-search plus one drive-search per channel drive,
    /// merged, newest first, clamped to [`RECORDINGS_MAX_LIMIT`]). An empty
    /// query is rejected by the vendored call before any network.
    pub async fn search_recordings(&self, query: &str, limit: usize) -> Result<Vec<RecordingInfo>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.search_recordings",
            limit,
            call
        );
        let rows = ost::api::search_recordings_data(&self.client, query, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = rows.len(), call, "searched recordings");
        Ok(rows)
    }

    /// List every meeting **transcript** the signed-in user can access, newest
    /// first (the vendored global lane: the OneDrive + channel `Recordings`
    /// folders, plus a bounded full-drive search fallback when the folder scan
    /// finds zero rows; clamped to [`TRANSCRIPTS_MAX_LIMIT`]).
    ///
    /// Wraps the vendored `list_transcripts_data`. Transcripts are `.vtt` (or a
    /// `transcript`-named `.docx`) driveItems next to the recording. See
    /// [`crate::Recordings`] for the source split.
    pub async fn list_transcripts(&self, limit: usize) -> Result<Vec<TranscriptInfo>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.list_transcripts",
            limit,
            call
        );
        let rows = ost::api::list_transcripts_data(&self.client, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = rows.len(), call, "listed transcripts");
        Ok(rows)
    }

    /// Search every meeting **transcript** by name (the vendored global lane:
    /// the OneDrive drive-search plus one per channel drive, `.vtt`/transcript
    /// filtered, merged and clamped to [`TRANSCRIPTS_MAX_LIMIT`]). An empty
    /// query is rejected by the vendored call before any network.
    pub async fn search_transcripts(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<TranscriptInfo>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(
            target: "squads_core::api",
            "api.search_transcripts",
            limit,
            call
        );
        let rows = ost::api::search_transcripts_data(&self.client, query, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = rows.len(), call, "searched transcripts");
        Ok(rows)
    }
}

impl fmt::Debug for ApiClient {
    /// Redacted: shows routing presence and call count, never token contents.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiClient")
            .field("region_gtms", &!self.region_gtms.is_null())
            .field("chat_service_url", &self.chat_service_url())
            .field("requests", &self.request_count())
            .finish()
    }
}

/// Serve exactly one HTTP request: capture it, reply `200` with `body`.
///
/// Test-only helper shared with the session tests so both drive the same
/// offline path. The captured request head lets callers assert what the client
/// sent (auth header, path) without a live tenant.
#[cfg(test)]
pub(crate) async fn one_shot_json_stub(
    body: String,
) -> (
    String,
    std::sync::Arc<std::sync::Mutex<String>>,
    tokio::task::JoinHandle<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("stub binds");
    let base = format!("http://{}", listener.local_addr().expect("stub addr"));
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let log = std::sync::Arc::clone(&seen);
    let task = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = sock.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if String::from_utf8_lossy(&buf).contains("\r\n\r\n") {
                break;
            }
        }
        *log.lock().unwrap_or_else(|e| e.into_inner()) = String::from_utf8_lossy(&buf).to_string();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        sock.write_all(resp.as_bytes()).await.ok();
        sock.shutdown().await.ok();
    });
    (base, seen, task)
}

/// One scripted HTTP response for [`http_stub`] (test-only).
///
/// `arrive`/`release` optionally turn the stub into a gate: it signals `arrive`
/// when the request has been read, then waits for `release` before replying.
/// That lets a test observe the client's state *while* a request is in flight
/// (the optimistic-send boundary) without any timing or sleeps.
#[cfg(test)]
pub(crate) struct StubResponse {
    /// HTTP status code to reply with.
    pub status: u16,
    /// Optional `Location` header (the server id receipt for sends).
    pub location: Option<String>,
    /// Response body (JSON).
    pub body: String,
    /// Signalled after the request is read, before the reply.
    pub arrive: Option<tokio::sync::oneshot::Sender<()>>,
    /// Awaited before the reply is written.
    pub release: Option<tokio::sync::oneshot::Receiver<()>>,
}

#[cfg(test)]
impl StubResponse {
    /// A `200 OK` JSON response with the given body and no `Location`.
    pub fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            location: None,
            body: body.into(),
            arrive: None,
            release: None,
        }
    }

    /// A `200 OK` response carrying the given `Location` header.
    pub fn ok_location(location: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            status: 200,
            location: Some(location.into()),
            body: body.into(),
            arrive: None,
            release: None,
        }
    }

    /// A response with an arbitrary status (e.g. `500`).
    pub fn status(status: u16) -> Self {
        Self {
            status,
            location: None,
            body: String::new(),
            arrive: None,
            release: None,
        }
    }
}

/// Serve a scripted list of HTTP responses, one connection/request each.
///
/// Returns `(base_url, seen_requests, task)`. Every request (request line,
/// headers **and body**) is recorded verbatim in order, so a test can assert
/// the exact method/path/body the client produced. The task ends after the last
/// response; a client that makes more requests than were scripted gets a
/// connection error.
#[cfg(test)]
pub(crate) async fn http_stub(
    responses: Vec<StubResponse>,
) -> (
    String,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    http_stub_with(move |_base| responses).await
}

/// Like [`http_stub`], but the responses are built from the bound base URL.
///
/// Lets a scripted response embed the stub's own origin (e.g. a
/// `_metadata.backwardLink` that must point back at the same stub).
#[cfg(test)]
pub(crate) async fn http_stub_with<F>(
    build: F,
) -> (
    String,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
)
where
    F: FnOnce(&str) -> Vec<StubResponse>,
{
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("stub binds");
    let base = format!("http://{}", listener.local_addr().expect("stub addr"));
    let responses = build(&base);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = std::sync::Arc::clone(&seen);
    let task = tokio::spawn(async move {
        for response in responses {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let request = read_http_request(&mut sock).await;
            log.lock().unwrap_or_else(|e| e.into_inner()).push(request);
            if let Some(arrive) = response.arrive {
                let _ = arrive.send(());
            }
            if let Some(release) = response.release {
                let _ = release.await;
            }
            let mut head = format!(
                "HTTP/1.1 {} {}\r\n",
                response.status,
                status_text(response.status)
            );
            if let Some(location) = &response.location {
                head.push_str(&format!("Location: {location}\r\n"));
            }
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.body.len()
            ));
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(response.body.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (base, seen, task)
}

/// Read one HTTP request (headers plus a `Content-Length`-sized body) as text.
#[cfg(test)]
async fn read_http_request(sock: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;
    fn find_headers_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = sock.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_headers_end(&buf) {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let content_length = head
                .lines()
                .find_map(|line| {
                    let (k, v) = line.split_once(':')?;
                    if k.eq_ignore_ascii_case("content-length") {
                        v.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let body_start = pos + 4;
            while buf.len() < body_start + content_length {
                let n = sock.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            break;
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

#[cfg(test)]
fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::tokens::{StoredToken, TokenKind};

    fn sample_tokens(region_gtms: serde_json::Value) -> TokenSet {
        TokenSet {
            refresh_token: "refresh-secret".into(),
            tenant: "contoso.onmicrosoft.com".into(),
            region_gtms,
            personal: false,
            spaces: StoredToken::with_expiry("spaces-token", 1_700_000_001),
            skype: StoredToken::with_expiry("skype-token", 1_700_000_002),
            graph: StoredToken::with_expiry("graph-token", 1_700_000_003),
            ic3: StoredToken::with_expiry("ic3-token", 1_700_000_004),
            recorder: StoredToken::with_expiry("recorder-token", 1_700_000_005),
        }
    }

    /// Like [`sample_tokens`] but with live (non-expired) deadlines, for tests
    /// that drive the real transport (which refuses expired tokens).
    fn fresh_tokens(region_gtms: serde_json::Value) -> TokenSet {
        let future = crate::auth::tokens::now_unix() + 3600;
        let mk = |name: &str| StoredToken::with_expiry(name, future);
        TokenSet {
            refresh_token: "refresh-secret".into(),
            tenant: "contoso.onmicrosoft.com".into(),
            region_gtms,
            personal: false,
            spaces: mk("spaces-token"),
            skype: mk("skype-token"),
            graph: mk("graph-token"),
            ic3: mk("ic3-token"),
            recorder: mk("recorder-token"),
        }
    }

    #[test]
    fn tokens_to_config_maps_every_field() {
        let region = serde_json::json!({
            "csaBase": "http://127.0.0.1:9",
            "chatService": "https://emea.ng.msg.teams.microsoft.com",
            "region": "emea"
        });
        let cfg = tokens_to_config(&sample_tokens(region.clone()));

        fn at(t: &Option<ost::auth::StoredToken>) -> &ost::auth::StoredToken {
            t.as_ref().expect("token present")
        }
        // Absolute deadlines are carried through verbatim (not recomputed).
        assert_eq!(at(&cfg.access_token).token, "spaces-token");
        assert_eq!(at(&cfg.access_token).expires_at, Some(1_700_000_001));
        assert_eq!(at(&cfg.skype_token).token, "skype-token");
        assert_eq!(at(&cfg.skype_token).expires_at, Some(1_700_000_002));
        assert_eq!(at(&cfg.graph_token).token, "graph-token");
        assert_eq!(at(&cfg.graph_token).expires_at, Some(1_700_000_003));
        assert_eq!(at(&cfg.ic3_token).token, "ic3-token");
        assert_eq!(at(&cfg.ic3_token).expires_at, Some(1_700_000_004));
        assert_eq!(at(&cfg.recorder_token).token, "recorder-token");
        assert_eq!(at(&cfg.recorder_token).expires_at, Some(1_700_000_005));

        assert_eq!(cfg.refresh_token.as_deref(), Some("refresh-secret"));
        assert_eq!(cfg.tenant_id.as_deref(), Some("contoso.onmicrosoft.com"));

        // The JSON string round-trips back through the vendored accessor.
        assert_eq!(cfg.get_region_gtms(), Some(region));
    }

    #[test]
    fn null_region_gtms_maps_to_none() {
        let cfg = tokens_to_config(&sample_tokens(serde_json::Value::Null));
        assert!(cfg.region_gtms.is_none());
        assert!(cfg.get_region_gtms().is_none());
    }

    #[test]
    fn client_is_offline_until_called_and_debug_is_redacted() {
        let cfg = tokens_to_config(&sample_tokens(serde_json::json!({"region": "emea"})));
        let client = ApiClient::from_config(cfg);
        assert_eq!(client.request_count(), 0);
        assert_eq!(client.region_gtms(), &serde_json::json!({"region": "emea"}));
        // Routing accessors fall back to the regional defaults.
        assert!(
            client
                .chat_service_url()
                .contains("ng.msg.teams.microsoft.com")
        );
        let dbg = format!("{client:?}");
        assert!(!dbg.contains("spaces-token") && !dbg.contains("graph-token"));
    }

    #[test]
    fn region_gtms_can_override_service_urls() {
        let cfg = tokens_to_config(&sample_tokens(serde_json::json!({
            "chatService": "http://127.0.0.1:1234/chat",
            "chatServiceAggregator": "http://127.0.0.1:1234/agg",
            "middleTier": "http://127.0.0.1:1234/mt"
        })));
        let client = ApiClient::from_config(cfg);
        assert_eq!(client.chat_service_url(), "http://127.0.0.1:1234/chat");
        assert_eq!(client.chatsvcagg_url(), "http://127.0.0.1:1234/agg");
        assert_eq!(client.middle_tier_url(), "http://127.0.0.1:1234/mt");
    }

    /// The M2 exit criterion: `list_chats` returns structured data from a local
    /// fake. Chats carry a `topic`, so the mate-name resolution (which would add
    /// a `whoami` + roster + history request) is never entered and the stub
    /// only has to answer the CSA conversations request.
    #[tokio::test]
    async fn list_chats_against_local_fake_returns_parsed_chats() {
        let body = serde_json::json!({
            "conversations": [
                {
                    "id": "19:alice@unq.gbl.spaces",
                    "threadProperties": { "topic": "Alice" },
                    "lastMessage": {
                        "imdisplayname": "Alice",
                        "content": "<p>hello <b>world</b></p>",
                        "originalarrivaltime": "2026-01-02T03:04:05Z"
                    }
                },
                {
                    "id": "19:team@thread.v2",
                    "threadProperties": { "topic": "Team sync" },
                    "lastMessage": {
                        "imdisplayname": "Bob",
                        "content": "<p>standup at 5</p>",
                        "originalarrivaltime": "2026-01-03T03:04:05Z"
                    }
                }
            ]
        })
        .to_string();
        let (base, seen, task) = one_shot_json_stub(body).await;

        let tokens = fresh_tokens(serde_json::json!({
            "csaBase": base,
            "graphBase": base,
            "chatService": base
        }));
        let client = ApiClient::from_tokens(&tokens);
        let chats = client.list_chats(20).await.expect("list chats");

        assert_eq!(chats.len(), 2, "two conversations parsed");
        assert_eq!(chats[0].id, "19:alice@unq.gbl.spaces");
        assert_eq!(chats[0].name, "Alice");
        assert!(!chats[0].is_group);
        assert_eq!(chats[0].last_message_sender.as_deref(), Some("Alice"));
        assert_eq!(
            chats[0].last_message_preview.as_deref(),
            Some("hello world")
        );
        assert_eq!(
            chats[0].last_message_time.as_deref(),
            Some("2026-01-02T03:04:05Z")
        );
        assert_eq!(chats[1].name, "Team sync");
        assert!(chats[1].is_group, "group thread id contains 'thread'");
        assert_eq!(client.request_count(), 1);

        task.await.expect("stub drains");
        let req = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            req.starts_with("GET /teams/users/ME/conversations?view=mychats&pageSize=20 "),
            "unexpected request line: {req}"
        );
        // The skype token from our set reached the transport.
        assert!(req.contains("authorization: Bearer skype-token"), "{req}");
    }

    #[tokio::test]
    async fn read_messages_against_local_fake_returns_parsed_messages() {
        let body = serde_json::json!({
            "messages": [
                {
                    "id": "1700000000000",
                    "imdisplayname": "Alice",
                    "content": "<p>hi</p>",
                    "messagetype": "RichText/Html",
                    "originalarrivaltime": "2026-01-02T03:04:05Z",
                    "from": "https://teams.microsoft.com/api/v1/users/ME/contacts/8:orgid:aaaa"
                }
            ],
            "_metadata": { "backwardLink": null }
        })
        .to_string();
        let (base, _seen, task) = one_shot_json_stub(body).await;

        let tokens = fresh_tokens(serde_json::json!({ "chatService": base }));
        let client = ApiClient::from_tokens(&tokens);
        let msgs = client
            .read_messages("19:alice@unq.gbl.spaces", 50)
            .await
            .expect("read messages");

        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].id, "1700000000000");
        assert_eq!(msgs[0].sender, "Alice");
        assert_eq!(msgs[0].sender_mri, "8:orgid:aaaa");
        assert_eq!(msgs[0].content, "hi");
        task.await.expect("stub drains");
    }

    #[test]
    fn stored_token_kind_values_are_distinct() {
        // Guard against a copy/paste regression in `tokens_to_config`: every
        // kind must map to its own token value.
        let set = sample_tokens(serde_json::json!({"r": 1}));
        let cfg = tokens_to_config(&set);
        let mut seen = std::collections::HashSet::new();
        for kind in TokenKind::ALL {
            let value = set.token(kind).token.clone();
            assert!(seen.insert(value.clone()), "duplicate token {value}");
        }
        assert_eq!(
            cfg.access_token.as_ref().map(|t| t.token.as_str()),
            Some("spaces-token")
        );
    }

    // ----- M13: identity & inline media -----------------------------------

    #[tokio::test]
    async fn whoami_maps_to_an_identity_with_the_derived_mri() {
        let body = serde_json::json!({
            "id": "abc-123",
            "displayName": "Alice Smith",
            "mail": "alice@contoso.com",
            "userPrincipalName": "alice@contoso.com"
        })
        .to_string();
        let (base, seen, task) = one_shot_json_stub(body).await;

        let tokens = fresh_tokens(serde_json::json!({ "graphBase": base }));
        let client = ApiClient::from_tokens(&tokens);
        let info = client.whoami().await.expect("whoami");
        assert_eq!(info.id, "abc-123");
        assert_eq!(info.display_name, "Alice Smith");
        assert_eq!(info.mail.as_deref(), Some("alice@contoso.com"));
        assert_eq!(client.request_count(), 1);

        let identity = crate::model::Identity::from_whoami(&info, tokens.tenant());
        assert_eq!(identity.mri, "8:orgid:abc-123");
        assert_eq!(identity.oid, "abc-123");
        assert_eq!(identity.display_name, "Alice Smith");
        assert_eq!(identity.tenant, "contoso.onmicrosoft.com");

        task.await.expect("stub drains");
        let req = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            req.starts_with("GET /me "),
            "unexpected request line: {req}"
        );
        // The Graph token, not the skype token, authorises `/me`.
        assert!(req.contains("authorization: Bearer graph-token"), "{req}");
    }

    #[test]
    fn own_member_mri_wrapper_matches_the_vendored_shape() {
        assert_eq!(own_member_mri("abc-123"), "8:orgid:abc-123");
        assert_eq!(own_member_mri("  abc-123  "), "8:orgid:abc-123");
    }

    #[test]
    fn media_host_rule_sends_the_token_only_to_microsoft_hosts() {
        // Microsoft media hosts need auth (and get headers).
        assert!(media_needs_auth(
            "https://us-api.asm.skype.com/v1/objects/0/x"
        ));
        assert!(media_needs_auth(
            "https://amer.ng.msg.teams.microsoft.com/v1/objects/0/x"
        ));
        assert!(!media_auth_headers("https://us-api.asm.skype.com/x", "T").is_empty());
        // Public hosts send no token.
        assert!(!media_needs_auth(
            "https://media.giphy.com/media/abc/giphy.gif"
        ));
        assert!(media_auth_headers("https://media.giphy.com/abc.gif", "T").is_empty());
        // An evil look-alike never gets the token.
        assert!(media_auth_headers("https://asm.skype.com.evil.com/x", "T").is_empty());
    }

    // ----- B2a: chat management -------------------------------------------

    /// The AAD object id `aad_object_id` resolves "me" to in the 1:1/group
    /// stubs (the peer is already a GUID, so it needs no lookup).
    const ME_GUID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    const PEER_GUID: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

    fn b2a_tokens(base: &str) -> TokenSet {
        fresh_tokens(serde_json::json!({
            "graphBase": base,
            "chatService": base
        }))
    }

    fn b2a_requests(seen: &std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> Vec<String> {
        seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    #[tokio::test]
    async fn create_one_to_one_reopens_the_derived_thread() {
        // `GET /me?$select=id` resolves "me"; the derived thread probe answers
        // 200, so the existing 1:1 re-opens with no create.
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(serde_json::json!({ "id": ME_GUID }).to_string()),
            StubResponse::ok("{}"),
        ])
        .await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        let chat = client
            .create_one_to_one_chat(PEER_GUID)
            .await
            .expect("create 1:1");
        // The derived id is the sorted, lowercased pair.
        assert_eq!(chat.id, format!("19:{ME_GUID}_{PEER_GUID}@unq.gbl.spaces"));
        assert!(!chat.is_group);
        assert_eq!(client.request_count(), 1, "one high-level call");

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 2, "resolve me, then probe the thread");
        assert!(
            reqs[0].starts_with("GET /me?$select=id "),
            "unexpected request: {}",
            reqs[0]
        );
        assert!(
            reqs[0].contains("authorization: Bearer graph-token"),
            "{}",
            reqs[0]
        );
        assert!(
            reqs[1].starts_with(&format!(
                "GET /v1/threads/19:{ME_GUID}_{PEER_GUID}@unq.gbl.spaces?view=msnp24Equivalent "
            )),
            "unexpected request: {}",
            reqs[1]
        );
        assert!(
            reqs[1].contains("authentication: skypetoken=skype-token"),
            "{}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn create_one_to_one_creates_a_missing_thread() {
        // Probe 404 → create on the chat service; the Location echoes the
        // derived thread.
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(serde_json::json!({ "id": ME_GUID }).to_string()),
            StubResponse::status(404),
            StubResponse::status(201),
        ])
        .await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        let chat = client
            .create_one_to_one_chat(PEER_GUID)
            .await
            .expect("create 1:1");
        assert_eq!(chat.id, format!("19:{ME_GUID}_{PEER_GUID}@unq.gbl.spaces"));

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 3, "resolve me, probe, create");
        assert!(
            reqs[2].starts_with("POST /v1/threads "),
            "unexpected request: {}",
            reqs[2]
        );
        // The create body carries both orgid members and the unique-roster
        // properties.
        assert!(
            reqs[2].contains(&format!("8:orgid:{ME_GUID}")),
            "{}",
            reqs[2]
        );
        assert!(
            reqs[2].contains(&format!("8:orgid:{PEER_GUID}")),
            "{}",
            reqs[2]
        );
        assert!(
            reqs[2].contains("\"uniquerosterthread\":\"true\""),
            "{}",
            reqs[2]
        );
    }

    #[tokio::test]
    async fn create_group_posts_members_and_topic_and_reads_the_location() {
        let location = format!("/v1/threads/19%3A{ME_GUID}_{PEER_GUID}%40thread.v2?foo=1");
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(serde_json::json!({ "id": ME_GUID }).to_string()),
            StubResponse::ok_location(location, "{}"),
        ])
        .await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        let chat = client
            .create_group_chat(Some("Launch"), &[PEER_GUID.to_string()])
            .await
            .expect("create group");
        assert!(chat.is_group);
        assert_eq!(chat.name, "Launch");
        assert_eq!(chat.id, format!("19:{ME_GUID}_{PEER_GUID}@thread.v2"));

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 2, "resolve me, then create");
        assert!(
            reqs[1].starts_with("POST /v1/threads "),
            "unexpected request: {}",
            reqs[1]
        );
        // The creator first, then the peer, all Admin; the topic is carried.
        assert!(
            reqs[1].contains(&format!("8:orgid:{ME_GUID}")),
            "{}",
            reqs[1]
        );
        assert!(
            reqs[1].contains(&format!("8:orgid:{PEER_GUID}")),
            "{}",
            reqs[1]
        );
        assert!(reqs[1].contains("\"role\":\"Admin\""), "{}", reqs[1]);
        assert!(reqs[1].contains("\"topic\":\"Launch\""), "{}", reqs[1]);
    }

    #[tokio::test]
    async fn leave_deletes_the_own_member_from_the_roster() {
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(serde_json::json!({ "id": ME_GUID }).to_string()),
            StubResponse::status(204),
        ])
        .await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        client
            .leave_chat("19:team@thread.v2")
            .await
            .expect("leave chat");

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 2, "whoami then delete");
        assert!(
            reqs[1].starts_with(&format!(
                "DELETE /v1/threads/19:team@thread.v2/members/8:orgid:{ME_GUID} "
            )),
            "unexpected request: {}",
            reqs[1]
        );
        assert!(
            reqs[1].contains("authentication: skypetoken=skype-token"),
            "{}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn set_chat_muted_writes_the_alerts_property() {
        let (base, seen, task) =
            http_stub(vec![StubResponse::status(200), StubResponse::status(200)]).await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        client
            .set_chat_muted("19:team@thread.v2", true)
            .await
            .expect("mute");
        client
            .set_chat_muted("19:team@thread.v2", false)
            .await
            .expect("unmute");

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 2);
        assert!(
            reqs[0].starts_with(
                "PUT /v1/users/ME/conversations/19:team@thread.v2/properties?name=alerts "
            ),
            "unexpected request: {}",
            reqs[0]
        );
        // Muted chats carry `"false"` (the server stores strings).
        assert!(reqs[0].contains("\"alerts\":\"false\""), "{}", reqs[0]);
        assert!(reqs[1].contains("\"alerts\":\"true\""), "{}", reqs[1]);
    }

    #[tokio::test]
    async fn set_chat_hidden_writes_the_visibility_properties() {
        let (base, seen, task) = http_stub(vec![
            StubResponse::status(200),
            StubResponse::status(200),
            StubResponse::status(200),
        ])
        .await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        client
            .set_chat_hidden("19:team@thread.v2", true)
            .await
            .expect("hide");
        client
            .set_chat_hidden("19:team@thread.v2", false)
            .await
            .expect("unhide");

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 3, "hide writes two properties, unhide one");
        assert!(
            reqs[0].starts_with(
                "PUT /v1/users/ME/conversations/19:team@thread.v2/properties?name=unpinnedTime "
            ),
            "unexpected request: {}",
            reqs[0]
        );
        assert!(reqs[0].contains("\"unpinnedTime\":"), "{}", reqs[0]);
        assert!(
            reqs[1].starts_with(
                "PUT /v1/users/ME/conversations/19:team@thread.v2/properties?name=historyHiddenTime "
            ),
            "unexpected request: {}",
            reqs[1]
        );
        // Unhide writes a null `unpinnedTime` and nothing else.
        assert!(reqs[2].contains("\"unpinnedTime\":null"), "{}", reqs[2]);
    }

    #[tokio::test]
    async fn chat_members_parses_the_thread_roster() {
        let body = serde_json::json!({
            "members": [
                {"id": format!("8:orgid:{ME_GUID}"), "role": "Admin", "friendlyName": "Me"},
                {"id": format!("8:orgid:{PEER_GUID}"), "role": "User", "friendlyName": "Ava"}
            ]
        })
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        let members = client
            .chat_members("19:team@thread.v2")
            .await
            .expect("roster");
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].mri, format!("8:orgid:{ME_GUID}"));
        assert_eq!(members[0].user_id.as_deref(), Some(ME_GUID));
        assert_eq!(members[0].display_name, "Me");
        assert!(members[0].is_owner, "Admin is the owner");
        assert_eq!(members[1].display_name, "Ava");
        assert!(!members[1].is_owner);

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 1, "a named roster needs no Graph fill");
        assert!(
            reqs[0].starts_with("GET /v1/threads/19:team@thread.v2/members "),
            "unexpected request: {}",
            reqs[0]
        );
    }

    #[test]
    fn created_chat_parsing_pins_the_vendored_shapes() {
        // The 1:1 Graph `POST /me/chats` response (no topic → empty name).
        let one = parse_created_chat(&serde_json::json!({ "id": "19:one@unq.gbl.spaces" }))
            .expect("parse 1:1");
        assert_eq!(one.id, "19:one@unq.gbl.spaces");
        assert_eq!(one.name, "");
        assert!(!one.is_group);
        let named =
            parse_created_chat(&serde_json::json!({ "id": "19:g@thread.v2", "topic": "T" }))
                .expect("parse group");
        assert_eq!(named.name, "T");

        // The thread id from a chat-service create `Location`.
        assert_eq!(
            created_thread_id(Some(
                "https://emea.ng.msg.teams.microsoft.com/v1/threads/19%3Aabc%40thread.v2?x=1"
            ))
            .as_deref(),
            Some("19:abc@thread.v2")
        );
        assert_eq!(created_thread_id(Some("https://x/notathread")), None);
        assert_eq!(created_thread_id(None), None);
    }

    #[test]
    fn chat_write_pure_helpers_pin_the_request_shapes() {
        assert_eq!(one_to_one_create_path(), "/me/chats");
        let body = one_to_one_create_body(PEER_GUID);
        assert_eq!(body["chatType"], "oneOnOne");
        assert_eq!(
            body["members"][0]["user@odata.bind"],
            format!("https://graph.microsoft.com/v1.0/users('{PEER_GUID}')")
        );
        assert_eq!(
            chat_members_path("19:x@thread.v2"),
            "/chats/19:x@thread.v2/members"
        );
        assert_eq!(
            leave_member_url("https://chat", "19:x", "8:orgid:me"),
            "https://chat/v1/threads/19:x/members/8:orgid:me"
        );
        assert_eq!(
            alerts_url("https://chat", "19:x"),
            "https://chat/v1/users/ME/conversations/19:x/properties?name=alerts"
        );
        assert_eq!(alerts_body(true)["alerts"], "false");
        assert_eq!(alerts_body(false)["alerts"], "true");
        assert_eq!(
            alerts_muted(&serde_json::json!({ "alerts": "false" })),
            Some(true)
        );
        assert_eq!(
            alerts_muted(&serde_json::json!({ "alerts": "true" })),
            Some(false)
        );
        assert_eq!(alerts_muted(&serde_json::json!({})), None);
        // Hide writes `unpinnedTime` + `historyHiddenTime`; unhide only nulls
        // `unpinnedTime`.
        let hide = hide_chat_properties(Some(1234));
        assert_eq!(hide.len(), 2);
        assert_eq!(hide[1].0, "historyHiddenTime");
        assert_eq!(hide[1].1["historyHiddenTime"], "1234");
        let unhide = hide_chat_properties(None);
        assert_eq!(unhide.len(), 1);
        assert!(unhide[0].1["unpinnedTime"].is_null());
    }

    // ----- B2b: folders & pinned messages ---------------------------------

    #[tokio::test]
    async fn conversation_folders_requires_a_token_and_makes_no_request() {
        // A client with no skype token (the folder lane's bearer) is refused
        // before any network — the vendored call needs the bearer explicitly.
        let client = ApiClient::from_config(Config::default());
        let err = client
            .conversation_folders()
            .await
            .err()
            .expect("no token = refused");
        assert!(
            matches!(err, crate::error::CoreError::NotSignedIn),
            "expected NotSignedIn, got {err:?}"
        );
    }

    #[tokio::test]
    async fn pinned_messages_reads_the_chat_service_thread_properties() {
        // The chat-service thread GET: a `properties` key whose name contains
        // "pinned" carries the pin objects (sender/preview/time present, so the
        // vendored fill-from-message pass is skipped).
        let body = serde_json::json!({
            "properties": {
                "pinnedMessages": [
                    {
                        "messageId": "1700000000000",
                        "sender": "Ava",
                        "content": "<p>hello <b>there</b></p>",
                        "originalarrivaltime": "2026-01-02T03:04:05Z",
                        "pinnedBy": "Bo",
                        "pinnedTime": "2026-01-03T00:00:00Z"
                    }
                ]
            }
        })
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        let (source, pins) = client
            .pinned_messages("19:team@thread.v2")
            .await
            .expect("pins");
        assert_eq!(source, PinSource::ChatService);
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].message_id, "1700000000000");
        assert_eq!(pins[0].sender.as_deref(), Some("Ava"));
        assert_eq!(pins[0].preview.as_deref(), Some("hello there"));
        assert_eq!(pins[0].time.as_deref(), Some("2026-01-02T03:04:05Z"));
        assert_eq!(pins[0].pinned_by.as_deref(), Some("Bo"));
        assert_eq!(pins[0].pinned_at.as_deref(), Some("2026-01-03T00:00:00Z"));

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 1, "preview present, so no fill request");
        assert!(
            reqs[0].starts_with("GET /v1/threads/19:team@thread.v2?view=msnp24Equivalent "),
            "unexpected request: {}",
            reqs[0]
        );
        assert!(
            reqs[0].contains("authentication: skypetoken=skype-token"),
            "{}",
            reqs[0]
        );
    }

    #[tokio::test]
    async fn unpin_refuses_before_any_network() {
        let (base, seen, task) = http_stub(vec![]).await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        // The Teams sign-in cannot unpin: the vendored call refuses first.
        let err = client
            .unpin_message("19:team@thread.v2", "1700000000000")
            .await
            .err()
            .expect("unpin refused");
        assert!(format!("{err:#}").contains("not available"), "{err:#}");
        // A blank/unsafe id is rejected too.
        let bad = client
            .unpin_message("19:team@thread.v2", "a/b")
            .await
            .err()
            .expect("bad pin id");
        assert!(format!("{bad:#}").contains("bad chat or pin id"), "{bad:#}");

        task.await.expect("stub drains");
        assert!(
            b2a_requests(&seen).is_empty(),
            "no request of any kind reaches the wire"
        );
    }

    #[test]
    fn folder_pure_helpers_pin_the_request_shapes() {
        // The folder list URL is the CSA host with both flags the web client
        // sends.
        let url = conversation_folders_url();
        assert!(url.contains("/conversationFolders?"), "{url}");
        assert!(
            url.contains("supportsAdditionalSystemGeneratedFolders=true"),
            "{url}"
        );

        // Parsing drops deleted + system-view folders, keeps Favorites and user
        // folders, and honours `conversationFolderOrder`.
        let payload = serde_json::json!({
            "folderHierarchyVersion": 7,
            "conversationFolderOrder": ["f1", "t~u~Favorites"],
            "conversationFolders": [
                {"id": "t~u~Favorites", "folderType": "Favorites",
                 "conversationFolderItems": [{"conversationId": "48:notes"}]},
                {"id": "f1", "name": "Work", "folderType": "UserCreated",
                 "conversationFolderItems": [{"conversationId": "19:a@thread.v2"}]},
                {"id": "q", "folderType": "QuickViews",
                 "conversationFolderItems": [{"conversationId": "48:notes"}]},
                {"id": "gone", "folderType": "UserCreated", "isDeleted": true,
                 "conversationFolderItems": []}
            ]
        });
        let folders = parse_conversation_folders(&payload);
        assert_eq!(
            folders.len(),
            2,
            "the QuickViews view and the deleted folder are dropped"
        );
        assert_eq!(folders[0].id, "f1", "the order list wins");
        assert_eq!(folders[1].id, "t~u~Favorites");
        assert!(parse_conversation_folders(&serde_json::json!({})).is_empty());

        // The move computes the edits that leave `48:notes` in f1 only.
        let (version, actions) = folder_move_actions(&payload, "48:notes", "f1").unwrap();
        assert_eq!(version, 7);
        assert!(actions.contains(&(
            "RemoveItem",
            "t~u~Favorites".to_string(),
            "48:notes".to_string()
        )));
        assert!(actions.contains(&("AddItem", "f1".to_string(), "48:notes".to_string())));
        // Moving to the folder it is already in is a no-op; an unknown target
        // is an error.
        assert!(
            folder_move_actions(&payload, "48:notes", "t~u~Favorites")
                .unwrap()
                .1
                .is_empty()
        );
        assert!(folder_move_actions(&payload, "48:notes", "nope").is_err());
        // A blank target removes the chat from every movable folder.
        let (_, out) = folder_move_actions(&payload, "48:notes", "").unwrap();
        assert_eq!(
            out,
            vec![(
                "RemoveItem",
                "t~u~Favorites".to_string(),
                "48:notes".to_string()
            )]
        );

        // The POST body carries the hierarchy version and the actions.
        let body = folder_move_body(7, &[("AddItem", "f1".into(), "48:notes".into())]);
        assert_eq!(body["folderHierarchyVersion"], 7);
        assert_eq!(body["actions"][0]["action"], "AddItem");
        assert_eq!(body["actions"][0]["folderId"], "f1");
        assert_eq!(body["actions"][0]["itemId"], "48:notes");
    }

    #[test]
    fn pin_parsing_handles_the_graph_and_thread_shapes() {
        // Graph `pinnedMessages?$expand=message`: the message id, sender and
        // preview come from the expanded message; the pin id is the DELETE key.
        let graph = serde_json::json!({
            "value": [
                {"id": "pin-1", "message": {
                    "id": "1700000000000",
                    "createdDateTime": "2026-01-02T03:04:05Z",
                    "from": {"user": {"displayName": "Ava"}},
                    "body": {"content": "<p>hello <b>there</b></p>"}
                }},
                {"id": "pin-2", "message": {"id": "1700000000001", "deletedDateTime": "2026-01-01T00:00:00Z"}}
            ]
        });
        let pins = parse_graph_pins(&graph).expect("graph pins");
        assert_eq!(pins.len(), 1, "the deleted message is skipped");
        assert_eq!(pins[0].message_id, "1700000000000");
        assert_eq!(pins[0].sender.as_deref(), Some("Ava"));
        assert_eq!(pins[0].preview.as_deref(), Some("hello there"));
        assert_eq!(pins[0].graph_pin_id.as_deref(), Some("pin-1"));
        // A Graph error body is an error, not an empty list.
        assert!(parse_graph_pins(&serde_json::json!({"error": {"code": "Forbidden"}})).is_err());

        // Thread pins: bare comma-separated ids and object entries both parse;
        // duplicates collapse.
        let thread = serde_json::json!({
            "properties": {
                "pinnedMessageIds": "1700000000000,1700000000001",
                "somePinnedThing": [{"messageId": "1700000000000", "sender": "Ava"}]
            }
        });
        let pins = parse_thread_pins(&thread);
        let ids: Vec<_> = pins.iter().map(|p| p.message_id.as_str()).collect();
        assert_eq!(ids, vec!["1700000000000", "1700000000001"]);
        assert!(parse_thread_pins(&serde_json::json!({})).is_empty());

        // The source precedence: non-empty chat-service pins win, else Graph.
        let (source, _) = pick_pins(
            Ok(vec![PinnedRef {
                message_id: "1".into(),
                ..Default::default()
            }]),
            None,
        )
        .unwrap();
        assert_eq!(source, PinSource::ChatService);
        let (source, _) = pick_pins(
            Ok(vec![]),
            Some(Ok(vec![PinnedRef {
                message_id: "2".into(),
                ..Default::default()
            }])),
        )
        .unwrap();
        assert_eq!(source, PinSource::Graph);
    }

    // ----- B4: team administration ----------------------------------------

    #[tokio::test]
    async fn search_public_teams_reads_groups_and_keeps_only_public() {
        let body = serde_json::json!({
            "value": [
                {"id": "t-public", "displayName": "Public Eng", "description": "Open",
                 "visibility": "public"},
                {"id": "t-private", "displayName": "Secret", "visibility": "private"}
            ]
        })
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        let (source, teams) = client
            .search_public_teams("eng", 25)
            .await
            .expect("search public teams");
        assert_eq!(source, PublicTeamsSource::Groups);
        assert_eq!(teams.len(), 1, "the private team is dropped");
        assert_eq!(teams[0].id, "t-public");
        assert_eq!(teams[0].name, "Public Eng");
        assert_eq!(teams[0].visibility, "public");

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 1);
        assert!(
            reqs[0].starts_with("GET /groups?$search="),
            "unexpected request: {}",
            reqs[0]
        );
        assert!(reqs[0].contains("$count=true"), "{}", reqs[0]);
        assert!(reqs[0].contains("$top=25"), "{}", reqs[0]);
        assert!(
            reqs[0].contains("authorization: Bearer graph-token"),
            "{}",
            reqs[0]
        );
    }

    #[tokio::test]
    async fn search_public_teams_falls_back_to_teams_startswith() {
        let teams_body = serde_json::json!({
            "value": [{"id": "t1", "displayName": "Eng", "visibility": "public"}]
        })
        .to_string();
        let (base, seen, task) = http_stub(vec![
            StubResponse::status(403),
            StubResponse::ok(teams_body),
        ])
        .await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        let (source, teams) = client
            .search_public_teams("eng", 25)
            .await
            .expect("search public teams");
        assert_eq!(source, PublicTeamsSource::Teams);
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].id, "t1");

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 2, "groups refused, then /teams");
        assert!(reqs[0].starts_with("GET /groups?$search="), "{}", reqs[0]);
        assert!(
            reqs[1].starts_with("GET /teams?$filter="),
            "unexpected request: {}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn search_public_teams_rejects_a_blank_query_before_any_network() {
        let (base, seen, task) = http_stub(vec![]).await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        let err = client
            .search_public_teams("   ", 25)
            .await
            .err()
            .expect("blank query refused");
        assert!(format!("{err:#}").contains("empty query"), "{err:#}");

        task.await.expect("stub drains");
        assert!(b2a_requests(&seen).is_empty());
    }

    #[tokio::test]
    async fn join_team_adds_self_as_a_member() {
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(serde_json::json!({ "id": ME_GUID }).to_string()),
            StubResponse::status(201),
        ])
        .await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        let id = client.join_team(" team-1 ").await.expect("join team");
        assert_eq!(id, "team-1", "the trimmed id comes back");

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 2, "whoami then the membership POST");
        assert!(reqs[0].starts_with("GET /me "), "{}", reqs[0]);
        assert!(
            reqs[1].starts_with("POST /teams/team-1/members "),
            "unexpected request: {}",
            reqs[1]
        );
        assert!(
            reqs[1].contains(&format!("users('{ME_GUID}')")),
            "the body binds the resolved self: {}",
            reqs[1]
        );
        assert!(reqs[1].contains("\"roles\":[]"), "{}", reqs[1]);
    }

    #[tokio::test]
    async fn add_team_member_posts_the_owner_body_and_parses_the_membership() {
        let body = serde_json::json!({
            "id": "M9", "displayName": "Ava", "userId": "oid-9",
            "email": "ava@x.example", "roles": ["owner"]
        })
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        let member = client
            .add_team_member("team-1", "ava@x.example", true)
            .await
            .expect("add member");
        assert_eq!(member.id, "M9");
        assert_eq!(member.display_name, "Ava");
        assert!(member.is_owner);

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 1);
        assert!(
            reqs[0].starts_with("POST /teams/team-1/members "),
            "unexpected request: {}",
            reqs[0]
        );
        assert!(reqs[0].contains("\"roles\":[\"owner\"]"), "{}", reqs[0]);
        assert!(reqs[0].contains("users('ava@x.example')"), "{}", reqs[0]);
    }

    #[tokio::test]
    async fn remove_team_member_deletes_the_membership() {
        let (base, seen, task) = http_stub(vec![StubResponse::status(204)]).await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        client
            .remove_team_member("team-1", "M9")
            .await
            .expect("remove member");

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 1);
        assert!(
            reqs[0].starts_with("DELETE /teams/team-1/members/M9 "),
            "unexpected request: {}",
            reqs[0]
        );
    }

    #[tokio::test]
    async fn team_settings_reads_the_team_and_its_primary_channel() {
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(
                serde_json::json!({
                    "id": "team-1",
                    "memberSettings": {
                        "allowCreateUpdateChannels": true,
                        "allowDeleteChannels": false
                    }
                })
                .to_string(),
            ),
            StubResponse::ok(serde_json::json!({ "id": "19:general@thread.tacv2" }).to_string()),
        ])
        .await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        let settings = client.team_settings("team-1").await.expect("team settings");
        assert_eq!(settings.allow_create_update_channels, Some(true));
        assert_eq!(settings.allow_delete_channels, Some(false));
        assert_eq!(
            settings.primary_channel_id.as_deref(),
            Some("19:general@thread.tacv2")
        );

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 2);
        assert!(reqs[0].starts_with("GET /teams/team-1 "), "{}", reqs[0]);
        assert!(
            reqs[1].starts_with("GET /teams/team-1/primaryChannel "),
            "unexpected request: {}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn team_settings_keeps_a_single_failure_unknown() {
        // The team read 200s (flags set) but primaryChannel 403s: the flags
        // survive and the channel id stays unknown — never a guess.
        let (base, _seen, task) = http_stub(vec![
            StubResponse::ok(
                serde_json::json!({
                    "id": "team-1",
                    "memberSettings": { "allowDeleteChannels": true }
                })
                .to_string(),
            ),
            StubResponse::status(403),
        ])
        .await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        let settings = client.team_settings("team-1").await.expect("team settings");
        assert_eq!(settings.allow_delete_channels, Some(true));
        assert_eq!(settings.allow_create_update_channels, None);
        assert_eq!(settings.primary_channel_id, None);
        task.await.expect("stub drains");
    }

    #[tokio::test]
    async fn channel_reactions_use_the_graph_lane_for_root_and_reply() {
        let (base, seen, task) = http_stub(vec![
            StubResponse::status(204),
            StubResponse::status(204),
            StubResponse::status(204),
        ])
        .await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        // Add on the root, add on a reply, then remove from the root.
        client
            .set_channel_reaction("team-1", "19:ch@thread.tacv2", "m1", "👍", None)
            .await
            .expect("react root");
        client
            .set_channel_reaction("team-1", "19:ch@thread.tacv2", "m1", "❤️", Some("r2"))
            .await
            .expect("react reply");
        client
            .unset_channel_reaction("team-1", "19:ch@thread.tacv2", "m1", "👍", None)
            .await
            .expect("unreact root");

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 3);
        assert!(
            reqs[0].starts_with(
                "POST /teams/team-1/channels/19:ch@thread.tacv2/messages/m1/setReaction "
            ),
            "unexpected request: {}",
            reqs[0]
        );
        // The body carries the emoji itself (Graph takes the unicode type).
        assert!(reqs[0].contains("\"reactionType\":\"👍\""), "{}", reqs[0]);
        assert!(
            reqs[1].starts_with(
                "POST /teams/team-1/channels/19:ch@thread.tacv2/messages/m1/replies/r2/setReaction "
            ),
            "unexpected request: {}",
            reqs[1]
        );
        assert!(
            reqs[2].starts_with(
                "POST /teams/team-1/channels/19:ch@thread.tacv2/messages/m1/unsetReaction "
            ),
            "unexpected request: {}",
            reqs[2]
        );
        // The channel lane is Graph-authenticated, never the skype token.
        assert!(
            reqs[0].contains("authorization: Bearer graph-token"),
            "{}",
            reqs[0]
        );
        assert!(!reqs[0].contains("skypetoken"), "{}", reqs[0]);
    }

    #[tokio::test]
    async fn channel_reaction_rejects_an_unknown_emoji_before_any_network() {
        let (base, seen, task) = http_stub(vec![]).await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        let err = client
            .set_channel_reaction("team-1", "19:ch@thread.tacv2", "m1", "🎉", None)
            .await
            .err()
            .expect("unknown emoji refused");
        assert!(
            format!("{err:#}").contains("unsupported reaction"),
            "{err:#}"
        );

        task.await.expect("stub drains");
        assert!(b2a_requests(&seen).is_empty());
    }

    #[test]
    fn b4_pure_helpers_pin_the_request_shapes() {
        // Public-team parsing keeps only public rows, dedupes ids and falls
        // back to the id when the name is blank.
        let payload = serde_json::json!({
            "value": [
                {"id": "a", "displayName": "A", "visibility": "public"},
                {"id": "a", "displayName": "A again", "visibility": "public"},
                {"id": "b", "visibility": "Public"},
                {"id": "c", "displayName": "C", "visibility": "hiddenmembership"},
                {"displayName": "no id", "visibility": "public"}
            ]
        });
        let teams = parse_public_teams(&payload);
        assert_eq!(teams.len(), 2);
        assert_eq!(teams[0].id, "a");
        assert_eq!(teams[1].id, "b");
        assert_eq!(teams[1].name, "b", "blank name falls back to the id");
        assert!(parse_public_teams(&serde_json::json!({})).is_empty());

        // The two search paths are distinct Graph shapes.
        assert!(public_teams_groups_path("eng", 99).starts_with("/groups?$search="));
        assert!(public_teams_groups_path("eng", 99).contains("$top=25"));
        assert!(public_teams_list_path("eng", 0).starts_with("/teams?$filter="));

        // Member add body (owner flag) and the reaction paths/body.
        let body = add_member_body("oid-1", true);
        assert_eq!(body["roles"], serde_json::json!(["owner"]));
        assert_eq!(members_path("t1"), "/teams/t1/members");
        assert_eq!(member_path("t1", "m9"), "/teams/t1/members/m9");
        assert_eq!(
            channel_set_reaction_path("t1", "c1", "m1"),
            "/teams/t1/channels/c1/messages/m1/setReaction"
        );
        assert_eq!(
            channel_reply_unset_reaction_path("t1", "c1", "m1", "r2"),
            "/teams/t1/channels/c1/messages/m1/replies/r2/unsetReaction"
        );
        assert_eq!(
            channel_react_body("❤️"),
            serde_json::json!({ "reactionType": "❤️" })
        );
        assert_eq!(PUBLIC_TEAMS_MAX, 25);
    }

    // ----- B5: recordings & transcripts -----------------------------------

    #[tokio::test]
    async fn list_recordings_reads_the_onedrive_folder_through_graph() {
        let body = serde_json::json!({ "value": [{
            "id": "r1", "name": "Weekly Sync.mp4", "size": 48211,
            "webUrl": "https://x/r1",
            "@microsoft.graph.downloadUrl": "https://dl/r1",
            "lastModifiedDateTime": "2026-09-24T10:00:00Z",
            "file": {"mimeType": "video/mp4"},
            "video": {"duration": 3723000},
            "parentReference": {"driveId": "D1"}
        }]})
        .to_string();
        // OneDrive answers, then the teams scan finds no teams (no channels).
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(body),
            StubResponse::ok(serde_json::json!({ "value": [] }).to_string()),
        ])
        .await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        let rows = client.list_recordings(99).await.expect("list recordings");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "r1");
        assert_eq!(rows[0].duration_ms, Some(3723000));
        assert_eq!(rows[0].source, RecordingSource::OneDrive);
        assert_eq!(client.request_count(), 1, "one high-level call");

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 2, "the OneDrive list + the teams scan");
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
    }

    #[tokio::test]
    async fn search_transcripts_uses_the_drive_search_and_rejects_blank() {
        let body = serde_json::json!({ "value": [{
            "id": "t1", "name": "Weekly Sync.vtt", "size": 4211,
            "file": {"mimeType": "text/vtt"},
            "parentReference": {"driveId": "D1"}
        }]})
        .to_string();
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(body),
            StubResponse::ok(serde_json::json!({ "value": [] }).to_string()),
        ])
        .await;
        let client = ApiClient::from_tokens(&b2a_tokens(&base));

        let rows = client
            .search_transcripts("weekly", 99)
            .await
            .expect("search");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "t1");

        // A blank query is refused before any further network.
        let err = client
            .search_transcripts("  ", 50)
            .await
            .err()
            .expect("blank query refused");
        assert!(format!("{err:#}").contains("empty query"), "{err:#}");

        task.await.expect("stub drains");
        let reqs = b2a_requests(&seen);
        assert_eq!(reqs.len(), 2);
        assert!(
            reqs[0].starts_with("GET /me/drive/search(q='weekly')?$top=99 "),
            "unexpected request: {}",
            reqs[0]
        );
    }

    #[test]
    fn b5_pure_helpers_are_reexported() {
        // The re-exported shapes/predicates are pinned here too (the service
        // tests exercise them against a stub).
        assert_eq!(RECORDINGS_MAX_LIMIT, 50);
        assert_eq!(TRANSCRIPTS_MAX_LIMIT, 100);
        assert_eq!(
            recordings_children_path(1),
            "/me/drive/root:/Recordings:/children?$top=1"
        );
        assert_eq!(recordings_clamp_limit(999), 50);
        assert!(is_recording_video("a.mp4", None));
        assert_eq!(
            transcripts_search_path(None, ".vtt", 999),
            "/me/drive/search(q='.vtt')?$top=100"
        );
        assert!(is_transcript("a.vtt", None));
        assert!(!is_transcript("notes.docx", None));
        // The source enum mirrors the two stores.
        assert_eq!(RecordingSource::OneDrive.label(), "OneDrive");
        assert_eq!(
            RecordingSource::Channel {
                team: "Eng".into(),
                channel: "general".into()
            }
            .label(),
            "Eng > #general"
        );
    }
}
