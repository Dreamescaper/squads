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
//! [`FileVersion::from_wire`].
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
    ChannelInfo, ChatInfo, FileVersion as WireFileVersion, MessageInfo, ReactionCount,
    Reactor as WireReactor, SharedFile as WireSharedFile, TeamInfo, TeamMemberInfo,
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
/// target), not the user id; `roles` carries Graph role names and contains
/// `"owner"` for team owners. The owner flag is derivable from `roles` and is
/// deliberately not duplicated here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    /// Membership id (not the user id).
    pub id: String,
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
}
