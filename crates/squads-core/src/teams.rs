//! Offline-first teams & channels service (milestone **M6**).
//!
//! This is the layer the M10 teams/channels browser drives. Like
//! [`Messaging`](crate::messaging), it ties the shared [`Store`] (instant,
//! offline reads) to the shared [`ApiClient`] (typed protocol calls), and every
//! method is testable offline against a local TCP stub.
//!
//! ```text
//!                    UI (M10)
//!                     │  list_teams / channels / open_channel / post_to_channel
//!                     ▼
//!                   Teams ──▶ Arc<Store>      (M3: cached teams/channels/messages)
//!                     └────▶ Arc<ApiClient>    (M2: typed protocol calls)
//! ```
//!
//! ## Which surface serves which operation
//!
//! Teams spreads one logical operation across three hosts; this service keeps
//! the split explicit (see `docs/DECISIONS.md`, channel routing):
//!
//! | Operation | Surface | Method |
//! |---|---|---|
//! | History (read) | **Chat service** | [`Teams::open_channel`] / [`Teams::load_older_channel`] |
//! | Top-level channel post | **Chat service** | [`Teams::post_to_channel`] |
//! | Thread reply | **Chat service** | [`Teams::reply_in_thread`] |
//! | Teams + channels + roster (metadata) | **Graph** | [`Teams::list_teams`] / [`Teams::channels`] / [`Teams::members`] |
//! | Create channel | **Graph** (vendored) | [`Teams::create_channel`] |
//! | Edit / delete channel | **Middle tier** | [`Teams::update_channel`] / [`Teams::delete_channel`] |
//! | Create team | **Middle tier** | [`Teams::create_team`] |
//! | Public-team search / join | **Graph** | [`Teams::search_public`] / [`Teams::join`] |
//! | Member add / remove / settings | **Graph** | [`Teams::add_member`] / [`Teams::remove_member`] / [`Teams::settings`] |
//! | Channel reaction | **Graph** (dedicated lane) | [`Teams::react_in_channel`] |
//!
//! Channel *messages* ride the **same chat-service endpoints as chats**: the
//! channel conversation id (`19:…@thread.tacv2`) is accepted by
//! `send_message_with_client_id` / `read_messages_page` unchanged, which is why
//! [`Teams::open_channel`] and [`Teams::post_to_channel`] simply reuse
//! [`Messaging`]. Channel *metadata* is Graph; channel *writes Graph refuses*
//! (edit/delete) and team creation are the middle tier.
//!
//! ## Thread replies: the conversation mapping
//!
//! A channel is a reply chain, not a flat chat. A reply is posted to the derived
//! **thread conversation** `<channel_id>;messageid=<root_id>` (the same shape
//! the server stamps on a channel reply's `conversationLink`), so
//! [`Teams::reply_in_thread`] does **not** post a quote block: it calls the
//! vendored `thread_reply_with_client_id` and stores the reply under that
//! derived conversation id (with the root echoed in `reply_to`). The UI must
//! group channel replies by `<channel>;messageid=<root>` — **not** the channel
//! id — when it renders a thread; [`Teams::thread_conversation`] exposes the
//! mapping so callers do not have to re-derive it, and
//! [`split_thread_conversation`] is its inverse.
//!
//! ## Team administration and channel reactions (B4)
//!
//! Public-team discovery/join, member add/remove and team settings are **Graph**
//! ([`Teams::search_public`], [`Teams::join`], [`Teams::add_member`],
//! [`Teams::remove_member`], [`Teams::settings`]). Channel **reactions** use a
//! **dedicated Graph lane** ([`Teams::react_in_channel`] →
//! `…/messages/{id}/(set|unset)Reaction`, with a `replies/{reply}` variant),
//! *not* the chat-service reaction path: that endpoint **404s** on channel
//! threads, and the Graph channel-reaction paths are unverified live (U-003).
//! The lane needs the **team** id alongside the channel, resolved from the
//! cached channel; the chat paths keep the M5
//! [`Messaging::react`](crate::messaging::Messaging::react) lane. See ADR-040.
//!
//! ## Optimistic writes
//!
//! [`Teams::post_to_channel`] reuses [`Messaging::send_text`]'s optimistic send
//! ([`SendState::Pending`] row before the POST; re-keyed to the server id and
//! [`SendState::Sent`] on success; [`SendState::Failed`], retained, on failure).
//! [`Teams::reply_in_thread`] implements the same shape for the thread path,
//! keyed by its own `client_message_id`.

use std::sync::Arc;

use crate::api::ApiClient;
use crate::error::Result;
use crate::messaging::{HistoryPage, Messaging, apply_reaction};
use crate::model::{Channel, Message, PublicTeam, SendState, Team, TeamMember, TeamSettings};
use crate::store::Store;

/// Messages fetched by [`Teams::open_channel`] (the newest page).
pub const OPEN_LIMIT: usize = crate::messaging::OPEN_LIMIT;

/// The offline-first teams & channels service (see the module docs).
pub struct Teams {
    profile: String,
    store: Arc<Store>,
    client: Arc<ApiClient>,
    /// The M5 conversation service, reused for channel history and top-level
    /// posts. It owns the per-conversation paging cursor (ADR-016), so `Teams`
    /// holds one instance for its whole lifetime: `open_channel` then
    /// `load_older_channel` must see the same cursors.
    messaging: Messaging,
}

impl Teams {
    /// Build the service over a shared store and protocol client.
    pub fn new(profile: impl Into<String>, store: Arc<Store>, client: Arc<ApiClient>) -> Self {
        let profile = profile.into();
        let messaging = Messaging::new(profile.clone(), Arc::clone(&store), Arc::clone(&client));
        Self {
            profile,
            store,
            client,
            messaging,
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

    /// The thread conversation id a channel reply lives under
    /// (`<channel_id>;messageid=<root_id>`). Pure; see the module docs.
    pub fn thread_conversation(&self, channel_id: &str, root_id: &str) -> String {
        ost::api::thread_reply_conversation(channel_id, root_id)
    }

    // ----- teams & channels (metadata) ------------------------------------

    /// The cached teams for this profile. Purely local: never touches the
    /// network (the UI's instant first paint).
    pub fn cached_teams(&self) -> Result<Vec<Team>> {
        self.store.list_teams(&self.profile)
    }

    /// Fetch the joined teams (Graph `/me/joinedTeams`), cache the teams **and
    /// their channels**, and return the cached teams.
    ///
    /// This is a metadata refresh; it makes one request per team (the vendored
    /// listing fetches each team's channels), so call it on open/refresh rather
    /// than on every render. [`Teams::cached_teams`] is the offline read.
    pub async fn list_teams(&self) -> Result<Vec<Team>> {
        let infos = self.client.list_teams().await?;
        let teams: Vec<Team> = infos.iter().map(Team::from_info).collect();
        self.store.upsert_teams(&self.profile, &teams)?;
        for info in &infos {
            let channels: Vec<Channel> = info
                .channels
                .iter()
                .map(|c| Channel::from_info(&info.id, c))
                .collect();
            self.store.upsert_channels(&self.profile, &channels)?;
        }
        self.store.list_teams(&self.profile)
    }

    /// The cached channels of one team. Purely local (offline read).
    pub fn cached_channels(&self, team_id: &str) -> Result<Vec<Channel>> {
        self.store.channels_for_team(team_id)
    }

    /// Fetch and cache one team's channels.
    ///
    /// The Graph listing carries channels inline, so this is a
    /// [`Teams::list_teams`] refresh followed by the cached read; there is no
    /// separate per-team channel endpoint in the wrapper.
    pub async fn channels(&self, team_id: &str) -> Result<Vec<Channel>> {
        self.list_teams().await?;
        self.cached_channels(team_id)
    }

    // ----- members --------------------------------------------------------

    /// Fetch one team's roster (Graph `GET /teams/{id}/members`), cache it, and
    /// return it (display-name order).
    pub async fn members(&self, team_id: &str) -> Result<Vec<TeamMember>> {
        let infos = self.client.list_team_members(team_id).await?;
        let members: Vec<TeamMember> = infos.iter().map(TeamMember::from_info).collect();
        self.store
            .upsert_team_members(&self.profile, team_id, &members)?;
        Ok(members)
    }

    /// The cached roster of one team (offline read).
    pub fn cached_members(&self, team_id: &str) -> Result<Vec<TeamMember>> {
        self.store.team_members(team_id)
    }

    // ----- team administration (B4) ---------------------------------------

    /// Search the directory for public (joinable) teams (Graph `/groups`
    /// `$search`, falling back to `/teams` `startswith`), marking each hit with
    /// whether it is already joined.
    ///
    /// **Single window** (Graph caps at [`PUBLIC_TEAMS_MAX`](crate::api::PUBLIC_TEAMS_MAX));
    /// a blank query returns an empty list without a request. Membership is not
    /// on the wire, so [`PublicTeam::is_joined`] is derived from the cached
    /// joined teams (the store); a search therefore reflects the last
    /// [`Teams::list_teams`].
    pub async fn search_public(&self, query: &str) -> Result<Vec<PublicTeam>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let (_source, infos) = self
            .client
            .search_public_teams(query, crate::api::PUBLIC_TEAMS_MAX)
            .await?;
        let joined: std::collections::HashSet<String> = self
            .store
            .list_teams(&self.profile)?
            .into_iter()
            .map(|team| team.id)
            .collect();
        Ok(infos
            .iter()
            .map(|info| PublicTeam::from_info(info, joined.contains(&info.id)))
            .collect())
    }

    /// Join one team as the signed-in user, then refresh the joined-teams cache.
    ///
    /// Returns the joined [`Team`] from the refreshed cache (or an id-named
    /// placeholder when Graph does not list the fresh membership yet). The
    /// refresh is the point: it fills the channels the new team brings, so the
    /// browser shows it immediately.
    pub async fn join(&self, team_id: &str) -> Result<Team> {
        let id = self.client.join_team(team_id).await?;
        let teams = self.list_teams().await?;
        Ok(teams
            .into_iter()
            .find(|team| team.id == id)
            .unwrap_or(Team {
                name: id.clone(),
                id,
            }))
    }

    /// Add one user (a Graph/AAD id or UPN) to a team and cache the new roster
    /// entry. `owner` grants the owner role.
    pub async fn add_member(
        &self,
        team_id: &str,
        user_id: &str,
        owner: bool,
    ) -> Result<TeamMember> {
        let info = self.client.add_team_member(team_id, user_id, owner).await?;
        let member = TeamMember::from_info(&info);
        self.store
            .upsert_team_members(&self.profile, team_id, std::slice::from_ref(&member))?;
        Ok(member)
    }

    /// Remove one membership (the roster **membership** id, not the user id)
    /// from a team and drop the cached entry.
    pub async fn remove_member(&self, team_id: &str, membership_id: &str) -> Result<()> {
        self.client
            .remove_team_member(team_id, membership_id)
            .await?;
        self.store
            .delete_team_member(&self.profile, team_id, membership_id)?;
        Ok(())
    }

    /// Read a team's member permissions + General channel id (Graph; two GETs).
    ///
    /// Purely a read: nothing is persisted (settings are cheap to re-read and
    /// per-team, so they are not cached — see ADR-040).
    pub async fn settings(&self, team_id: &str) -> Result<TeamSettings> {
        let settings = self.client.team_settings(team_id).await?;
        Ok(TeamSettings::from_wire(&settings))
    }

    /// Add or remove an emoji reaction on a **channel** message, adjusting the
    /// cached row's counts.
    ///
    /// Channel reactions use the dedicated **Graph** lane (the chat-service
    /// reaction endpoint 404s on channel threads — see ADR-018/ADR-040 and
    /// U-003), which needs the **team** id as well as the channel; the team is
    /// resolved from the cached channel (`Store::team_for_channel`), so the
    /// channel must have been listed (a [`Teams::list_teams`]/[`Teams::channels`]
    /// refresh) first.
    ///
    /// `conversation_id` is the conversation the message is stored under: the
    /// channel id for a top-level message, or the derived thread conversation
    /// `<channel>;messageid=<root>` for a reply. When `is_reply` is `true` the
    /// reaction targets the `…/replies/{reply}/setReaction` lane: the root is
    /// taken from `conversation_id`'s `;messageid=` suffix when present, else
    /// from the stored message's `reply_to`, and `message_id` is the **reply**
    /// id. The local count adjustment is coarse (±1, no reactor identity), the
    /// same as [`Messaging::react`](crate::messaging::Messaging::react); the
    /// authoritative reactor list arrives with the next history refresh.
    pub async fn react_in_channel(
        &self,
        conversation_id: &str,
        message_id: &str,
        emoji: &str,
        remove: bool,
        is_reply: bool,
    ) -> Result<()> {
        let (channel_id, root_from_conv) = split_thread_conversation(conversation_id);
        let team_id = self
            .store
            .team_for_channel(&self.profile, channel_id)?
            .ok_or_else(|| {
                crate::error::CoreError::Protocol(anyhow::anyhow!(
                    "channel {channel_id} is not cached, so its team is unknown"
                ))
            })?;

        // For a reply, the Graph lane wants (root, reply); the root comes from
        // the derived conversation id, falling back to the stored message.
        let (root_id, reply_id): (String, Option<String>) = if is_reply {
            let root = match root_from_conv {
                Some(root) => root.to_string(),
                None => self
                    .store
                    .message(conversation_id, message_id)?
                    .and_then(|message| message.reply_to)
                    .ok_or_else(|| {
                        crate::error::CoreError::Protocol(anyhow::anyhow!(
                            "reply root unknown (not in the conversation id or the stored message)"
                        ))
                    })?,
            };
            (root, Some(message_id.to_string()))
        } else {
            (message_id.to_string(), None)
        };

        if remove {
            self.client
                .unset_channel_reaction(&team_id, channel_id, &root_id, emoji, reply_id.as_deref())
                .await?;
        } else {
            self.client
                .set_channel_reaction(&team_id, channel_id, &root_id, emoji, reply_id.as_deref())
                .await?;
        }

        if let Some(mut message) = self.store.message(conversation_id, message_id)? {
            apply_reaction(&mut message.reactions, emoji, remove);
            self.store
                .upsert_messages(&self.profile, std::slice::from_ref(&message))?;
        }
        Ok(())
    }

    // ----- channel history (chat service) ---------------------------------

    /// Fetch the newest page of a channel conversation and merge it into the
    /// store, returning up to `limit` messages (oldest-first) plus the backward
    /// cursor.
    ///
    /// Reuses [`Messaging::open`]: a channel conversation id is accepted by the
    /// same chat-service endpoints as a chat. `limit` caps the returned window;
    /// [`OPEN_LIMIT`] messages are fetched.
    pub async fn open_channel(&self, channel_id: &str, limit: usize) -> Result<HistoryPage> {
        let page = self.messaging.open(channel_id).await?;
        Ok(HistoryPage {
            messages: self.store.messages(channel_id, limit)?,
            backward_link: page.backward_link,
        })
    }

    /// Fetch the next older page of a channel conversation using the stored
    /// cursor (see [`Teams::open_channel`]).
    pub async fn load_older_channel(&self, channel_id: &str) -> Result<HistoryPage> {
        self.messaging.load_older(channel_id).await
    }

    // ----- channel writes (chat service) ----------------------------------

    /// Post a top-level message to a channel, optimistically (see the module
    /// docs). Reuses [`Messaging::send_text`].
    pub async fn post_to_channel(&self, channel_id: &str, text: &str) -> Result<Message> {
        self.messaging.send_text(channel_id, text).await
    }

    /// Post a **thread reply** under `root_id` in a channel, optimistically.
    ///
    /// (a) a [`SendState::Pending`] row with a fresh `client_message_id` is
    /// upserted under the derived thread conversation
    /// `<channel_id>;messageid=<root_id>`; (b) the vendored
    /// `thread_reply_with_client_id` posts the plain send body (carrying the
    /// same id) to that conversation; (c) on success the row is re-keyed to the
    /// server id and marked [`SendState::Sent`]; (d) on failure it is marked
    /// [`SendState::Failed`], retained, and the error is returned.
    ///
    /// The returned [`Message`]'s `chat_id` is the thread conversation id (not
    /// the channel), and its `reply_to` is `root_id`; see the module docs.
    pub async fn reply_in_thread(
        &self,
        channel_id: &str,
        root_id: &str,
        text: &str,
    ) -> Result<Message> {
        let client_message_id = ost::api::new_client_message_id();
        let pending = self.pending_thread_reply(channel_id, root_id, text, &client_message_id);
        self.store
            .upsert_messages(&self.profile, std::slice::from_ref(&pending))?;

        match self
            .client
            .thread_reply(channel_id, root_id, text, &client_message_id)
            .await
        {
            Ok(sent) => self.finalize_reply(&pending, sent.id),
            Err(err) => {
                self.store.update_message_send_state(
                    &self.profile,
                    &pending.chat_id,
                    &pending.id,
                    SendState::Failed,
                )?;
                Err(err)
            }
        }
    }

    // ----- channel & team writes (Graph / middle tier) --------------------

    /// Create a channel in `team_id` and cache the returned channel.
    ///
    /// **Graph route** (the vendored `create_channel_data`): unlike edit/delete
    /// this write is not on the middle tier. The store is refreshed from the
    /// write's own answer; a full [`Teams::list_teams`] re-syncs membership
    /// later.
    pub async fn create_channel(
        &self,
        team_id: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<Channel> {
        let info = self
            .client
            .create_channel(team_id, name, description)
            .await?;
        let channel = Channel::from_info(team_id, &info);
        self.store
            .upsert_channels(&self.profile, std::slice::from_ref(&channel))?;
        Ok(channel)
    }

    /// Rename a channel and/or change its description (middle tier), updating
    /// the cached row from the change's own arguments (no extra round-trip).
    ///
    /// `name`/`description` are `Option`: only the given fields change. A blank
    /// `name` is ignored; a `Some(description)` sets it (Graph takes `""` to
    /// clear).
    pub async fn update_channel(
        &self,
        team_id: &str,
        channel_id: &str,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<()> {
        self.client
            .update_channel(team_id, channel_id, name, description)
            .await?;
        if let Some(mut channel) = self.store.channel(team_id, channel_id)? {
            if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty()) {
                channel.name = n.to_string();
            }
            if let Some(d) = description {
                channel.description = Some(d.trim().to_string());
            }
            self.store
                .upsert_channels(&self.profile, std::slice::from_ref(&channel))?;
        }
        Ok(())
    }

    /// Delete one channel (middle tier) and drop the cached row.
    pub async fn delete_channel(&self, team_id: &str, channel_id: &str) -> Result<()> {
        self.client.delete_channel(team_id, channel_id).await?;
        self.store
            .delete_channel(&self.profile, team_id, channel_id)?;
        Ok(())
    }

    /// Create a team (middle tier) and cache it with its channels.
    ///
    /// The vendored `create_team_data` posts the web client's create body to
    /// `POST …/beta/teams/create`, then reads the new team back from Graph. The
    /// returned [`Team`] carries the new team's **Graph group id**.
    pub async fn create_team(&self, name: &str, description: Option<&str>) -> Result<Team> {
        let result = self.client.create_team(name, description).await?;
        let team = Team::from_info(&result.team);
        self.store
            .upsert_teams(&self.profile, std::slice::from_ref(&team))?;
        let channels: Vec<Channel> = result
            .team
            .channels
            .iter()
            .map(|c| Channel::from_info(&team.id, c))
            .collect();
        if !channels.is_empty() {
            self.store.upsert_channels(&self.profile, &channels)?;
        }
        Ok(team)
    }

    // ----- internals ------------------------------------------------------

    /// Build the locally-authored pending reply row for a channel thread.
    fn pending_thread_reply(
        &self,
        channel_id: &str,
        root_id: &str,
        text: &str,
        client_message_id: &str,
    ) -> Message {
        Message {
            // Until the server names the real id, the idempotency key stands in
            // for it (and is what the store row is keyed by).
            id: client_message_id.to_string(),
            // Replies live under the derived thread conversation, not the
            // channel id (see the module docs).
            chat_id: self.thread_conversation(channel_id, root_id),
            sender_mri: String::new(),
            sender_name: String::new(),
            timestamp: now_iso(),
            content: text.to_string(),
            raw: ost::api::build_message_html(text),
            reactions: Vec::new(),
            reply_to: Some(root_id.to_string()),
            client_message_id: Some(client_message_id.to_string()),
            edited: false,
            is_own: true,
            send_state: SendState::Pending,
        }
    }

    /// Reconcile a successful thread reply: re-key to the server id (when it
    /// differs) and mark [`SendState::Sent`].
    fn finalize_reply(&self, pending: &Message, server_id: Option<String>) -> Result<Message> {
        let mut sent = pending.clone();
        sent.send_state = SendState::Sent;
        match server_id {
            Some(server_id) if server_id != pending.id => {
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
}

/// Current UTC time as a Teams-shaped ISO-8601 string (millisecond precision).
///
/// Orders locally-authored pending rows so they sort as the newest reply until
/// the server's real timestamp arrives.
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Split a channel conversation id into `(channel_id, root_id)` (B4).
///
/// A channel reply lives under the derived conversation
/// `<channel>;messageid=<root>` (see the module docs); the channel-reaction
/// lane needs the plain **channel** plus, for a reply, the **root** message id.
/// A plain channel id returns `(id, None)`. Pure; the inverse of
/// [`Teams::thread_conversation`].
pub fn split_thread_conversation(conversation_id: &str) -> (&str, Option<&str>) {
    match conversation_id.split_once(";messageid=") {
        Some((channel, root)) if !root.is_empty() => (channel, Some(root)),
        _ => (conversation_id, None),
    }
}

/// Redacting `Debug`: no message bodies or channel names.
impl std::fmt::Debug for Teams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Teams")
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

    fn tokens_for(base: &str) -> TokenSet {
        let future = crate::auth::tokens::now_unix() + 3600;
        let mk = |name: &str| StoredToken::with_expiry(name, future);
        TokenSet {
            refresh_token: "refresh-secret".into(),
            tenant: "contoso.onmicrosoft.com".into(),
            // Every surface routes at the same stub, so a test can assert
            // which path a call took.
            region_gtms: serde_json::json!({
                "graphBase": base,
                "middleTier": base,
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

    fn test_teams(base: &str) -> (Arc<Store>, Teams) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let client = Arc::new(ApiClient::from_tokens(&tokens_for(base)));
        let teams = Teams::new("default", Arc::clone(&store), client);
        (store, teams)
    }

    fn message(chat_id: &str, id: &str, sender: &str, content: &str, own: bool) -> Message {
        Message {
            id: id.into(),
            chat_id: chat_id.into(),
            sender_mri: format!("8:orgid:{sender}"),
            sender_name: sender.into(),
            timestamp: "2026-01-02T09:30:00Z".into(),
            content: content.into(),
            raw: format!("<p>{content}</p>"),
            reactions: Vec::new(),
            reply_to: None,
            client_message_id: None,
            edited: false,
            is_own: own,
            send_state: SendState::Sent,
        }
    }

    fn requests(seen: &std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> Vec<String> {
        seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    // ----- metadata -------------------------------------------------------

    #[tokio::test]
    async fn list_teams_fetches_parses_and_caches_teams_and_channels() {
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(
                serde_json::json!({"value": [{"id": "team-1", "displayName": "Engineering"}]})
                    .to_string(),
            ),
            StubResponse::ok(
                serde_json::json!({"value": [{
                    "id": "19:general@thread.tacv2",
                    "displayName": "General",
                    "description": "Team-wide",
                    "membershipType": "standard",
                    "webUrl": "https://teams.microsoft.com/l/channel/x"
                }]})
                .to_string(),
            ),
        ])
        .await;
        let (store, teams) = test_teams(&base);

        let got = teams.list_teams().await.expect("list teams");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "team-1");
        assert_eq!(got[0].name, "Engineering");
        // Cached read matches and is offline (no new requests).
        assert_eq!(teams.cached_teams().unwrap(), got);

        let channels = teams.cached_channels("team-1").unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].id, "19:general@thread.tacv2");
        assert_eq!(channels[0].team_id, "team-1");
        assert_eq!(channels[0].name, "General");
        assert_eq!(channels[0].description.as_deref(), Some("Team-wide"));
        assert_eq!(channels[0].membership_type.as_deref(), Some("standard"));
        assert!(channels[0].is_channel);
        assert_eq!(store.channels_for_team("team-1").unwrap().len(), 1);
        assert_eq!(teams.client().request_count(), 1);

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert!(
            reqs[0].starts_with("GET /me/joinedTeams "),
            "unexpected request line: {}",
            reqs[0]
        );
        assert!(
            reqs[0].contains("authorization: Bearer graph-token"),
            "Graph metadata must carry the Graph token: {}",
            reqs[0]
        );
        assert!(
            reqs[1].starts_with("GET /teams/team-1/channels "),
            "{}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn channels_refreshes_then_reads_the_cache() {
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(
                serde_json::json!({"value": [{"id": "team-1", "displayName": "Eng"}]}).to_string(),
            ),
            StubResponse::ok(
                serde_json::json!({"value": [
                    {"id": "19:b@thread.tacv2", "displayName": "Beta"},
                    {"id": "19:a@thread.tacv2", "displayName": "Alpha"}
                ]})
                .to_string(),
            ),
        ])
        .await;
        let (_store, teams) = test_teams(&base);

        let channels = teams.channels("team-1").await.expect("channels");
        assert_eq!(
            channels.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["Alpha", "Beta"],
            "channels are name-ordered"
        );
        task.await.expect("stub drains");
        assert!(requests(&seen)[0].starts_with("GET /me/joinedTeams "));
    }

    #[tokio::test]
    async fn members_fetch_parses_owner_roles_and_cache() {
        let body = serde_json::json!({"value": [
            {"id": "M1", "displayName": "Alice", "email": "a@x.example", "roles": ["owner"]},
            {"id": "M2", "displayName": "Bob", "userId": "oid-2", "roles": []}
        ]})
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let (_store, teams) = test_teams(&base);

        let members = teams.members("team-1").await.expect("members");
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].display_name, "Alice");
        assert_eq!(members[0].email.as_deref(), Some("a@x.example"));
        assert_eq!(members[0].roles, vec!["owner".to_string()]);
        assert!(members[0].is_owner());
        assert!(!members[1].is_owner());
        assert!(members[1].email.is_none());

        // Cached read is offline and preserves the roles.
        let cached = teams.cached_members("team-1").unwrap();
        assert_eq!(cached.len(), 2);
        assert!(cached[0].is_owner());

        task.await.expect("stub drains");
        assert!(
            requests(&seen)[0].starts_with("GET /teams/team-1/members "),
            "{}",
            requests(&seen)[0]
        );
    }

    // ----- channel history & posts (chat service) -------------------------

    #[tokio::test]
    async fn open_channel_and_load_older_use_the_chat_service() {
        let channel = "19:ch@thread.tacv2";
        let (base, seen, task) = http_stub_with(|base| {
            let link =
                format!("{base}/v1/users/ME/conversations/{channel}/messages?pageSize=2&older=1");
            let page1 = serde_json::json!({
                "messages": [{
                    "id": "m2", "content": "<p>two</p>", "messagetype": "RichText/Html",
                    "originalarrivaltime": "2026-01-02T00:00:00Z"
                }],
                "_metadata": { "backwardLink": link }
            });
            let page2 = serde_json::json!({
                "messages": [{
                    "id": "m1", "content": "<p>one</p>", "messagetype": "RichText/Html",
                    "originalarrivaltime": "2026-01-01T00:00:00Z"
                }],
                "_metadata": { "backwardLink": serde_json::Value::Null }
            });
            vec![
                StubResponse::ok(page1.to_string()),
                StubResponse::ok(page2.to_string()),
            ]
        })
        .await;
        let (_store, teams) = test_teams(&base);

        let page = teams.open_channel(channel, 10).await.expect("open channel");
        assert_eq!(
            page.messages
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            vec!["m2"]
        );
        assert!(page.backward_link.is_some());

        let older = teams.load_older_channel(channel).await.expect("older");
        assert_eq!(
            older
                .messages
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            vec!["m1"]
        );
        assert!(older.backward_link.is_none());

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert!(
            reqs[0].starts_with(&format!(
                "GET /v1/users/ME/conversations/{channel}/messages?pageSize=50 "
            )),
            "{}",
            reqs[0]
        );
        assert!(reqs[1].contains("older=1"), "{}", reqs[1]);
    }

    #[tokio::test]
    async fn post_to_channel_is_optimistic_and_posts_the_richtext_body() {
        let (arrive_tx, arrive_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut response = StubResponse::ok_location(
            "https://chat.example/v1/users/ME/conversations/19:ch@thread.tacv2/messages/1700000000001",
            "{}",
        );
        response.arrive = Some(arrive_tx);
        response.release = Some(release_rx);
        let (base, seen, task) = http_stub(vec![response]).await;
        let (store, teams) = test_teams(&base);
        let teams = Arc::new(teams);

        let post = {
            let teams = Arc::clone(&teams);
            tokio::spawn(async move {
                teams
                    .post_to_channel("19:ch@thread.tacv2", "hello channel")
                    .await
            })
        };

        arrive_rx.await.expect("request arrived");
        let rows = store.messages("19:ch@thread.tacv2", 10).unwrap();
        assert_eq!(rows.len(), 1, "the pending row exists before the reply");
        assert_eq!(rows[0].send_state, SendState::Pending);
        assert!(rows[0].is_own);

        release_tx.send(()).expect("release the stub");
        let sent = post.await.expect("task joins").expect("post ok");
        assert_eq!(sent.send_state, SendState::Sent);
        assert_eq!(sent.id, "1700000000001");

        let rows = store.messages("19:ch@thread.tacv2", 10).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "the pending row was replaced, not duplicated"
        );
        assert_eq!(rows[0].id, "1700000000001");

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(
            req.starts_with("POST /v1/users/ME/conversations/19:ch@thread.tacv2/messages "),
            "unexpected request line: {req}"
        );
        assert!(
            req.contains("\"content\":\"<p>hello channel</p>\""),
            "{req}"
        );
        assert!(req.contains("\"messagetype\":\"RichText/Html\""), "{req}");
    }

    #[tokio::test]
    async fn reply_in_thread_hits_the_thread_reply_path_with_the_root_id() {
        let channel = "19:ch@thread.tacv2";
        let root = "1700000000000";
        let thread = "19:ch@thread.tacv2;messageid=1700000000000";

        let (arrive_tx, arrive_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut response = StubResponse::ok_location(
            "https://chat.example/v1/users/ME/conversations/19:ch@thread.tacv2;messageid=1700000000000/messages/1700000000999",
            "{}",
        );
        response.arrive = Some(arrive_tx);
        response.release = Some(release_rx);
        let (base, seen, task) = http_stub(vec![response]).await;
        let (store, teams) = test_teams(&base);
        let teams = Arc::new(teams);

        assert_eq!(teams.thread_conversation(channel, root), thread);

        let reply = {
            let teams = Arc::clone(&teams);
            tokio::spawn(
                async move { teams.reply_in_thread(channel, root, "threaded reply").await },
            )
        };

        // Optimistic boundary: the pending row is under the *thread* conversation.
        arrive_rx.await.expect("request arrived");
        let pending = store.messages(thread, 10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].send_state, SendState::Pending);
        assert_eq!(pending[0].chat_id, thread);
        assert_eq!(pending[0].reply_to.as_deref(), Some(root));
        assert!(pending[0].client_message_id.is_some());

        release_tx.send(()).expect("release the stub");
        let reply = reply.await.expect("task joins").expect("reply ok");
        assert_eq!(reply.send_state, SendState::Sent);
        assert_eq!(reply.id, "1700000000999");
        assert_eq!(reply.chat_id, thread);
        assert_eq!(reply.reply_to.as_deref(), Some(root));

        let rows = store.messages(thread, 10).unwrap();
        assert_eq!(rows.len(), 1, "re-keyed, not duplicated");
        assert_eq!(rows[0].id, "1700000000999");
        assert!(
            store.messages(channel, 10).unwrap().is_empty(),
            "the reply is not stored under the plain channel id"
        );

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(
            req.starts_with(&format!(
                "POST /v1/users/ME/conversations/{thread}/messages "
            )),
            "unexpected request line: {req}"
        );
        assert!(
            req.contains(&format!(";messageid={root}")),
            "the root id must be in the thread conversation: {req}"
        );
        assert!(
            req.contains("\"content\":\"<p>threaded reply</p>\""),
            "{req}"
        );
    }

    #[tokio::test]
    async fn reply_in_thread_failure_keeps_a_failed_row() {
        let (base, _seen, task) = http_stub(vec![StubResponse::status(500)]).await;
        let (store, teams) = test_teams(&base);
        let thread = "19:ch@thread.tacv2;messageid=7";

        let err = teams
            .reply_in_thread("19:ch@thread.tacv2", "7", "nope")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("500"), "err = {err}");

        let rows = store.messages(thread, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].send_state, SendState::Failed);
        task.await.expect("stub drains");
    }

    // ----- writes: Graph create, middle-tier edit/delete/create -------------

    #[tokio::test]
    async fn create_channel_posts_to_graph_and_caches_the_returned_channel() {
        let body = serde_json::json!({
            "id": "19:new@thread.tacv2",
            "displayName": "New room",
            "description": "Plans",
            "webUrl": "https://teams.microsoft.com/l/channel/new"
        })
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let (store, teams) = test_teams(&base);

        let channel = teams
            .create_channel("team-1", "New room", Some("Plans"))
            .await
            .expect("create channel");
        assert_eq!(channel.id, "19:new@thread.tacv2");
        assert_eq!(channel.team_id, "team-1");
        assert_eq!(channel.name, "New room");
        assert_eq!(channel.description.as_deref(), Some("Plans"));
        assert!(channel.is_channel);

        // Cached from the write's own answer (offline read).
        let cached = store.channels_for_team("team-1").unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].id, "19:new@thread.tacv2");

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(
            req.starts_with("POST /teams/team-1/channels "),
            "unexpected request line: {req}"
        );
        assert!(req.contains("\"displayName\":\"New room\""), "{req}");
        assert!(req.contains("\"description\":\"Plans\""), "{req}");
        // The vendored create rides Graph (not the middle tier); assert the
        // Graph token is what reached the stub.
        assert!(req.contains("authorization: Bearer graph-token"), "{req}");
    }

    #[tokio::test]
    async fn update_channel_patches_the_middle_tier_url_and_refreshes_the_cache() {
        let team_thread = "19:team@thread.tacv2";
        let channel_id = "19:ch@thread.tacv2";
        let (base, seen, task) = http_stub(vec![StubResponse::ok("{}")]).await;
        let (store, teams) = test_teams(&base);
        store
            .upsert_channels(
                "default",
                &[Channel {
                    id: channel_id.into(),
                    team_id: team_thread.into(),
                    name: "Old name".into(),
                    description: None,
                    membership_type: None,
                    web_url: None,
                    is_channel: true,
                }],
            )
            .unwrap();

        teams
            .update_channel(team_thread, channel_id, Some("New name"), Some("new desc"))
            .await
            .expect("update channel");

        let cached = store.channel(team_thread, channel_id).unwrap().unwrap();
        assert_eq!(cached.name, "New name");
        assert_eq!(cached.description.as_deref(), Some("new desc"));

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        // The middle tier keys teams by their thread id (already a thread id
        // here, so no Graph lookup) and percent-encodes the segments.
        assert!(
            req.starts_with(
                "PATCH /beta/teams/19%3Ateam%40thread.tacv2/channels/19%3Ach%40thread.tacv2 "
            ),
            "unexpected request line: {req}"
        );
        // Middle-tier auth: AAD bearer + X-Skypetoken, never the Graph token.
        assert!(req.contains("authorization: Bearer spaces-token"), "{req}");
        assert!(req.contains("x-skypetoken: skype-token"), "{req}");
        assert!(!req.contains("graph-token"), "{req}");
        assert!(req.contains("\"displayName\":\"New name\""), "{req}");
        assert!(req.contains("\"description\":\"new desc\""), "{req}");
    }

    #[tokio::test]
    async fn delete_channel_deletes_on_the_middle_tier_and_drops_the_cache() {
        let team_thread = "19:team@thread.tacv2";
        let channel_id = "19:ch@thread.tacv2";
        let (base, seen, task) = http_stub(vec![StubResponse::ok("{}")]).await;
        let (store, teams) = test_teams(&base);
        store
            .upsert_channels(
                "default",
                &[Channel {
                    id: channel_id.into(),
                    team_id: team_thread.into(),
                    name: "Doomed".into(),
                    description: None,
                    membership_type: None,
                    web_url: None,
                    is_channel: true,
                }],
            )
            .unwrap();

        teams
            .delete_channel(team_thread, channel_id)
            .await
            .expect("delete channel");
        assert!(store.channels_for_team(team_thread).unwrap().is_empty());

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(
            req.starts_with(
                "DELETE /beta/teams/19%3Ateam%40thread.tacv2/channels/19%3Ach%40thread.tacv2 "
            ),
            "unexpected request line: {req}"
        );
        assert!(req.contains("x-skypetoken: skype-token"), "{req}");
    }

    #[tokio::test]
    async fn create_team_posts_the_web_body_to_the_middle_tier_and_caches() {
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(
                serde_json::json!({"value": {
                    "siteInfo": {"groupId": "group-1"},
                    "skypeThreadId": "19:team@thread.tacv2"
                }})
                .to_string(),
            ),
            // The vendored create reads the new team back from Graph.
            StubResponse::ok(
                serde_json::json!({"id": "group-1", "displayName": "Squad"}).to_string(),
            ),
            StubResponse::ok(
                serde_json::json!({"value": [
                    {"id": "19:general@thread.tacv2", "displayName": "General"}
                ]})
                .to_string(),
            ),
        ])
        .await;
        let (store, teams) = test_teams(&base);

        let team = teams
            .create_team("Squad", Some("Ship it"))
            .await
            .expect("create team");
        assert_eq!(team.id, "group-1");
        assert_eq!(team.name, "Squad");
        assert_eq!(store.list_teams("default").unwrap().len(), 1);
        assert_eq!(store.channels_for_team("group-1").unwrap().len(), 1);

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert!(
            reqs[0].starts_with("POST /beta/teams/create "),
            "unexpected request line: {}",
            reqs[0]
        );
        assert!(reqs[0].contains("x-skypetoken: skype-token"), "{}", reqs[0]);
        assert!(reqs[0].contains("\"displayName\":\"Squad\""), "{}", reqs[0]);
        assert!(reqs[0].contains("\"accessType\":1"), "{}", reqs[0]);
        // The read-back is Graph.
        assert!(reqs[1].starts_with("GET /teams/group-1 "), "{}", reqs[1]);
        assert!(
            reqs[2].starts_with("GET /teams/group-1/channels "),
            "{}",
            reqs[2]
        );
    }

    // ----- invariants -----------------------------------------------------

    #[test]
    fn teams_is_send_and_sync_and_debug_is_redacting() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Teams>();
        let (_store, teams) = test_teams("http://127.0.0.1:9");
        let dbg = format!("{teams:?}");
        assert!(dbg.contains("Teams"));
        assert!(dbg.contains("default"));
    }

    #[test]
    fn cached_reads_never_touch_the_network() {
        // Port 9 is unreachable; `cached_*` must not connect.
        let (store, teams) = test_teams("http://127.0.0.1:9");
        store
            .upsert_teams(
                "default",
                &[Team {
                    id: "t1".into(),
                    name: "One".into(),
                }],
            )
            .unwrap();
        assert_eq!(teams.cached_teams().unwrap().len(), 1);
        assert!(teams.cached_channels("t1").unwrap().is_empty());
        assert!(teams.cached_members("t1").unwrap().is_empty());
        assert_eq!(teams.client().request_count(), 0);
    }

    // ----- B4: team administration ----------------------------------------

    fn seed_channel(store: &Store, team_id: &str, channel_id: &str) {
        store
            .upsert_channels(
                "default",
                &[Channel {
                    id: channel_id.into(),
                    team_id: team_id.into(),
                    name: "General".into(),
                    description: None,
                    membership_type: None,
                    web_url: None,
                    is_channel: true,
                }],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn search_public_marks_membership_against_the_joined_cache() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok(
            serde_json::json!({
                "value": [
                    {"id": "team-1", "displayName": "Joined", "visibility": "public"},
                    {"id": "team-2", "displayName": "New", "visibility": "public"}
                ]
            })
            .to_string(),
        )])
        .await;
        let (store, teams) = test_teams(&base);
        store
            .upsert_teams(
                "default",
                &[Team {
                    id: "team-1".into(),
                    name: "Joined".into(),
                }],
            )
            .unwrap();

        let found = teams.search_public("eng").await.expect("search");
        assert_eq!(found.len(), 2);
        assert!(found[0].is_joined, "team-1 is in the joined cache");
        assert!(!found[1].is_joined);

        // A blank query short-circuits with no request.
        assert!(teams.search_public("  ").await.unwrap().is_empty());

        task.await.expect("stub drains");
        assert!(
            requests(&seen)[0].starts_with("GET /groups?$search="),
            "{}",
            requests(&seen)[0]
        );
    }

    #[tokio::test]
    async fn join_posts_self_then_refreshes_the_team_list() {
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(serde_json::json!({ "id": "me-oid" }).to_string()),
            StubResponse::status(201),
            StubResponse::ok(
                serde_json::json!({"value": [{"id": "team-1", "displayName": "Eng"}]}).to_string(),
            ),
            StubResponse::ok(
                serde_json::json!({"value": [
                    {"id": "19:general@thread.tacv2", "displayName": "General"}
                ]})
                .to_string(),
            ),
        ])
        .await;
        let (store, teams) = test_teams(&base);

        let team = teams.join("team-1").await.expect("join");
        assert_eq!(team.id, "team-1");
        assert_eq!(team.name, "Eng");
        // The refresh cached the joined team and its channels.
        assert_eq!(store.list_teams("default").unwrap().len(), 1);
        assert_eq!(store.channels_for_team("team-1").unwrap().len(), 1);

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert!(reqs[0].starts_with("GET /me "), "{}", reqs[0]);
        assert!(
            reqs[1].starts_with("POST /teams/team-1/members "),
            "{}",
            reqs[1]
        );
        assert!(reqs[2].starts_with("GET /me/joinedTeams "), "{}", reqs[2]);
    }

    #[tokio::test]
    async fn add_member_caches_and_remove_member_drops_the_roster_entry() {
        let body = serde_json::json!({
            "id": "M9", "displayName": "Ava", "email": "ava@x.example", "roles": []
        })
        .to_string();
        let (base, seen, task) =
            http_stub(vec![StubResponse::ok(body), StubResponse::status(204)]).await;
        let (store, teams) = test_teams(&base);

        let member = teams
            .add_member("team-1", "oid-9", false)
            .await
            .expect("add member");
        assert_eq!(member.id, "M9");
        assert!(!member.is_owner());
        assert_eq!(store.team_members("team-1").unwrap().len(), 1);

        teams
            .remove_member("team-1", "M9")
            .await
            .expect("remove member");
        assert!(store.team_members("team-1").unwrap().is_empty());

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert!(
            reqs[0].starts_with("POST /teams/team-1/members "),
            "{}",
            reqs[0]
        );
        assert!(
            reqs[1].starts_with("DELETE /teams/team-1/members/M9 "),
            "{}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn settings_maps_the_team_member_permissions() {
        let (base, seen, task) = http_stub(vec![
            StubResponse::ok(
                serde_json::json!({
                    "id": "team-1",
                    "memberSettings": { "allowCreateUpdateChannels": true }
                })
                .to_string(),
            ),
            StubResponse::ok(serde_json::json!({ "id": "19:general@thread.tacv2" }).to_string()),
        ])
        .await;
        let (_store, teams) = test_teams(&base);

        let settings = teams.settings("team-1").await.expect("settings");
        assert_eq!(settings.allow_create_update_channels, Some(true));
        assert_eq!(settings.allow_delete_channels, None, "unknown, not false");
        assert_eq!(
            settings.primary_channel_id.as_deref(),
            Some("19:general@thread.tacv2")
        );

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert!(reqs[0].starts_with("GET /teams/team-1 "), "{}", reqs[0]);
        assert!(
            reqs[1].starts_with("GET /teams/team-1/primaryChannel "),
            "{}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn react_in_channel_resolves_the_team_and_updates_the_store() {
        let channel = "19:ch@thread.tacv2";
        let (base, seen, task) =
            http_stub(vec![StubResponse::status(204), StubResponse::status(204)]).await;
        let (store, teams) = test_teams(&base);
        seed_channel(&store, "team-1", channel);
        store
            .upsert_messages(
                "default",
                &[message(channel, "m1", "Ava", "react to me", false)],
            )
            .unwrap();

        teams
            .react_in_channel(channel, "m1", "👍", false, false)
            .await
            .expect("add reaction");
        let row = store.message(channel, "m1").unwrap().unwrap();
        assert_eq!(row.reactions.len(), 1);
        assert_eq!(row.reactions[0].count, 1);

        teams
            .react_in_channel(channel, "m1", "👍", true, false)
            .await
            .expect("remove reaction");
        assert!(
            store
                .message(channel, "m1")
                .unwrap()
                .unwrap()
                .reactions
                .is_empty()
        );

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert!(
            reqs[0].starts_with(&format!(
                "POST /teams/team-1/channels/{channel}/messages/m1/setReaction "
            )),
            "unexpected request: {}",
            reqs[0]
        );
        assert!(
            reqs[1].starts_with(&format!(
                "POST /teams/team-1/channels/{channel}/messages/m1/unsetReaction "
            )),
            "unexpected request: {}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn react_in_channel_routes_a_thread_reply_through_the_reply_lane() {
        let channel = "19:ch@thread.tacv2";
        let root = "1700000000000";
        let thread = format!("{channel};messageid={root}");
        let (base, seen, task) = http_stub(vec![StubResponse::status(204)]).await;
        let (store, teams) = test_teams(&base);
        seed_channel(&store, "team-1", channel);
        let mut reply = message(&thread, "r2", "Ava", "in thread", false);
        reply.reply_to = Some(root.to_string());
        store.upsert_messages("default", &[reply]).unwrap();

        // The thread conversation id carries the root, so no store lookup is
        // needed for it.
        teams
            .react_in_channel(&thread, "r2", "❤️", false, true)
            .await
            .expect("react to reply");
        assert_eq!(
            store.message(&thread, "r2").unwrap().unwrap().reactions[0].count,
            1
        );

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(
            req.starts_with(&format!(
                "POST /teams/team-1/channels/{channel}/messages/{root}/replies/r2/setReaction "
            )),
            "unexpected request: {req}"
        );
    }

    #[tokio::test]
    async fn react_in_channel_refuses_an_uncached_channel_before_any_network() {
        let (base, seen, task) = http_stub(vec![]).await;
        let (_store, teams) = test_teams(&base);

        let err = teams
            .react_in_channel("19:unknown@thread.tacv2", "m1", "👍", false, false)
            .await
            .expect_err("uncached channel refused");
        assert!(format!("{err:#}").contains("not cached"), "{err:#}");

        task.await.expect("stub drains");
        assert!(requests(&seen).is_empty());
    }

    #[test]
    fn split_thread_conversation_inverts_the_mapping() {
        assert_eq!(
            split_thread_conversation("19:ch@thread.tacv2"),
            ("19:ch@thread.tacv2", None)
        );
        assert_eq!(
            split_thread_conversation("19:ch@thread.tacv2;messageid=7"),
            ("19:ch@thread.tacv2", Some("7"))
        );
        // A trailing empty root is not a thread id.
        assert_eq!(
            split_thread_conversation("19:ch@thread.tacv2;messageid="),
            ("19:ch@thread.tacv2;messageid=", None)
        );
    }
}
