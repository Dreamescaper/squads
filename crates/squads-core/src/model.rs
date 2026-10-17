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

use ost::api::{ChatInfo, MessageInfo, ReactionCount, Reactor as WireReactor};

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
}
