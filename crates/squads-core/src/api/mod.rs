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
//! Channel reactions and read receipts are **best-effort**: they ride separate
//! Graph paths that are not live-verified, and the chat-service consumption
//! horizon 403s on `tacv2` threads (see `docs/BLOCKERS.md`).

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use ost::api::ChatInfo;
use ost::api::MessageInfo;
use ost::api::client::TeamsClient;
use ost::config::Config;

/// Protocol types surfaced by the M5 chat calls, re-exported so the UI and
/// `messaging` do not need to reach into the vendored crate.
pub use ost::api::{MessagesPage, ReadReceipt, SentMessage};

/// Protocol types surfaced by the M6 teams/channel calls.
pub use ost::api::{ChannelInfo, TeamCreateResult, TeamInfo, TeamMemberInfo};

use crate::auth::tokens::{StoredToken, TokenSet};
use crate::error::Result;

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

/// A typed, instrumented facade over the vendored Teams client.
///
/// Holds one `ost::api::client::TeamsClient` (built from an in-memory config)
/// plus the `regionGtms` routing blob and a request counter. `Debug` is
/// redacting: it never reveals the tokens the client is holding.
pub struct ApiClient {
    client: TeamsClient,
    region_gtms: serde_json::Value,
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
        Self {
            client: TeamsClient::from_config(config),
            region_gtms,
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
}
