//! UI-facing domain model (milestone **M3**).
//!
//! The store, the UI and (from M4) the realtime fan-out all speak these types
//! rather than the protocol crate's wire structs. Keeping a small, owned model
//! here means:
//!
//! - the protocol crate can change wire shapes without touching the UI,
//! - the store persists plain, serde-friendly values, and
//! - the model can carry local-only state the wire never sends (e.g. `unread`,
//!   `is_own`, `edited`).
//!
//! Conversions are explicit and cheap:
//! [`Chat::from_info`] / `From<&ost::api::ChatInfo>` and
//! [`Message::from_info`] / `From<&ost::api::MessageInfo>`.
//! M6 adds [`Team::from_info`] / [`Channel::from_info`] /
//! [`TeamMember::from_info`]; M7 adds [`FileEntry::from_shared`] /
//! [`FileVersion::from_wire`]; M13 adds [`Identity::from_whoami`]; B3 adds
//! [`Person::from_member`]; B2a adds [`ChatMember::from_info`]; B2b adds
//! [`ChatFolder::from_wire`] / [`PinnedMessage::from_wire`]; B4 adds
//! [`PublicTeam::from_info`] / [`TeamSettings::from_wire`].
//!
//! ## Two facts you must orient on
//!
//! 1. **`MessageInfo` carries no "edited" marker.** Its fields are exactly
//!    `id`, `sender_mri`, `sender`, `timestamp`, `content`, `raw`, `reactions`,
//!    `reply_to`, `client_message_id` (verified against
//!    `crates/squads-protocol/src/api/chat.rs`); edits are signalled only out of
//!    band (a realtime event in M4, or our own edit call in M5). [`Message::edited`]
//!    therefore defaults to `false` from the wire and is set explicitly with
//!    [`Message::mark_edited`].
//! 2. **`MessageInfo` cannot know whether a message is ours.** `is_own` is set
//!    explicitly with [`Message::mark_own`] (or [`Message::with_own`]) once the
//!    caller knows the signed-in user's MRI. We do not guess from the sender
//!    name.

use serde::{Deserialize, Serialize};

use ost::api::{
    ChannelInfo, ChatInfo, ChatMemberInfo, ConversationFolder as WireConversationFolder,
    FileVersion as WireFileVersion, MeetingInfo as WireMeetingInfo, MessageInfo, PinnedRef,
    PublicTeamInfo, ReactionCount, Reactor as WireReactor, SharedFile as WireSharedFile, TeamInfo,
    TeamMemberInfo, TeamMemberSettings, UserInfo,
};

/// Delivery state of a message we authored (milestone **M5**).
///
/// Optimistic sending writes a locally-authored message to the store *before*
/// the network call; this enum is what the UI renders a pending/failed bubble
/// from. Messages that arrive from the wire (history or realtime) are always
/// [`SendState::Sent`]. The value is persisted in the `send_state` column
/// (schema v2) as its lowercase name; an unknown/foreign value degrades to
/// [`SendState::Sent`] rather than failing a read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendState {
    /// Posted locally, awaiting the server's confirmation.
    Pending,
    /// Confirmed by the server (the default, and every wired-in message).
    #[default]
    Sent,
    /// The POST failed; the row is retained so the UI can retry.
    Failed,
}

impl SendState {
    /// The lowercase name stored in the database / carried in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            SendState::Pending => "pending",
            SendState::Sent => "sent",
            SendState::Failed => "failed",
        }
    }

    /// Parse a stored value; anything unrecognized (including `NULL` mapped to
    /// `""`) degrades to [`SendState::Sent`].
    pub fn from_db(value: &str) -> Self {
        match value {
            "pending" => SendState::Pending,
            "failed" => SendState::Failed,
            _ => SendState::Sent,
        }
    }

    /// Whether the message is still awaiting a server id.
    pub fn is_pending(self) -> bool {
        matches!(self, SendState::Pending)
    }
}

// ----- identity (M13) ------------------------------------------------------

/// The signed-in user's identity (milestone **M13**).
///
/// Resolved from Graph `/me` (`ost::api::whoami_data`) after sign-in and cached
/// both in memory ([`Session`](crate::Session)) and in the `accounts` row
/// (schema v8). The MRI is what [`Message::mark_own`] compares a message's
/// sender against, so knowing it is what makes **server-loaded history and
/// realtime messages** render on the correct side (the M5 optimistic paths set
/// `is_own` directly, but nothing else could).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// Our MRI (`8:orgid:<guid>`); empty when whoami named no id.
    pub mri: String,
    /// Our AAD object id (the guid part of the MRI).
    pub oid: String,
    /// Our display name (`"User"` when Graph named none).
    pub display_name: String,
    /// The AAD tenant the tokens were minted against.
    pub tenant: String,
}

impl Identity {
    /// Build from the vendored whoami [`UserInfo`] plus the tenant.
    ///
    /// The MRI is derived with the vendored [`ost::api::own_member_mri`]
    /// (`8:orgid:<oid>`); an empty id yields an empty MRI rather than a bogus
    /// `8:orgid:` one.
    pub fn from_whoami(info: &UserInfo, tenant: impl Into<String>) -> Self {
        let oid = info.id.trim().to_string();
        let mri = if oid.is_empty() {
            String::new()
        } else {
            ost::api::own_member_mri(&oid)
        };
        Identity {
            mri,
            oid,
            display_name: info.display_name.clone(),
            tenant: tenant.into(),
        }
    }

    /// Whether this identity carries a usable MRI.
    pub fn has_mri(&self) -> bool {
        !self.mri.trim().is_empty()
    }
}

/// A chat / thread shown in the chat list.
///
/// The first six fields mirror `ost::api::ChatInfo`. `unread` and `pinned` are
/// **local-only UI flags**: the server payload does not carry them, they are
/// not populated by [`Chat::from_info`], and the M3 schema does not persist them
/// yet (a later migration will). They exist so the UI does not have to shadow
/// these per-chat booleans in a side map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chat {
    /// Thread / conversation id (e.g. `19:…@unq.gbl.spaces`).
    pub id: String,
    /// Display name (topic, resolved mate, last sender or a system label).
    pub name: String,
    /// Whether this is a group conversation (not 1:1).
    pub is_group: bool,
    /// ISO-8601 timestamp of the newest message, when the server sent one.
    pub last_message_time: Option<String>,
    /// Display name of the newest message's sender.
    pub last_message_sender: Option<String>,
    /// One-line preview of the newest message (HTML stripped).
    pub last_message_preview: Option<String>,
    /// Local UI flag: unseen activity not yet cleared by the UI. Not persisted
    /// in M3 and not populated from the wire.
    pub unread: bool,
    /// Local UI flag: pinned to the top of the list. Not persisted in M3 and
    /// not populated from the wire.
    pub pinned: bool,
}

impl Chat {
    /// Build a domain [`Chat`] from a protocol [`ChatInfo`].
    ///
    /// The local-only flags default to `false`.
    pub fn from_info(info: &ChatInfo) -> Self {
        Chat {
            id: info.id.clone(),
            name: info.name.clone(),
            is_group: info.is_group,
            last_message_time: info.last_message_time.clone(),
            last_message_sender: info.last_message_sender.clone(),
            last_message_preview: info.last_message_preview.clone(),
            unread: false,
            pinned: false,
        }
    }
}

impl From<&ChatInfo> for Chat {
    fn from(info: &ChatInfo) -> Self {
        Chat::from_info(info)
    }
}

/// One emoji reaction on a message, with its reactor list.
///
/// Mirrors `ost::api::ReactionCount`, but owned and serde-serializable so the
/// store can round-trip it as a JSON column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    /// The picker emoji (e.g. `👍`).
    pub emoji: String,
    /// Total reactors for this emoji (may exceed `reactors.len()` when the wire
    /// names fewer reactors than the count).
    pub count: usize,
    /// Who reacted, when the wire named them.
    pub reactors: Vec<Reactor>,
}

/// One person who reacted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reactor {
    /// The user's id as the wire names it (an MRI or a Graph user id).
    pub id: String,
    /// Display name when known, else empty.
    pub name: String,
}

impl From<&ReactionCount> for Reaction {
    fn from(r: &ReactionCount) -> Self {
        Reaction {
            emoji: r.emoji.clone(),
            count: r.count,
            reactors: r.reactors.iter().map(Reactor::from).collect(),
        }
    }
}

impl From<&WireReactor> for Reactor {
    fn from(r: &WireReactor) -> Self {
        Reactor {
            id: r.id.clone(),
            name: r.name.clone(),
        }
    }
}

/// Whether `id` names a channel conversation (milestone **M6**).
///
/// Delegates to the vendored [`ost::api::is_channel_conversation_id`]: a channel
/// thread id starts with `19:` and ends with `@thread.tacv2` (modern) or
/// `@thread.skype` (legacy). Group chats (`@thread.v2`), meetings and 1:1s are
/// **not** channels. Pure, so it is trivially testable.
pub fn is_channel_conversation_id(id: &str) -> bool {
    ost::api::is_channel_conversation_id(id)
}

/// A team the signed-in user has joined (milestone **M6**).
///
/// `id` is the **Graph group id** the `/me/joinedTeams` listing uses. Channel
/// routing needs the *Teams* thread id (`19:…@thread…`) instead; that is
/// resolved on demand with [`Teams::team_internal_id`](crate::teams::Teams) or
/// the vendored `team_thread_id` (see `docs/DECISIONS.md`, channel routing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    /// Graph group id of the team.
    pub id: String,
    /// Display name (`displayName`, falling back to the id).
    pub name: String,
}

impl Team {
    /// Build a domain [`Team`] from a protocol `TeamInfo`.
    pub fn from_info(info: &TeamInfo) -> Self {
        Team {
            id: info.id.clone(),
            name: info.name.clone(),
        }
    }
}

impl From<&TeamInfo> for Team {
    fn from(info: &TeamInfo) -> Self {
        Team::from_info(info)
    }
}

/// One channel inside a team (milestone **M6**).
///
/// The first six fields mirror `ost::api::ChannelInfo`; `team_id` is attached by
/// the caller (the wire `ChannelInfo` does not carry it) and `is_channel` is
/// derived from the id with [`is_channel_conversation_id`] — a channel that is
/// not a `@thread.tacv2`/`@thread.skype` conversation (an unusual/legacy id)
/// still lists, but `is_channel` is `false` so callers do not mistake it for a
/// channel *conversation* they can post into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    /// Channel thread id (`19:…@thread.tacv2`).
    pub id: String,
    /// Owning team's Graph group id.
    pub team_id: String,
    /// Display name (`displayName`, falling back to the id).
    pub name: String,
    /// Graph `description`, when set.
    pub description: Option<String>,
    /// Graph `membershipType` (`standard`/`private`/`shared`).
    pub membership_type: Option<String>,
    /// Graph `webUrl` (open-in-browser deep link).
    pub web_url: Option<String>,
    /// Local: whether [`Channel::id`] is a channel conversation id (see above).
    pub is_channel: bool,
}

impl Channel {
    /// Build a domain [`Channel`] from a protocol `ChannelInfo`, attaching the
    /// owning `team_id` and deriving `is_channel` from the id.
    pub fn from_info(team_id: impl Into<String>, info: &ChannelInfo) -> Self {
        Channel {
            id: info.id.clone(),
            team_id: team_id.into(),
            name: info.name.clone(),
            description: info.description.clone(),
            membership_type: info.membership_type.clone(),
            web_url: info.web_url.clone(),
            is_channel: is_channel_conversation_id(&info.id),
        }
    }
}

impl From<(&str, &ChannelInfo)> for Channel {
    fn from((team_id, info): (&str, &ChannelInfo)) -> Self {
        Channel::from_info(team_id, info)
    }
}

/// One team roster entry (milestone **M6**).
///
/// Mirrors `ost::api::TeamMemberInfo`. `id` is the **membership** id (the DELETE
/// target), not the user id; `user_id` is the Graph/AAD user id when the roster
/// named one (used to match the signed-in identity for "teams you own" — B4);
/// `roles` carries Graph role names and contains `"owner"` for team owners. The
/// owner flag is derivable from `roles` and is deliberately not duplicated here.
///
/// The store does **not** persist `user_id` (the `team_members` table predates
/// it), so a roster read from the cache leaves it `None`; a live
/// [`Teams::members`](crate::Teams::members) fills it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    /// Membership id (not the user id).
    pub id: String,
    /// Graph/AAD user id, when the roster named one (not persisted).
    #[serde(default)]
    pub user_id: Option<String>,
    /// Display name (`displayName`, falling back to the membership id).
    pub display_name: String,
    /// Email address, when Graph named one.
    pub email: Option<String>,
    /// Graph role names (`["owner"]` for owners, empty for members).
    pub roles: Vec<String>,
}

impl TeamMember {
    /// Build a domain [`TeamMember`] from a protocol `TeamMemberInfo`.
    pub fn from_info(info: &TeamMemberInfo) -> Self {
        TeamMember {
            id: info.id.clone(),
            user_id: info.user_id.clone(),
            display_name: info.display_name.clone(),
            email: info.email.clone(),
            roles: info.roles.clone(),
        }
    }

    /// Whether this member holds the `owner` role.
    pub fn is_owner(&self) -> bool {
        self.roles.iter().any(|r| r == "owner")
    }
}

impl From<&TeamMemberInfo> for TeamMember {
    fn from(info: &TeamMemberInfo) -> Self {
        TeamMember::from_info(info)
    }
}

// ----- team administration (B4) -------------------------------------------

/// One joinable (public) team from the **B4** directory search.
///
/// Mirrors `ost::api::PublicTeamInfo` (Graph `/groups` `$search`, falling back
/// to `/teams` `startswith`) and adds one local field:
/// [`is_joined`](PublicTeam::is_joined). The wire row carries **no** membership
/// flag, so the caller marks it against its own joined-teams list (the
/// [`Teams`](crate::Teams) service does this from the store cache); a bare
/// [`From<&PublicTeamInfo>`] conversion therefore defaults it to `false` rather
/// than guessing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicTeam {
    /// Graph group id of the team (also the join target).
    pub id: String,
    /// Display name (`displayName`, falling back to the id).
    pub display_name: String,
    /// Graph `description`, when set.
    pub description: Option<String>,
    /// Graph `visibility` (the vendored parser keeps only `public`).
    pub visibility: String,
    /// Local: whether the signed-in user has already joined this team. Not sent
    /// by the wire; set by the caller against the joined list.
    pub is_joined: bool,
}

impl PublicTeam {
    /// Build a domain [`PublicTeam`] from a protocol `PublicTeamInfo`, attaching
    /// the caller-supplied membership flag (the wire does not carry one).
    pub fn from_info(info: &PublicTeamInfo, is_joined: bool) -> Self {
        PublicTeam {
            id: info.id.clone(),
            display_name: info.name.clone(),
            description: info.description.clone(),
            visibility: info.visibility.clone(),
            is_joined,
        }
    }
}

impl From<&PublicTeamInfo> for PublicTeam {
    /// Membership is unknown from the wire, so [`is_joined`](PublicTeam::is_joined)
    /// defaults to `false`; use [`PublicTeam::from_info`] to mark it.
    fn from(info: &PublicTeamInfo) -> Self {
        PublicTeam::from_info(info, false)
    }
}

/// A team's member permissions + General channel (backlog item **B4**).
///
/// Mirrors `ost::api::TeamMemberSettings` (two Graph GETs: `GET /teams/{id}`
/// for `memberSettings`, `GET /teams/{id}/primaryChannel` for the General
/// channel). Each field is `None` when the read failed or the field was absent
/// — **unknown**, deliberately distinct from `false`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamSettings {
    /// Whether non-owners may delete channels (`memberSettings.allowDeleteChannels`).
    pub allow_delete_channels: Option<bool>,
    /// Whether non-owners may create/update channels
    /// (`memberSettings.allowCreateUpdateChannels`).
    pub allow_create_update_channels: Option<bool>,
    /// Id of the team's General (primary) channel, when the read succeeded.
    pub primary_channel_id: Option<String>,
}

impl TeamSettings {
    /// Build a domain [`TeamSettings`] from the vendored projection.
    pub fn from_wire(settings: &TeamMemberSettings) -> Self {
        TeamSettings {
            allow_delete_channels: settings.allow_delete_channels,
            allow_create_update_channels: settings.allow_create_update_channels,
            primary_channel_id: settings.primary_channel_id.clone(),
        }
    }
}

impl From<&TeamMemberSettings> for TeamSettings {
    fn from(settings: &TeamMemberSettings) -> Self {
        TeamSettings::from_wire(settings)
    }
}

// ----- people (B3) --------------------------------------------------------

/// One directory person from the **B3** people search (Graph `/users`).
///
/// Built from the vendored `ost::api::TeamMemberInfo` the people search
/// projects. The directory `$select` is `id,displayName,mail,userPrincipalName`,
/// so `id` is the Graph user id, `display_name` falls back to the id, and
/// `email` is `mail` when present else `userPrincipalName` (the vendored
/// mapping folds the two). The `roles`/`is_owner` a roster entry carries are
/// **not** meaningful for a directory hit and are dropped here.
///
/// One wire fact is documented rather than guessed: the `$select` does **not**
/// request a job title, so [`job_title`](Person::job_title) is modeled for
/// forward-compatibility and is always `None` from [`Person::from_member`] until
/// the vendored projection carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    /// Graph user id.
    pub id: String,
    /// Display name (`displayName`, falling back to the id).
    pub display_name: String,
    /// Email (`mail`, falling back to `userPrincipalName`), when either was set.
    pub email: Option<String>,
    /// Job title. **Not carried** by the people-search projection today (the
    /// `$select` omits it); always `None` from the wire.
    pub job_title: Option<String>,
}

impl Person {
    /// Build a domain [`Person`] from a protocol `TeamMemberInfo`.
    ///
    /// `job_title` defaults to `None` (the projection does not carry one — see
    /// the type docs).
    pub fn from_member(info: &TeamMemberInfo) -> Self {
        Person {
            id: info.id.clone(),
            display_name: info.display_name.clone(),
            email: info.email.clone(),
            job_title: None,
        }
    }

    /// A short `"Name <email>"` label for the picker (the email is omitted when
    /// unknown).
    pub fn label(&self) -> String {
        match self
            .email
            .as_deref()
            .filter(|email| !email.trim().is_empty())
        {
            Some(email) => format!("{} <{}>", self.display_name, email),
            None => self.display_name.clone(),
        }
    }
}

impl From<&TeamMemberInfo> for Person {
    fn from(info: &TeamMemberInfo) -> Self {
        Person::from_member(info)
    }
}

// ----- chat roster (B2a) --------------------------------------------------

/// One member of a **chat** roster (backlog item **B2a**).
///
/// Built from the vendored `ost::api::ChatMemberInfo` the chat-service thread
/// roster (`GET /v1/threads/{id}/members`) and the Graph `conversationMember`
/// projection both produce. A roster entry is **not** a directory [`Person`]:
/// its identity is the chat-service **MRI** (`8:orgid:<guid>`, the value the
/// leave path deletes and the message `from` link carries), it carries
/// roster **roles** and an owner flag, and the chat service often has no
/// display name (the vendored call fills blanks from Graph). So it gets its own
/// type; [`Person`] remains the directory-search shape the people picker uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMember {
    /// Chat-service identity (`8:orgid:<guid>`); the leave target.
    pub mri: String,
    /// Graph/AAD user id (the guid part of [`mri`](ChatMember::mri)), when the
    /// wire named one (the presence key).
    pub id: Option<String>,
    /// Display name; empty when the roster carried none and the Graph fill
    /// failed (the row then falls back to the email or a placeholder).
    pub display_name: String,
    /// Email, when the Graph fill resolved one.
    pub email: Option<String>,
    /// Roster role names, lowercased (`["admin"]`/`["user"]` from the chat
    /// service, `["owner"]`/`["member"]` from Graph).
    pub roles: Vec<String>,
    /// Whether the wire marked this member an owner (`Admin` role on the chat
    /// service, `owner` on Graph).
    pub is_owner: bool,
}

impl ChatMember {
    /// Build a domain [`ChatMember`] from a protocol `ChatMemberInfo`.
    pub fn from_info(info: &ChatMemberInfo) -> Self {
        ChatMember {
            mri: info.mri.clone(),
            id: info.user_id.clone(),
            display_name: info.display_name.clone(),
            email: info.email.clone(),
            roles: info.roles.clone(),
            is_owner: info.is_owner,
        }
    }

    /// A short `"Name <email>"` label for a roster row (the email is omitted
    /// when unknown; the MRI is shown when there is no name at all).
    pub fn label(&self) -> String {
        let name = self.display_name.trim();
        if !name.is_empty() {
            match self.email.as_deref().filter(|e| !e.trim().is_empty()) {
                Some(email) => format!("{name} <{email}>"),
                None => name.to_string(),
            }
        } else if let Some(email) = self.email.as_deref().filter(|e| !e.trim().is_empty()) {
            email.to_string()
        } else {
            self.mri.clone()
        }
    }
}

impl From<&ChatMemberInfo> for ChatMember {
    fn from(info: &ChatMemberInfo) -> Self {
        ChatMember::from_info(info)
    }
}

// ----- folders & pins (B2b) -----------------------------------------------

/// One chat **folder** (backlog item **B2b**).
///
/// Mirrors the vendored `ost::api::ConversationFolder` but is owned and
/// protocol-independent. `item_ids` are the conversations the folder holds (in
/// wire order); the [`Folders`](crate::Folders) service rebuilds them from the
/// store's per-chat `folder_id` when it returns cached folders.
///
/// **System folders vs. user folders.** Teams returns both the folders a chat
/// can be *moved into* (Favorites and user-created folders) and a set of
/// server-generated **views** ([`SYSTEM_FOLDER_TYPES`](ost::api::SYSTEM_FOLDER_TYPES):
/// RecentChats, TeamsAndChannels, QuickViews, MutedChats, MeetingChats,
/// EngageCommunities) that are not move targets. The vendored parser already
/// **drops** the views, so a parsed folder is normally `is_system == false`;
/// [`is_system`](ChatFolder::is_system) is computed anyway (case-insensitively)
/// so a hand-built or future folder is classified correctly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatFolder {
    /// Server folder id (the move target; e.g. `t~u~Favorites`).
    pub id: String,
    /// Display name (the vendored parser falls back to the folder type).
    pub name: String,
    /// Server folder type (`Favorites`, `UserCreated`, …); `""` when unnamed.
    pub folder_type: String,
    /// Whether [`folder_type`](ChatFolder::folder_type) is one of the
    /// server-generated views ([`SYSTEM_FOLDER_TYPES`](ost::api::SYSTEM_FOLDER_TYPES))
    /// that a chat cannot be moved into.
    pub is_system: bool,
    /// Conversation ids in the folder (wire order; rebuilt from the store for
    /// cached folders).
    pub item_ids: Vec<String>,
}

impl ChatFolder {
    /// Build a domain [`ChatFolder`] from a protocol `ConversationFolder`.
    pub fn from_wire(folder: &WireConversationFolder) -> Self {
        ChatFolder {
            id: folder.id.clone(),
            name: folder.name.clone(),
            folder_type: folder.folder_type.clone(),
            is_system: is_system_folder(&folder.folder_type),
            item_ids: folder.item_ids.clone(),
        }
    }
}

impl From<&WireConversationFolder> for ChatFolder {
    fn from(folder: &WireConversationFolder) -> Self {
        ChatFolder::from_wire(folder)
    }
}

/// Whether `folder_type` names a server-generated **view** (not a move target).
///
/// Case-insensitive, delegating to the vendored
/// [`ost::api::SYSTEM_FOLDER_TYPES`] list. Pure and total.
pub fn is_system_folder(folder_type: &str) -> bool {
    ost::api::SYSTEM_FOLDER_TYPES
        .iter()
        .any(|t| t.eq_ignore_ascii_case(folder_type.trim()))
}

/// One server-side **pinned message** (backlog item **B2b**).
///
/// Built from a vendored `ost::api::PinnedRef`; `message` is attached by the
/// [`Pins`](crate::Pins) service when the pinned message is also cached in the
/// store (it is **not** persisted with the pin — the store keeps the pin
/// membership fields only, and the message body is resolved on read).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedMessage {
    /// Owning conversation.
    pub chat_id: String,
    /// Pinned message's server id (the jump target).
    pub message_id: String,
    /// Sender display name, when the wire named one.
    pub sender: Option<String>,
    /// One-line plain-text preview (HTML stripped), when available.
    pub preview: Option<String>,
    /// The pinned message's arrival time (ISO-8601), when available.
    pub time: Option<String>,
    /// When the message was pinned (ISO-8601), when the wire carries it.
    pub pinned_at: Option<String>,
    /// Who pinned it, when the wire carries it.
    pub pinned_by: Option<String>,
    /// Graph `pinnedChatMessageInfo` id, when this pin came from Graph (the
    /// value Graph's unpin DELETE takes). `None` for chat-service pins.
    pub graph_pin_id: Option<String>,
    /// The cached [`Message`] when it is present in the store; `None` when the
    /// pinned message is outside the cached window.
    #[serde(default)]
    pub message: Option<Message>,
}

impl PinnedMessage {
    /// Build a domain [`PinnedMessage`] from a protocol `PinnedRef`, attaching
    /// the owning `chat_id`. `message` starts `None` (see the type docs).
    pub fn from_wire(chat_id: impl Into<String>, pin: &PinnedRef) -> Self {
        PinnedMessage {
            chat_id: chat_id.into(),
            message_id: pin.message_id.clone(),
            sender: pin.sender.clone(),
            preview: pin.preview.clone(),
            time: pin.time.clone(),
            pinned_at: pin.pinned_at.clone(),
            pinned_by: pin.pinned_by.clone(),
            graph_pin_id: pin.graph_pin_id.clone(),
            message: None,
        }
    }

    /// Attach the cached message this pin refers to.
    pub fn with_message(mut self, message: Message) -> Self {
        self.message = Some(message);
        self
    }

    /// A one-line preview: the wire preview, else the cached message's text,
    /// else an empty string.
    pub fn preview_text(&self) -> String {
        if let Some(preview) = self.preview.as_deref().filter(|p| !p.trim().is_empty()) {
            return preview.to_string();
        }
        self.message
            .as_ref()
            .map(|m| m.content.clone())
            .unwrap_or_default()
    }
}

impl From<(&str, &PinnedRef)> for PinnedMessage {
    fn from((chat_id, pin): (&str, &PinnedRef)) -> Self {
        PinnedMessage::from_wire(chat_id, pin)
    }
}

// ----- files (M7) ---------------------------------------------------------

/// One file shared in a chat/channel or listed from a drive (milestone **M7**).
///
/// Mirrors the vendored `ost::api::SharedFile` but is owned and
/// protocol-independent, so the store and the UI never touch the wire
/// projection. [`FileEntry::drive_id`] is optional: an unresolved chat
/// attachment (a file that could not be resolved to a driveItem — deleted, or
/// no access) lists from message metadata alone, so it can be opened in a
/// browser but not downloaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// DriveItem id (or, for an unresolved attachment, its attachment id/URL).
    pub id: String,
    /// Owning drive id, when known.
    pub drive_id: Option<String>,
    /// Display name (never empty; the wire falls back to `[unnamed]`).
    pub name: String,
    /// Size in bytes (`0` when unknown: a folder or an unresolved attachment).
    pub size: u64,
    /// MIME type, when Graph named one.
    pub mime: Option<String>,
    /// Open-in-browser URL.
    pub web_url: Option<String>,
    /// Direct download URL (Graph `@microsoft.graph.downloadUrl`), when present.
    pub download_url: Option<String>,
    /// True when this driveItem is a folder (no size/mime; drill in with
    /// [`Files::list_folder`](crate::Files::list_folder)).
    pub is_folder: bool,
    /// ISO-8601 creation time, when known.
    pub created: Option<String>,
    /// ISO-8601 last-modified time, when known.
    pub modified: Option<String>,
    /// Display name of the chat sender (chat listings only; `None` elsewhere).
    pub sender: Option<String>,
    /// Cached sharing link (set by
    /// [`Files::create_link`](crate::Files::create_link); the list paths never
    /// fill it).
    pub share_url: Option<String>,
    /// File-attachment GUID mined from the driveItem eTag; matches the
    /// `<attachment id>` in the message that shared the file.
    pub attachment_id: Option<String>,
}

impl FileEntry {
    /// Build a domain [`FileEntry`] from a protocol `SharedFile`.
    pub fn from_shared(file: &WireSharedFile) -> Self {
        FileEntry {
            id: file.id.clone(),
            drive_id: file.drive_id.clone(),
            name: file.name.clone(),
            size: file.size,
            mime: file.mime.clone(),
            web_url: file.web_url.clone(),
            download_url: file.download_url.clone(),
            is_folder: file.is_folder,
            created: file.created.clone(),
            modified: file.modified.clone(),
            sender: file.sender.clone(),
            share_url: file.share_url.clone(),
            attachment_id: file.attachment_id.clone(),
        }
    }
}

impl From<&WireSharedFile> for FileEntry {
    fn from(file: &WireSharedFile) -> Self {
        FileEntry::from_shared(file)
    }
}

impl From<WireSharedFile> for FileEntry {
    fn from(file: WireSharedFile) -> Self {
        FileEntry::from_shared(&file)
    }
}

/// One version in a drive item's version history (milestone **M7**).
///
/// Mirrors the vendored `ost::api::FileVersion`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileVersion {
    /// Version id (Graph `versions/{id}`).
    pub id: String,
    /// Size in bytes (`0` when Graph omitted it).
    pub size: u64,
    /// ISO-8601 last-modified time, when known.
    pub modified: Option<String>,
    /// Display name of the last editor, when known.
    pub modified_by: Option<String>,
}

impl FileVersion {
    /// Build a domain [`FileVersion`] from a protocol `FileVersion`.
    pub fn from_wire(version: &WireFileVersion) -> Self {
        FileVersion {
            id: version.id.clone(),
            size: version.size,
            modified: version.modified.clone(),
            modified_by: version.modified_by.clone(),
        }
    }
}

impl From<&WireFileVersion> for FileVersion {
    fn from(version: &WireFileVersion) -> Self {
        FileVersion::from_wire(version)
    }
}

/// A sharing link for one drive item (Graph `createLink`), milestone **M7**.
///
/// Defined here (not reused from the vendored crate) because the vendored
/// `SharedLink` is not re-exported by `ost::api`; the [`ApiClient`] wrapper maps
/// it into this owned type.
///
/// [`ApiClient`]: crate::ApiClient
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedLink {
    /// The shareable URL.
    pub url: String,
    /// The link scope Graph reported (`anonymous` / `organization`).
    pub scope: Option<String>,
}

// ----- calls (M9) ---------------------------------------------------------

/// Which side of a call we are on (milestone **M9**).
///
/// Persisted in the schema-v6 `call_log` table as the lowercase name; an
/// unknown/foreign value degrades to [`CallDirection::Outgoing`] rather than
/// failing a read (call history is a convenience, not a critical record).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallDirection {
    /// We placed the call.
    #[default]
    Outgoing,
    /// Someone called us.
    Incoming,
}

impl CallDirection {
    /// The lowercase name stored in the database / carried in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            CallDirection::Outgoing => "outgoing",
            CallDirection::Incoming => "incoming",
        }
    }

    /// Parse a stored value; anything unrecognized degrades to `Outgoing`.
    pub fn from_db(value: &str) -> Self {
        match value {
            "incoming" => CallDirection::Incoming,
            _ => CallDirection::Outgoing,
        }
    }
}

/// How a logged call ended (milestone **M9**).
///
/// Persisted in `call_log.outcome` as the lowercase name; an unknown/foreign
/// value (or a call still in progress) degrades to [`CallOutcome::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallOutcome {
    /// The call connected and was hung up normally.
    Completed,
    /// An incoming call was declined before it connected.
    Declined,
    /// An incoming call rang out (or was declined) and was never answered.
    Missed,
    /// Placing/accepting the call failed.
    Failed,
    /// We cancelled a call while it was still ringing/connecting.
    Cancelled,
    /// In progress, or an outcome we do not model.
    #[default]
    Unknown,
}

impl CallOutcome {
    /// The lowercase name stored in the database / carried in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            CallOutcome::Completed => "completed",
            CallOutcome::Declined => "declined",
            CallOutcome::Missed => "missed",
            CallOutcome::Failed => "failed",
            CallOutcome::Cancelled => "cancelled",
            CallOutcome::Unknown => "unknown",
        }
    }

    /// Parse a stored value; anything unrecognized degrades to `Unknown`.
    pub fn from_db(value: &str) -> Self {
        match value {
            "completed" => CallOutcome::Completed,
            "declined" => CallOutcome::Declined,
            "missed" => CallOutcome::Missed,
            "failed" => CallOutcome::Failed,
            "cancelled" => CallOutcome::Cancelled,
            _ => CallOutcome::Unknown,
        }
    }

    /// Whether the call is still in progress (no terminal outcome yet).
    pub fn is_open(self) -> bool {
        matches!(self, CallOutcome::Unknown)
    }
}

/// One row of local call history (milestone **M9**, schema v6).
///
/// A purely local record: Teams does not expose a call-history API through the
/// reverse-engineered surfaces, so this is what the UI's recents list reads.
/// `participants` are the display names / MRIs the invitation or the placement
/// named, stored as a JSON array. `id` is the call id when the wire named one
/// (an incoming invitation's `debugContent.callId`), else a generated uuid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallLogEntry {
    /// Stable call id (wire `callId` when known, else a generated uuid).
    pub id: String,
    /// The thread/chat the call belongs to (may be empty when unknown).
    pub thread_id: String,
    /// Whether we placed or received the call.
    pub direction: CallDirection,
    /// Unix seconds when the call started (placing/ringing).
    pub started_at: i64,
    /// Unix seconds when the call ended, when known.
    pub ended_at: Option<i64>,
    /// Talk time in seconds, when known.
    pub duration_secs: Option<u64>,
    /// Display names / MRIs of the participants, when known.
    pub participants: Vec<String>,
    /// How the call ended (or [`CallOutcome::Unknown`] while in progress).
    pub outcome: CallOutcome,
}

impl CallLogEntry {
    /// Begin a call-history row.
    pub fn started(
        id: impl Into<String>,
        thread_id: impl Into<String>,
        direction: CallDirection,
        started_at: i64,
    ) -> Self {
        Self {
            id: id.into(),
            thread_id: thread_id.into(),
            direction,
            started_at,
            ended_at: None,
            duration_secs: None,
            participants: Vec::new(),
            outcome: CallOutcome::Unknown,
        }
    }

    /// Attach the participant names/MRIs.
    pub fn with_participants(mut self, participants: Vec<String>) -> Self {
        self.participants = participants;
        self
    }

    /// Mark the call finished: set `ended_at`, derive `duration_secs` from
    /// `started_at` (never negative) and record the outcome.
    pub fn finished(mut self, ended_at: i64, outcome: CallOutcome) -> Self {
        self.ended_at = Some(ended_at);
        self.duration_secs = Some(ended_at.saturating_sub(self.started_at).max(0) as u64);
        self.outcome = outcome;
        self
    }
}

// ----- calendar (M12) -----------------------------------------------------

/// One calendar meeting (milestone **M12**).
///
/// Owned mirror of the vendored `ost::api::MeetingInfo` (a Graph `calendarView`
/// event), so the store and the UI never touch the wire projection. The two
/// **wire facts** to orient on (verified against
/// `crates/squads-protocol/src/api/calendar.rs`, not guessed):
///
/// 1. `start`/`end` are the Graph `dateTime` values **verbatim**
///    (`YYYY-MM-DDTHH:MM:SS.fffffff`, no timezone suffix); the event's `timeZone`
///    is dropped by the vendored projection. They sort lexicographically, which
///    is what the store orders and filters on.
/// 2. The projection carries only `id`, `subject`, `start`, `end`, `join_url`,
///    `organizer`, `organizer_email`, `is_organizer` and `is_online`.
///    [`all_day`](Meeting::all_day), [`join_meeting_id`](Meeting::join_meeting_id),
///    [`passcode`](Meeting::passcode), [`web_link`](Meeting::web_link) and
///    [`location`](Meeting::location) are modeled for forward-compatibility but
///    are **not** populated by [`Meeting::from_info`] (they default to
///    `false`/`None`) until the vendored `$select`/projection carries them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meeting {
    /// Graph event id (the DELETE target for a cancel).
    pub id: String,
    /// Subject (`"(no subject)"` when the event had a blank one).
    pub subject: String,
    /// `start.dateTime` verbatim (`YYYY-MM-DDTHH:MM:SS.fffffff`), when set.
    pub start: Option<String>,
    /// `end.dateTime` verbatim, when set.
    pub end: Option<String>,
    /// Whether the event is an all-day event. **Not carried** by the vendored
    /// projection today (see the type docs); always `false` from the wire.
    pub all_day: bool,
    /// Organizer display name, when set.
    pub organizer: Option<String>,
    /// Organizer SMTP address (`organizer.emailAddress.address`), when set.
    pub organizer_email: Option<String>,
    /// Graph `isOrganizer`: the signed-in user organizes this event.
    pub is_organizer: bool,
    /// Teams join URL (`onlineMeeting.joinUrl`), when the event has one.
    pub join_url: Option<String>,
    /// Numeric meeting id. **Not carried** by the vendored projection today
    /// (see the type docs); always `None` from the wire.
    pub join_meeting_id: Option<String>,
    /// Meeting passcode. **Not carried** by the vendored projection today;
    /// always `None` from the wire.
    pub passcode: Option<String>,
    /// Graph `webLink` (open-in-browser deep link). **Not carried** by the
    /// vendored projection today (the field is selected but dropped when
    /// mapping); always `None` from the wire.
    pub web_link: Option<String>,
    /// Free-form event location. **Not carried** by the vendored projection
    /// today; always `None` from the wire.
    pub location: Option<String>,
    /// True when Graph flags the event as an online meeting.
    pub is_online: bool,
}

impl Meeting {
    /// Build a domain [`Meeting`] from a protocol `MeetingInfo`.
    ///
    /// The five fields the vendored projection does not carry default to
    /// `false`/`None` (see the type docs).
    pub fn from_info(info: &WireMeetingInfo) -> Self {
        Meeting {
            id: info.id.clone(),
            subject: info.subject.clone(),
            start: info.start.clone(),
            end: info.end.clone(),
            all_day: false,
            organizer: info.organizer.clone(),
            organizer_email: info.organizer_email.clone(),
            is_organizer: info.is_organizer,
            join_url: info.join_url.clone(),
            join_meeting_id: None,
            passcode: None,
            web_link: None,
            location: None,
            is_online: info.is_online,
        }
    }

    /// Whether the meeting can be joined by a direct call leg (its join URL
    /// resolves to a native thread id).
    ///
    /// Delegates to the vendored [`ost::api::parse_join_url`], so a
    /// `teams.microsoft.com/l/meetup-join/19:…@thread…` link is joinable while
    /// a bare `https://` link (or no link at all) is not.
    pub fn is_joinable(&self) -> bool {
        self.join_url
            .as_deref()
            .map(|url| ost::api::parse_join_url(url).kind == "thread")
            .unwrap_or(false)
    }
}

impl From<&WireMeetingInfo> for Meeting {
    fn from(info: &WireMeetingInfo) -> Self {
        Meeting::from_info(info)
    }
}

/// Redacting `Debug`: prints identity, subject and timing, but not the join URL
/// or passcode (a join URL can embed a passcode — it must not land in logs).
impl std::fmt::Debug for Meeting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Meeting")
            .field("id", &self.id)
            .field("subject", &self.subject)
            .field("start", &self.start)
            .field("end", &self.end)
            .field("all_day", &self.all_day)
            .field("organizer", &self.organizer)
            .field("is_organizer", &self.is_organizer)
            .field("has_join_url", &self.join_url.is_some())
            .field("is_online", &self.is_online)
            .finish()
    }
}

/// One chat message in the timeline.
///
/// `chat_id` is the owning thread: [`MessageInfo`] does not carry it, so it is
/// attached by the caller via [`Message::from_info`] / [`Message::with_chat_id`].
///
/// `edited` and `is_own` are local state (see the module docs). `raw` holds the
/// server HTML; the custom [`std::fmt::Debug`] deliberately omits it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Server message id.
    pub id: String,
    /// Owning chat / thread id.
    pub chat_id: String,
    /// Sender MRI (`8:orgid:<guid>`); `""` when the server omitted `from`.
    pub sender_mri: String,
    /// Sender display name (`"?"` when unknown, matching the protocol crate).
    pub sender_name: String,
    /// ISO-8601 arrival timestamp (or compose time), `""` when unknown.
    pub timestamp: String,
    /// Plain-text body (HTML stripped).
    pub content: String,
    /// Unstripped server HTML (mentions, code blocks, images live here).
    pub raw: String,
    /// Grouped reaction counts.
    pub reactions: Vec<Reaction>,
    /// Parent message id for quote / thread replies.
    pub reply_to: Option<String>,
    /// Idempotency key for our own sends.
    pub client_message_id: Option<String>,
    /// Local: the message has been edited (see module docs).
    pub edited: bool,
    /// Local: the message was sent by the signed-in user.
    pub is_own: bool,
    /// Local: optimistic-send delivery state (M5). Defaults to
    /// [`SendState::Sent`], which is correct for every wired-in message.
    #[serde(default)]
    pub send_state: SendState,
}

impl Message {
    /// Build a domain [`Message`] from a protocol [`MessageInfo`], attaching
    /// the owning `chat_id`.
    ///
    /// `edited` and `is_own` default to `false` (the wire cannot tell us); set
    /// them with [`Message::mark_edited`] / [`Message::mark_own`] when known.
    pub fn from_info(chat_id: impl Into<String>, info: &MessageInfo) -> Self {
        Message {
            id: info.id.clone(),
            chat_id: chat_id.into(),
            sender_mri: info.sender_mri.clone(),
            sender_name: info.sender.clone(),
            timestamp: info.timestamp.clone(),
            content: info.content.clone(),
            raw: info.raw.clone(),
            reactions: info.reactions.iter().map(Reaction::from).collect(),
            reply_to: info.reply_to.clone(),
            client_message_id: info.client_message_id.clone(),
            edited: false,
            is_own: false,
            // A message read off the wire has, by definition, reached the server.
            send_state: SendState::Sent,
        }
    }

    /// Attach (or replace) the owning chat id.
    pub fn with_chat_id(mut self, chat_id: impl Into<String>) -> Self {
        self.chat_id = chat_id.into();
        self
    }

    /// Set the owning chat id in place.
    pub fn set_chat_id(&mut self, chat_id: impl Into<String>) {
        self.chat_id = chat_id.into();
    }

    /// Set [`Message::is_own`] directly.
    pub fn with_own(mut self, is_own: bool) -> Self {
        self.is_own = is_own;
        self
    }

    /// Mark [`Message::is_own`] by comparing this message's sender against the
    /// signed-in user's `self_mri`.
    ///
    /// Matching is deliberately tolerant of the two shapes Teams uses: a full
    /// MRI (`8:orgid:<guid>`) matches case-insensitively, and a *bare* OID
    /// (`<guid>`, no `:`) matches when the sender MRI ends with it. Anything
    /// else (including an empty `self_mri`) leaves `is_own` unchanged.
    pub fn mark_own(&mut self, self_mri: &str) -> &mut Self {
        if !self.sender_mri.is_empty() && !self_mri.is_empty() {
            let mri = self.sender_mri.to_ascii_lowercase();
            let me = self_mri.to_ascii_lowercase();
            let matches = if me.contains(':') {
                mri == me
            } else {
                mri == me || mri.ends_with(&me)
            };
            if matches {
                self.is_own = true;
            }
        }
        self
    }

    /// Set [`Message::edited`].
    pub fn with_edited(mut self, edited: bool) -> Self {
        self.edited = edited;
        self
    }

    /// Mark the message as edited.
    pub fn mark_edited(&mut self) -> &mut Self {
        self.edited = true;
        self
    }

    /// Set [`Message::send_state`].
    pub fn with_send_state(mut self, state: SendState) -> Self {
        self.send_state = state;
        self
    }

    /// Set [`Message::send_state`] in place.
    pub fn set_send_state(&mut self, state: SendState) {
        self.send_state = state;
    }
}

impl From<&MessageInfo> for Message {
    /// Convert without a chat id (`chat_id` is left empty). Prefer
    /// [`Message::from_info`] so the owning thread is recorded.
    fn from(info: &MessageInfo) -> Self {
        Message::from_info(String::new(), info)
    }
}

/// Redacting-ish `Debug`: prints identity and plain text, but not `raw` HTML.
///
/// The raw body can be large and can carry mentions, links and embedded
/// payloads; logging it by default would be noisy and could leak more than a
/// developer expects. We show its length instead.
impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Message")
            .field("id", &self.id)
            .field("chat_id", &self.chat_id)
            .field("sender_mri", &self.sender_mri)
            .field("sender_name", &self.sender_name)
            .field("timestamp", &self.timestamp)
            .field("content", &self.content)
            .field("raw_len", &self.raw.len())
            .field("reactions", &self.reactions.len())
            .field("reply_to", &self.reply_to)
            .field("client_message_id", &self.client_message_id)
            .field("edited", &self.edited)
            .field("is_own", &self.is_own)
            .field("send_state", &self.send_state)
            .finish()
    }
}

// ----- search (B1) --------------------------------------------------------

/// One message-search result (backlog item **B1**).
///
/// The **local-first** search ([`crate::search::Search`]) builds these from the
/// cached [`Message`] rows in the [`Store`](crate::store::Store); the optional
/// server fallback maps a vendored `ost::api::SearchHitInfo` into the same shape
/// so the UI renders both identically. `snippet` is a short, plain-text excerpt
/// centred on the match (built by [`crate::search::snippet`]); `chat_name` is
/// resolved from the cached [`Chat`], falling back to the id when the chat is
/// not cached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// Owning conversation id — open it to jump to the message.
    pub chat_id: String,
    /// Display name of the conversation (the id when the chat is not cached).
    pub chat_name: String,
    /// Server message id (the jump target).
    pub message_id: String,
    /// Sender display name.
    pub sender: String,
    /// Plain-text excerpt centred on the match, ellipsised when truncated.
    pub snippet: String,
    /// ISO-8601 arrival timestamp.
    pub timestamp: String,
    /// Whether the message was sent by the signed-in user.
    pub is_own: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> ChatInfo {
        ChatInfo {
            id: "19:alice@unq.gbl.spaces".into(),
            name: "Alice".into(),
            is_group: false,
            last_message_time: Some("2026-01-02T03:04:05Z".into()),
            last_message_sender: Some("Alice".into()),
            last_message_preview: Some("hello world".into()),
        }
    }

    fn message_info() -> MessageInfo {
        MessageInfo {
            id: "1700000000000".into(),
            sender_mri: "8:orgid:aaaa".into(),
            sender: "Alice".into(),
            timestamp: "2026-01-02T03:04:05Z".into(),
            content: "hello".into(),
            raw: "<p>hello <b>world</b></p>".into(),
            reactions: vec![ReactionCount {
                emoji: "👍".into(),
                count: 2,
                reactors: vec![
                    WireReactor {
                        id: "8:orgid:bbbb".into(),
                        name: "Bob".into(),
                    },
                    WireReactor {
                        id: "8:orgid:cccc".into(),
                        name: String::new(),
                    },
                ],
            }],
            reply_to: Some("1699999999999".into()),
            client_message_id: Some("1234567890123456789".into()),
        }
    }

    #[test]
    fn chat_from_info_maps_fields_and_defaults_local_flags() {
        let chat = Chat::from_info(&info());
        assert_eq!(chat.id, "19:alice@unq.gbl.spaces");
        assert_eq!(chat.name, "Alice");
        assert!(!chat.is_group);
        assert_eq!(
            chat.last_message_time.as_deref(),
            Some("2026-01-02T03:04:05Z")
        );
        assert_eq!(chat.last_message_sender.as_deref(), Some("Alice"));
        assert_eq!(chat.last_message_preview.as_deref(), Some("hello world"));
        assert!(!chat.unread);
        assert!(!chat.pinned);
        // The `From` impl delegates to `from_info`.
        assert_eq!(Chat::from(&info()), chat);
    }

    #[test]
    fn message_from_info_attaches_chat_id_and_defaults_local_state() {
        let msg = Message::from_info("19:alice@unq.gbl.spaces", &message_info());
        assert_eq!(msg.chat_id, "19:alice@unq.gbl.spaces");
        assert_eq!(msg.id, "1700000000000");
        assert_eq!(msg.sender_mri, "8:orgid:aaaa");
        assert_eq!(msg.sender_name, "Alice");
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.raw, "<p>hello <b>world</b></p>");
        assert_eq!(msg.reply_to.as_deref(), Some("1699999999999"));
        assert_eq!(
            msg.client_message_id.as_deref(),
            Some("1234567890123456789")
        );
        assert!(!msg.edited);
        assert!(!msg.is_own);
    }

    #[test]
    fn reactions_convert_with_reactors() {
        let msg = Message::from_info("c", &message_info());
        assert_eq!(msg.reactions.len(), 1);
        let r = &msg.reactions[0];
        assert_eq!(r.emoji, "👍");
        assert_eq!(r.count, 2);
        assert_eq!(r.reactors.len(), 2);
        assert_eq!(r.reactors[0].id, "8:orgid:bbbb");
        assert_eq!(r.reactors[0].name, "Bob");
        assert_eq!(r.reactors[1].name, "");
    }

    #[test]
    fn from_message_info_without_chat_id_leaves_it_empty() {
        let msg = Message::from(&message_info());
        assert_eq!(msg.chat_id, "");
        // with_chat_id replaces it.
        assert_eq!(msg.clone().with_chat_id("c1").chat_id, "c1");
    }

    #[test]
    fn mark_own_matches_full_mri_and_bare_oid() {
        let mut msg = Message::from_info("c", &message_info());
        msg.mark_own("8:orgid:AAAA"); // case-insensitive full MRI
        assert!(msg.is_own);

        let mut other = Message::from_info("c", &message_info());
        other.mark_own("aaaa"); // bare OID suffix form
        assert!(other.is_own);

        let mut stranger = Message::from_info("c", &message_info());
        stranger.mark_own("8:orgid:zzzz");
        assert!(!stranger.is_own);

        let mut empty = Message::from_info("c", &message_info());
        empty.mark_own("");
        assert!(!empty.is_own);
    }

    #[test]
    fn mark_edited_and_with_edited_set_the_flag() {
        let mut msg = Message::from_info("c", &message_info());
        msg.mark_edited();
        assert!(msg.edited);
        assert!(
            Message::from_info("c", &message_info())
                .with_edited(true)
                .edited
        );
    }

    #[test]
    fn send_state_defaults_to_sent_and_round_trips() {
        // Wired-in messages are always `Sent`.
        let msg = Message::from_info("c", &message_info());
        assert_eq!(msg.send_state, SendState::Sent);

        // Explicit states and their database names.
        assert_eq!(
            Message::from_info("c", &message_info())
                .with_send_state(SendState::Pending)
                .send_state,
            SendState::Pending
        );
        assert_eq!(SendState::Pending.as_str(), "pending");
        assert_eq!(SendState::from_db("failed"), SendState::Failed);
        assert!(SendState::Pending.is_pending());
        // Unknown / blank values degrade to `Sent`.
        assert_eq!(SendState::from_db("bogus"), SendState::Sent);
        assert_eq!(SendState::from_db(""), SendState::Sent);

        // Serializable, and a missing field in old JSON defaults to `Sent`.
        let mut value = serde_json::to_value(msg).unwrap();
        value.as_object_mut().unwrap().remove("send_state");
        let back: Message = serde_json::from_value(value).unwrap();
        assert_eq!(back.send_state, SendState::Sent);
    }

    #[test]
    fn debug_omits_raw_html_but_keeps_identity() {
        let msg = Message::from_info("19:alice@unq.gbl.spaces", &message_info());
        let dbg = format!("{msg:?}");
        assert!(dbg.contains("1700000000000"));
        assert!(dbg.contains("Alice"));
        assert!(dbg.contains("hello"));
        // Raw HTML is reduced to its length, never printed.
        assert!(!dbg.contains("<p>"), "raw HTML leaked into Debug: {dbg}");
        assert!(dbg.contains("raw_len"));
    }

    #[test]
    fn model_types_round_trip_through_json() {
        // Reactions are persisted as JSON in the store, so they must serde
        // round-trip cleanly.
        let msg = Message::from_info("c", &message_info());
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: Message = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, msg);
    }

    // ----- M6: teams, channels, members -------------------------------------

    #[test]
    fn team_and_member_convert_from_protocol_types() {
        let team = Team::from_info(&TeamInfo {
            id: "group-1".into(),
            name: "Engineering".into(),
            channels: Vec::new(),
        });
        assert_eq!(team.id, "group-1");
        assert_eq!(team.name, "Engineering");

        let member = TeamMember::from_info(&TeamMemberInfo {
            id: "M1".into(),
            display_name: "Alice".into(),
            user_id: Some("oid-1".into()),
            email: Some("alice@example.com".into()),
            roles: vec!["owner".into()],
            is_owner: true,
        });
        assert_eq!(member.id, "M1");
        assert_eq!(member.user_id.as_deref(), Some("oid-1"));
        assert_eq!(member.display_name, "Alice");
        assert_eq!(member.email.as_deref(), Some("alice@example.com"));
        assert_eq!(member.roles, vec!["owner".to_string()]);
        assert!(member.is_owner());

        let plain = TeamMember::from_info(&TeamMemberInfo {
            id: "M2".into(),
            display_name: "Bob".into(),
            user_id: None,
            email: None,
            roles: Vec::new(),
            is_owner: false,
        });
        assert!(!plain.is_owner());
        assert!(plain.email.is_none());
    }

    // ----- B4: team administration -----------------------------------------

    #[test]
    fn public_team_converts_and_marks_membership_locally() {
        let info = PublicTeamInfo {
            id: "team-1".into(),
            name: "Engineering".into(),
            description: Some("Build things".into()),
            visibility: "public".into(),
        };
        let joined = PublicTeam::from_info(&info, true);
        assert_eq!(joined.id, "team-1");
        assert_eq!(joined.display_name, "Engineering");
        assert_eq!(joined.description.as_deref(), Some("Build things"));
        assert_eq!(joined.visibility, "public");
        assert!(joined.is_joined);
        // The wire carries no membership flag, so `From` defaults it false.
        let bare = PublicTeam::from(&info);
        assert!(!bare.is_joined);
        assert_eq!(bare.id, joined.id);
        // It round-trips through JSON (the shape a future cache would store).
        let back: PublicTeam =
            serde_json::from_str(&serde_json::to_string(&joined).unwrap()).unwrap();
        assert_eq!(back, joined);
    }

    #[test]
    fn team_settings_map_unknown_fields_to_none() {
        let settings = TeamSettings::from_wire(&TeamMemberSettings {
            allow_delete_channels: Some(true),
            allow_create_update_channels: None,
            primary_channel_id: Some("19:general@thread.tacv2".into()),
        });
        assert_eq!(settings.allow_delete_channels, Some(true));
        assert_eq!(settings.allow_create_update_channels, None);
        assert_eq!(
            settings.primary_channel_id.as_deref(),
            Some("19:general@thread.tacv2")
        );
        // A bare default is all-unknown, not all-false.
        let empty = TeamSettings::from_wire(&TeamMemberSettings::default());
        assert_eq!(empty.allow_delete_channels, None);
        assert_eq!(empty.allow_create_update_channels, None);
        assert!(empty.primary_channel_id.is_none());
    }

    // ----- B3: people -------------------------------------------------------

    #[test]
    fn person_converts_from_a_directory_member() {
        let person = Person::from_member(&TeamMemberInfo {
            id: "u1".into(),
            display_name: "Ava Lindqvist".into(),
            user_id: Some("u1".into()),
            email: Some("ava@contoso.com".into()),
            roles: Vec::new(),
            is_owner: false,
        });
        assert_eq!(person.id, "u1");
        assert_eq!(person.display_name, "Ava Lindqvist");
        assert_eq!(person.email.as_deref(), Some("ava@contoso.com"));
        // The projection does not carry a job title: it is never guessed.
        assert!(person.job_title.is_none());
        assert_eq!(person.label(), "Ava Lindqvist <ava@contoso.com>");
        // The `From` impl delegates to `from_member`.
        assert_eq!(
            Person::from(&TeamMemberInfo {
                id: "u1".into(),
                display_name: "Ava Lindqvist".into(),
                user_id: Some("u1".into()),
                email: Some("ava@contoso.com".into()),
                roles: Vec::new(),
                is_owner: false,
            }),
            person
        );
    }

    #[test]
    fn person_label_omits_a_missing_email_and_round_trips() {
        let person = Person::from_member(&TeamMemberInfo {
            id: "u2".into(),
            display_name: "Tom Becker".into(),
            user_id: None,
            email: None,
            roles: Vec::new(),
            is_owner: false,
        });
        assert_eq!(person.label(), "Tom Becker");
        let back: Person = serde_json::from_str(&serde_json::to_string(&person).unwrap()).unwrap();
        assert_eq!(back, person);
    }

    // ----- B2a: chat roster -------------------------------------------------

    #[test]
    fn chat_member_converts_from_a_roster_entry_and_labels() {
        let info = ChatMemberInfo {
            mri: "8:orgid:u1".into(),
            user_id: Some("u1".into()),
            display_name: "Ava Lindqvist".into(),
            email: Some("ava@contoso.com".into()),
            roles: vec!["admin".into()],
            is_owner: true,
        };
        let member = ChatMember::from_info(&info);
        assert_eq!(member.mri, "8:orgid:u1");
        assert_eq!(member.id.as_deref(), Some("u1"));
        assert_eq!(member.display_name, "Ava Lindqvist");
        assert_eq!(member.email.as_deref(), Some("ava@contoso.com"));
        assert!(member.is_owner);
        assert_eq!(member.label(), "Ava Lindqvist <ava@contoso.com>");
        // The `From` impl delegates to `from_info`.
        assert_eq!(ChatMember::from(&info), member);
        // A roster entry round-trips through serde (the store's JSON rule).
        let back: ChatMember =
            serde_json::from_str(&serde_json::to_string(&member).unwrap()).unwrap();
        assert_eq!(back, member);
    }

    #[test]
    fn chat_member_label_falls_back_to_email_then_mri() {
        // No name, an email → the email.
        let email_only = ChatMember::from_info(&ChatMemberInfo {
            mri: "8:orgid:u2".into(),
            user_id: Some("u2".into()),
            display_name: String::new(),
            email: Some("tom@contoso.com".into()),
            roles: Vec::new(),
            is_owner: false,
        });
        assert_eq!(email_only.label(), "tom@contoso.com");
        // Neither → the MRI, never an empty label.
        let bare = ChatMember::from_info(&ChatMemberInfo {
            mri: "8:orgid:u3".into(),
            user_id: Some("u3".into()),
            display_name: "  ".into(),
            email: None,
            roles: Vec::new(),
            is_owner: false,
        });
        assert_eq!(bare.label(), "8:orgid:u3");
    }

    #[test]
    fn channel_from_info_attaches_team_and_derives_is_channel() {
        let info = ChannelInfo {
            id: "19:general@thread.tacv2".into(),
            name: "General".into(),
            description: Some("Team-wide".into()),
            membership_type: Some("standard".into()),
            web_url: Some("https://teams.microsoft.com/l/channel/x".into()),
        };
        let channel = Channel::from_info("group-1", &info);
        assert_eq!(channel.id, "19:general@thread.tacv2");
        assert_eq!(channel.team_id, "group-1");
        assert_eq!(channel.name, "General");
        assert_eq!(channel.description.as_deref(), Some("Team-wide"));
        assert_eq!(channel.membership_type.as_deref(), Some("standard"));
        assert_eq!(
            channel.web_url.as_deref(),
            Some("https://teams.microsoft.com/l/channel/x")
        );
        assert!(channel.is_channel);
        // The tuple `From` impl delegates to `from_info`.
        assert_eq!(Channel::from(("group-1", &info)), channel);

        // A rare non-channel id still lists but is not classified as a channel
        // conversation.
        let odd = ChannelInfo {
            id: "19:legacy@thread.v2".into(),
            name: "Legacy".into(),
            description: None,
            membership_type: None,
            web_url: None,
        };
        assert!(!Channel::from_info("group-1", &odd).is_channel);
    }

    #[test]
    fn channel_classification_matches_teams_shapes() {
        assert!(is_channel_conversation_id("19:abc@thread.tacv2"));
        assert!(is_channel_conversation_id("19:abc@thread.skype"));
        assert!(is_channel_conversation_id("  19:abc@thread.tacv2  "));
        // Group chats, 1:1s and meetings are not channels.
        assert!(!is_channel_conversation_id("19:abc@thread.v2"));
        assert!(!is_channel_conversation_id("19:abc@unq.gbl.spaces"));
        assert!(!is_channel_conversation_id("19:team@thread.v2"));
        assert!(!is_channel_conversation_id(""));
        assert!(!is_channel_conversation_id("not-a-thread"));
    }

    // ----- M7: files --------------------------------------------------------

    fn shared_file() -> WireSharedFile {
        WireSharedFile {
            id: "item-1".into(),
            name: "Plan.docx".into(),
            size: 2048,
            mime: Some(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document".into(),
            ),
            web_url: Some("https://contoso.sharepoint.com/:w:/x/ABC".into()),
            download_url: Some("https://contoso.sharepoint.com/download".into()),
            drive_id: Some("drive-1".into()),
            created: Some("2026-01-01T00:00:00Z".into()),
            modified: Some("2026-01-02T00:00:00Z".into()),
            sender: Some("Alice".into()),
            is_folder: false,
            share_url: None,
            attachment_id: Some("550e8400-e29b-41d4-a716-446655440000".into()),
        }
    }

    #[test]
    fn file_entry_converts_from_shared_file() {
        let entry = FileEntry::from_shared(&shared_file());
        assert_eq!(entry.id, "item-1");
        assert_eq!(entry.drive_id.as_deref(), Some("drive-1"));
        assert_eq!(entry.name, "Plan.docx");
        assert_eq!(entry.size, 2048);
        assert!(entry.mime.as_deref().unwrap().contains("wordprocessingml"));
        assert_eq!(entry.sender.as_deref(), Some("Alice"));
        assert!(!entry.is_folder);
        assert!(entry.share_url.is_none());
        assert_eq!(
            entry.attachment_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        // The `From` impls delegate to `from_shared`.
        assert_eq!(FileEntry::from(&shared_file()), entry);
        assert_eq!(FileEntry::from(shared_file()), entry);
    }

    #[test]
    fn file_entry_keeps_unknown_drive_for_unresolved_refs() {
        let mut wire = shared_file();
        wire.drive_id = None;
        wire.size = 0;
        wire.mime = None;
        wire.download_url = None;
        let entry = FileEntry::from(wire);
        assert!(entry.drive_id.is_none());
        assert_eq!(entry.size, 0);
        assert!(entry.download_url.is_none());
    }

    #[test]
    fn file_version_and_shared_link_are_owned() {
        let version = FileVersion::from_wire(&WireFileVersion {
            id: "3.0".into(),
            size: 48211,
            modified: Some("2026-09-20T10:00:00Z".into()),
            modified_by: Some("Megan Harper".into()),
        });
        assert_eq!(version.id, "3.0");
        assert_eq!(version.size, 48211);
        assert_eq!(version.modified_by.as_deref(), Some("Megan Harper"));
        assert_eq!(
            FileVersion::from(&WireFileVersion {
                id: "1.0".into(),
                size: 0,
                modified: None,
                modified_by: None,
            })
            .size,
            0
        );

        let link = SharedLink {
            url: "https://contoso.sharepoint.com/:w:/x/ABC".into(),
            scope: Some("organization".into()),
        };
        let back: SharedLink =
            serde_json::from_str(&serde_json::to_string(&link).unwrap()).unwrap();
        assert_eq!(back, link);
    }

    // ----- M9: call log -----------------------------------------------------

    #[test]
    fn call_direction_and_outcome_round_trip_through_db_names() {
        assert_eq!(CallDirection::Outgoing.as_str(), "outgoing");
        assert_eq!(CallDirection::from_db("incoming"), CallDirection::Incoming);
        assert_eq!(CallDirection::from_db("bogus"), CallDirection::Outgoing);

        assert_eq!(CallOutcome::Completed.as_str(), "completed");
        assert_eq!(CallOutcome::from_db("missed"), CallOutcome::Missed);
        assert_eq!(CallOutcome::from_db("failed"), CallOutcome::Failed);
        assert_eq!(CallOutcome::from_db("declined"), CallOutcome::Declined);
        assert_eq!(CallOutcome::from_db("cancelled"), CallOutcome::Cancelled);
        // Unknown / in-progress both degrade to `Unknown`.
        assert_eq!(CallOutcome::from_db("unknown"), CallOutcome::Unknown);
        assert_eq!(CallOutcome::from_db("nonsense"), CallOutcome::Unknown);
        assert!(CallOutcome::Unknown.is_open());
        assert!(!CallOutcome::Completed.is_open());
    }

    #[test]
    fn call_log_entry_finish_derives_duration_and_outcome() {
        let entry = CallLogEntry::started("call-1", "thread-1", CallDirection::Incoming, 1_000)
            .with_participants(vec!["Alice".into(), "8:orgid:self".into()]);
        assert_eq!(entry.id, "call-1");
        assert_eq!(entry.thread_id, "thread-1");
        assert_eq!(entry.direction, CallDirection::Incoming);
        assert_eq!(entry.started_at, 1_000);
        assert!(entry.ended_at.is_none());
        assert!(entry.duration_secs.is_none());
        assert_eq!(entry.participants.len(), 2);
        assert_eq!(entry.outcome, CallOutcome::Unknown);

        let done = entry.clone().finished(1_042, CallOutcome::Completed);
        assert_eq!(done.ended_at, Some(1_042));
        assert_eq!(done.duration_secs, Some(42));
        assert_eq!(done.outcome, CallOutcome::Completed);

        // A clock that went backwards never yields a negative duration.
        let backwards = entry.finished(900, CallOutcome::Failed);
        assert_eq!(backwards.duration_secs, Some(0));

        // Serializable (persisted as JSON columns for participants).
        let back: CallLogEntry =
            serde_json::from_str(&serde_json::to_string(&done).unwrap()).unwrap();
        assert_eq!(back, done);
    }

    // ----- M12: calendar meetings -------------------------------------------

    fn meeting_info() -> WireMeetingInfo {
        WireMeetingInfo {
            id: "E1".into(),
            subject: "Standup".into(),
            start: Some("2026-09-24T09:00:00.0000000".into()),
            end: Some("2026-09-24T09:15:00.0000000".into()),
            join_url: Some(
                "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0".into(),
            ),
            organizer: Some("Doe, Jane".into()),
            organizer_email: Some("jane@contoso.com".into()),
            is_organizer: false,
            is_online: true,
        }
    }

    #[test]
    fn meeting_converts_from_protocol_info_and_defaults_uncarried_fields() {
        let meeting = Meeting::from_info(&meeting_info());
        assert_eq!(meeting.id, "E1");
        assert_eq!(meeting.subject, "Standup");
        assert_eq!(
            meeting.start.as_deref(),
            Some("2026-09-24T09:00:00.0000000")
        );
        assert_eq!(meeting.end.as_deref(), Some("2026-09-24T09:15:00.0000000"));
        assert_eq!(meeting.organizer.as_deref(), Some("Doe, Jane"));
        assert_eq!(meeting.organizer_email.as_deref(), Some("jane@contoso.com"));
        assert!(!meeting.is_organizer);
        assert!(meeting.is_online);
        assert!(meeting.join_url.is_some());
        // The `From` impl delegates to `from_info`.
        assert_eq!(Meeting::from(&meeting_info()), meeting);
        // Fields the vendored projection does not carry default honestly.
        assert!(!meeting.all_day);
        assert!(meeting.join_meeting_id.is_none());
        assert!(meeting.passcode.is_none());
        assert!(meeting.web_link.is_none());
        assert!(meeting.location.is_none());
    }

    #[test]
    fn meeting_is_joinable_when_the_link_names_a_thread() {
        assert!(Meeting::from_info(&meeting_info()).is_joinable());

        let mut bare = meeting_info();
        bare.join_url = Some("https://example.com/x".into());
        assert!(!Meeting::from_info(&bare).is_joinable());

        let mut none = meeting_info();
        none.join_url = None;
        assert!(!Meeting::from_info(&none).is_joinable());
    }

    #[test]
    fn meeting_debug_redacts_the_join_url() {
        let dbg = format!("{:?}", Meeting::from_info(&meeting_info()));
        assert!(dbg.contains("Standup"));
        assert!(dbg.contains("has_join_url"));
        assert!(
            !dbg.contains("meetup-join"),
            "the join URL must not be printed: {dbg}"
        );
    }

    #[test]
    fn meeting_round_trips_through_json() {
        let meeting = Meeting::from_info(&meeting_info());
        let back: Meeting =
            serde_json::from_str(&serde_json::to_string(&meeting).unwrap()).unwrap();
        assert_eq!(back, meeting);
    }

    // ----- M13: identity ----------------------------------------------------

    #[test]
    fn identity_from_whoami_derives_the_mri() {
        let info = UserInfo {
            display_name: "Alice Smith".into(),
            mail: Some("alice@contoso.com".into()),
            id: "  abc-123  ".into(),
        };
        let identity = Identity::from_whoami(&info, "contoso.onmicrosoft.com");
        assert_eq!(identity.mri, "8:orgid:abc-123");
        assert_eq!(identity.oid, "abc-123");
        assert_eq!(identity.display_name, "Alice Smith");
        assert_eq!(identity.tenant, "contoso.onmicrosoft.com");
        assert!(identity.has_mri());

        // An empty id yields an empty MRI, never a bogus `8:orgid:`.
        let blank = Identity::from_whoami(
            &UserInfo {
                display_name: "User".into(),
                mail: None,
                id: "   ".into(),
            },
            "t",
        );
        assert!(blank.mri.is_empty());
        assert!(!blank.has_mri());
    }

    #[test]
    fn identity_marks_matching_messages_own() {
        let identity = Identity::from_whoami(
            &UserInfo {
                display_name: "Alice".into(),
                mail: None,
                id: "aaaa".into(),
            },
            "t",
        );
        let mut mine = Message::from_info("c", &message_info());
        mine.sender_mri = "8:orgid:aaaa".into();
        mine.mark_own(&identity.mri);
        assert!(mine.is_own);

        // A stranger's message stays `false`.
        let mut theirs = Message::from_info("c", &message_info());
        theirs.sender_mri = "8:orgid:zzzz".into();
        theirs.mark_own(&identity.mri);
        assert!(!theirs.is_own);
    }

    // ----- B2b: folders & pins ----------------------------------------------

    fn wire_folder(
        id: &str,
        name: &str,
        folder_type: &str,
        items: &[&str],
    ) -> WireConversationFolder {
        WireConversationFolder {
            id: id.into(),
            name: name.into(),
            folder_type: folder_type.into(),
            item_ids: items.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn chat_folder_converts_and_classifies_system_types() {
        let folder = ChatFolder::from_wire(&wire_folder(
            "t~u~Favorites",
            "Favorites",
            "Favorites",
            &["19:a@thread.v2", "48:notes"],
        ));
        assert_eq!(folder.id, "t~u~Favorites");
        assert_eq!(folder.name, "Favorites");
        assert_eq!(folder.folder_type, "Favorites");
        assert_eq!(folder.item_ids.len(), 2);
        // Favorites is a move target, not a view.
        assert!(!folder.is_system);
        assert_eq!(
            ChatFolder::from(&wire_folder("f", "Work", "UserCreated", &[])),
            ChatFolder::from_wire(&wire_folder("f", "Work", "UserCreated", &[]))
        );

        // The server-generated views are system folders (case-insensitive).
        assert!(is_system_folder("RecentChats"));
        assert!(is_system_folder("quickviews"));
        assert!(is_system_folder(" MutedChats "));
        assert!(!is_system_folder("Favorites"));
        assert!(!is_system_folder("UserCreated"));
        assert!(!is_system_folder(""));
    }

    fn wire_pin(message_id: &str) -> PinnedRef {
        PinnedRef {
            message_id: message_id.into(),
            sender: Some("Ava".into()),
            preview: Some("hello".into()),
            time: Some("2026-01-02T03:04:05Z".into()),
            pinned_by: Some("Bo".into()),
            pinned_at: Some("2026-01-03T00:00:00Z".into()),
            graph_pin_id: None,
        }
    }

    #[test]
    fn pinned_message_converts_and_resolves_a_preview() {
        let pin = PinnedMessage::from_wire("c1", &wire_pin("m1"));
        assert_eq!(pin.chat_id, "c1");
        assert_eq!(pin.message_id, "m1");
        assert_eq!(pin.sender.as_deref(), Some("Ava"));
        assert_eq!(pin.preview_text(), "hello");
        assert!(pin.message.is_none());
        assert_eq!(
            PinnedMessage::from(("c1", &wire_pin("m1"))),
            PinnedMessage::from_wire("c1", &wire_pin("m1"))
        );

        // With no wire preview, the cached message's text is the fallback.
        let mut bare = wire_pin("m2");
        bare.preview = None;
        let with_message = PinnedMessage::from_wire("c1", &bare)
            .with_message(Message::from_info("c1", &message_info()));
        assert_eq!(with_message.preview_text(), "hello");
        assert!(with_message.message.is_some());
        assert_eq!(with_message.preview_text(), "hello");

        // JSON round-trip (the store persists the membership columns).
        let back: PinnedMessage =
            serde_json::from_str(&serde_json::to_string(&pin).unwrap()).unwrap();
        assert_eq!(back, pin);
    }
}
