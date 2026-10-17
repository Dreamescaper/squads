//! Notification routing and unread/activity derivation (milestone **M8**).
//!
//! This module turns the M4 realtime stream into the two things a Teams client
//! actually surfaces: **desktop notifications** and **unread badges / an
//! activity feed**.
//!
//! ```text
//!   RealtimeEvent ──route(..)──▶ Option<Notification> ──▶ Notifier
//!   Store (messages + read_state horizon) ──▶ UnreadSummary / ActivityItem
//! ```
//!
//! ## Routing is pure
//!
//! [`route`] is a pure function of one [`RealtimeEvent`] and a
//! [`NotificationContext`] (our MRI/name, the muted chats, and the ids of our own
//! messages). It implements the rules the UI expects:
//!
//! * **own messages never notify** — neither a locally-marked `is_own` message
//!   nor one whose sender MRI is ours;
//! * **only a new message notifies** — edits, deletes, typing, read-state,
//!   thread updates, resyncs and unknowns return `None` (a reaction-only change
//!   is an edit on the wire, so it is silent too);
//! * **per-chat mute is honoured** (mute beats a mention);
//! * a message that mentions us is [`NotificationKind::Mention`]; a message that
//!   replies to one of our own is [`NotificationKind::Reply`]; anything else is
//!   [`NotificationKind::NewMessage`].
//!
//! Mentions are found with [`crate::text::mentions`] (`<at …>` tags and `@name`),
//! matching either our MRI or our display name.
//!
//! ## Delivery
//!
//! A [`Notifier`] is the delivery seam. [`LogNotifier`] (the default) writes a
//! `tracing` line and [`RecordingNotifier`] captures notifications in memory for
//! tests. A `notify-rust`-backed [`DesktopNotifier`] lives behind the
//! **default-off** `desktop-notifications` cargo feature so the headless/CI
//! build needs no DBus or notification daemon (ADR-022, ADR-008 precedent).
//!
//! ## Quiet hours
//!
//! **Quiet hours are not implemented.** Suppressing notifications during a
//! window needs a clock and per-user settings the M8 brief did not settle, and
//! any clock in the router would make it non-deterministic; it is recorded as a
//! follow-up in `docs/BLOCKERS.md` (B-010).

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};

use crate::error::Result;
use crate::model::Message;
use crate::realtime::RealtimeEvent;
use crate::store::Store;
use crate::text::mentions;

/// Longest notification body we keep (a one-line summary); longer text is
/// truncated on a character boundary with an ellipsis.
pub const BODY_MAX: usize = 200;

/// How many chats [`Notifications::unread_summaries`] / [`Notifications::activity`]
/// scan. Teams rarely has more concurrent chats with history than this.
pub const CHAT_SCAN_LIMIT: usize = 500;

/// What a notification is about.
///
/// [`NotificationKind::NewMessage`], [`NotificationKind::Mention`] and
/// [`NotificationKind::Reply`] are produced by [`route`]. `Reaction` and
/// `MissedCall` are **placeholders**: the M4 bus carries no reaction or call
/// event yet (call frames are filtered upstream, B-008; reactions arrive as
/// message edits, which are silent), so nothing produces them today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    /// A new message in a chat we are not muted in.
    NewMessage,
    /// A new message that mentions us.
    Mention,
    /// A new message that replies to one of our messages.
    Reply,
    /// Someone reacted to one of our messages (placeholder — see the enum docs).
    Reaction,
    /// A missed call / an incoming call (placeholder — see the enum docs).
    MissedCall,
}

impl NotificationKind {
    /// A short, stable label (for logs and tests).
    pub fn as_str(self) -> &'static str {
        match self {
            NotificationKind::NewMessage => "new_message",
            NotificationKind::Mention => "mention",
            NotificationKind::Reply => "reply",
            NotificationKind::Reaction => "reaction",
            NotificationKind::MissedCall => "missed_call",
        }
    }
}

/// One notification to deliver.
///
/// `title` is the sender's display name and `body` a truncated one-line
/// summary. The custom [`fmt::Debug`] keeps the sender but prints the body as a
/// length, so message content is not dumped into logs.
#[derive(Clone, PartialEq, Eq)]
pub struct Notification {
    /// What kind of notification this is.
    pub kind: NotificationKind,
    /// The chat the event belongs to.
    pub chat_id: String,
    /// The notification title (sender display name).
    pub title: String,
    /// The notification body (truncated message text).
    pub body: String,
    /// Whether the message mentions the signed-in user.
    pub mention: bool,
}

impl fmt::Debug for Notification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notification")
            .field("kind", &self.kind)
            .field("chat_id", &self.chat_id)
            .field("title", &self.title)
            .field("body_len", &self.body.len())
            .field("mention", &self.mention)
            .finish()
    }
}

/// Delivers one [`Notification`].
///
/// Object-safe and `Send + Sync` so a session can hold an `Arc<dyn Notifier>`
/// and swap the implementation (log, record, desktop, test double) at runtime.
pub trait Notifier: Send + Sync {
    /// Deliver `n`. Must not block indefinitely and must not panic.
    fn notify(&self, n: &Notification);
}

/// The default notifier: writes a `tracing` line, never touches the OS.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogNotifier;

impl Notifier for LogNotifier {
    fn notify(&self, n: &Notification) {
        tracing::info!(
            kind = n.kind.as_str(),
            chat_id = %n.chat_id,
            mention = n.mention,
            "notification"
        );
    }
}

/// A notifier that records every notification in memory (tests, diagnostics).
#[derive(Debug, Default)]
pub struct RecordingNotifier {
    seen: Mutex<Vec<Notification>>,
}

impl RecordingNotifier {
    /// Create an empty recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of everything delivered so far.
    pub fn notifications(&self) -> Vec<Notification> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// How many notifications were delivered.
    pub fn len(&self) -> usize {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether nothing was delivered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Notifier for RecordingNotifier {
    fn notify(&self, n: &Notification) {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(n.clone());
    }
}

/// A `notify-rust`-backed desktop notifier (see the module docs).
///
/// Only compiled when the **default-off** `desktop-notifications` feature is
/// enabled; the default build has no dependency on `notify-rust`, DBus or a
/// notification daemon (ADR-022).
#[cfg(feature = "desktop-notifications")]
#[derive(Debug, Default, Clone, Copy)]
pub struct DesktopNotifier;

#[cfg(feature = "desktop-notifications")]
impl DesktopNotifier {
    /// Create the desktop notifier.
    pub fn new() -> Self {
        Self
    }
}

#[cfg(feature = "desktop-notifications")]
impl Notifier for DesktopNotifier {
    fn notify(&self, n: &Notification) {
        if let Err(error) = notify_rust::Notification::new()
            .summary(&n.title)
            .body(&n.body)
            .show()
        {
            tracing::warn!(%error, "desktop notification failed");
        }
    }
}

/// Everything the router needs to know about the current user and the chats.
///
/// Built by the caller (the UI/session knows our MRI and name; the store knows
/// the muted chats and our own messages) and kept **pure** so [`route`] is
/// trivially testable.
///
/// The two switches mirror the M11 [`Settings`](crate::settings::Settings)
/// notification options: `notifications_enabled` turns delivery off entirely,
/// and `notify_only_mentions` suppresses ordinary messages so only mentions and
/// replies-to-own get through. Both default to the permissive values
/// (enabled, all messages) so a context built without them behaves exactly as
/// before.
#[derive(Debug, Clone)]
pub struct NotificationContext {
    self_mri: Option<String>,
    self_name: Option<String>,
    muted_chats: HashSet<String>,
    own_message_ids: HashSet<String>,
    notifications_enabled: bool,
    notify_only_mentions: bool,
}

impl Default for NotificationContext {
    fn default() -> Self {
        Self {
            self_mri: None,
            self_name: None,
            muted_chats: HashSet::new(),
            own_message_ids: HashSet::new(),
            notifications_enabled: true,
            notify_only_mentions: false,
        }
    }
}

impl NotificationContext {
    /// An empty context (nothing muted, nobody known as "us", notifications on).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set our MRI and display name (for mention detection).
    pub fn with_self(mut self, mri: impl Into<String>, name: impl Into<String>) -> Self {
        self.self_mri = Some(mri.into());
        self.self_name = Some(name.into());
        self
    }

    /// Set our MRI only.
    pub fn with_self_mri(mut self, mri: impl Into<String>) -> Self {
        self.self_mri = Some(mri.into());
        self
    }

    /// Set our display name only.
    pub fn with_self_name(mut self, name: impl Into<String>) -> Self {
        self.self_name = Some(name.into());
        self
    }

    /// Turn notification delivery on or off (default: on).
    pub fn with_notifications_enabled(mut self, enabled: bool) -> Self {
        self.notifications_enabled = enabled;
        self
    }

    /// Suppress ordinary messages, keeping only mentions/replies (default: off).
    pub fn with_notify_only_mentions(mut self, only_mentions: bool) -> Self {
        self.notify_only_mentions = only_mentions;
        self
    }

    /// Mark one chat muted.
    pub fn mute(mut self, chat_id: impl Into<String>) -> Self {
        self.muted_chats.insert(chat_id.into());
        self
    }

    /// Mark many chats muted.
    pub fn mute_all(mut self, chat_ids: impl IntoIterator<Item = String>) -> Self {
        self.muted_chats.extend(chat_ids);
        self
    }

    /// Record one of our own message ids (for reply detection).
    pub fn own_message(mut self, id: impl Into<String>) -> Self {
        self.own_message_ids.insert(id.into());
        self
    }

    /// Record many of our own message ids.
    pub fn own_messages(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.own_message_ids.extend(ids);
        self
    }

    /// Record one of our own message ids in place.
    pub fn consider_own(&mut self, id: impl Into<String>) {
        self.own_message_ids.insert(id.into());
    }

    /// Whether `chat_id` is muted.
    pub fn is_muted(&self, chat_id: &str) -> bool {
        self.muted_chats.contains(chat_id)
    }

    /// Whether notifications are enabled at all.
    pub fn notifications_enabled(&self) -> bool {
        self.notifications_enabled
    }

    /// Whether only mentions/replies should notify.
    pub fn notify_only_mentions(&self) -> bool {
        self.notify_only_mentions
    }

    /// Our MRI, when known.
    pub fn self_mri(&self) -> Option<&str> {
        self.self_mri.as_deref()
    }

    /// Our display name, when known.
    pub fn self_name(&self) -> Option<&str> {
        self.self_name.as_deref()
    }
}

/// Route one realtime event to a notification (see the module docs).
///
/// Returns `None` for anything but a new message from someone else in a
/// non-muted chat, when notifications are enabled. With
/// [`NotificationContext::with_notify_only_mentions`] set, an ordinary message
/// (not a mention and not a reply to one of ours) is also dropped.
pub fn route(event: &RealtimeEvent, ctx: &NotificationContext) -> Option<Notification> {
    if !ctx.notifications_enabled() {
        return None;
    }
    let RealtimeEvent::NewMessage { chat_id, message } = event else {
        return None;
    };
    if ctx.is_muted(chat_id) {
        return None;
    }
    if is_own_message(message, ctx) {
        return None;
    }
    if message.id.is_empty() {
        return None;
    }
    let mention = message_mentions(message, ctx);
    let reply = !mention && is_reply_to_own(message, ctx);
    if ctx.notify_only_mentions() && !mention && !reply {
        return None;
    }
    let kind = if mention {
        NotificationKind::Mention
    } else if reply {
        NotificationKind::Reply
    } else {
        NotificationKind::NewMessage
    };
    Some(Notification {
        kind,
        chat_id: chat_id.clone(),
        title: title_for(message),
        body: truncate(&message.content, BODY_MAX),
        mention,
    })
}

/// Whether `message` was sent by us (`is_own`, or our MRI as the sender).
fn is_own_message(message: &Message, ctx: &NotificationContext) -> bool {
    if message.is_own {
        return true;
    }
    match ctx.self_mri() {
        Some(me) if !me.is_empty() => mri_matches(&message.sender_mri, me),
        _ => false,
    }
}

/// Whether `message` is a reply to one of our own messages.
fn is_reply_to_own(message: &Message, ctx: &NotificationContext) -> bool {
    message
        .reply_to
        .as_ref()
        .is_some_and(|parent| ctx.own_message_ids.contains(parent))
}

/// Whether the message body mentions us (by MRI or by display name).
fn message_mentions(message: &Message, ctx: &NotificationContext) -> bool {
    let found = mentions(&message.raw);
    if found.is_empty() {
        return false;
    }
    let me = ctx.self_mri();
    let name = ctx.self_name();
    found.iter().any(|mention| {
        if let (Some(mri), Some(me)) = (mention.mri.as_deref(), me)
            && !me.is_empty()
            && mri_matches(mri, me)
        {
            return true;
        }
        match name {
            Some(name) => !name.is_empty() && mention.name.eq_ignore_ascii_case(name),
            None => false,
        }
    })
}

/// Tolerant MRI match: a full MRI compares case-insensitively; a bare OID
/// matches a full MRI that ends with it (the same rule as [`Message::mark_own`]).
fn mri_matches(candidate: &str, me: &str) -> bool {
    if candidate.is_empty() {
        return false;
    }
    let candidate = candidate.to_ascii_lowercase();
    let me = me.to_ascii_lowercase();
    if me.contains(':') {
        candidate == me
    } else {
        candidate == me || candidate.ends_with(&me)
    }
}

/// The notification title: the sender's display name, or a generic fallback.
fn title_for(message: &Message) -> String {
    let name = message.sender_name.trim();
    if name.is_empty() || name == "?" {
        "New message".to_string()
    } else {
        name.to_string()
    }
}

/// Truncate `text` to at most `max` characters, appending `…` when cut.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// Per-chat unread derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadSummary {
    /// The chat.
    pub chat_id: String,
    /// Number of messages newer than the stored read horizon.
    pub unread: usize,
    /// The newest such message (for a preview line), if any.
    pub newest: Option<Message>,
}

/// One item of the cross-chat activity feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityItem {
    /// The chat the message belongs to.
    pub chat_id: String,
    /// The (unread) message.
    pub message: Message,
    /// Whether the message mentions us.
    pub mention: bool,
}

/// The notification/activity service the session hands the UI.
///
/// Holds the shared [`Store`] and the configured [`Notifier`]; it derives the
/// unread/activity views and delivers routed notifications.
pub struct Notifications {
    profile: String,
    store: Arc<Store>,
    notifier: Arc<dyn Notifier>,
}

impl Notifications {
    /// Build the service over a shared store and a notifier.
    pub fn new(profile: impl Into<String>, store: Arc<Store>, notifier: Arc<dyn Notifier>) -> Self {
        Self {
            profile: profile.into(),
            store,
            notifier,
        }
    }

    /// The account profile this service derives state for.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The shared local store.
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// The configured notifier.
    pub fn notifier(&self) -> &Arc<dyn Notifier> {
        &self.notifier
    }

    /// Replace the notifier.
    pub fn set_notifier(&mut self, notifier: Arc<dyn Notifier>) {
        self.notifier = notifier;
    }

    /// Route `event` and, when it yields a notification, deliver it. Returns the
    /// notification that was delivered (the pure [`route`] is the tested unit).
    pub fn notify_event(
        &self,
        event: &RealtimeEvent,
        ctx: &NotificationContext,
    ) -> Option<Notification> {
        let notification = route(event, ctx)?;
        self.notifier.notify(&notification);
        Some(notification)
    }

    /// Per-chat unread counts, in the chat list's own order (newest activity
    /// first). Chats with zero unread are included.
    pub fn unread_summaries(&self) -> Result<Vec<UnreadSummary>> {
        let chats = self.store.list_chats(&self.profile, CHAT_SCAN_LIMIT)?;
        let mut out = Vec::with_capacity(chats.len());
        for chat in chats {
            let unread = self.store.unread_count(&self.profile, &chat.id)?;
            let newest = self
                .store
                .unread_messages(&self.profile, &chat.id, 1)?
                .into_iter()
                .next();
            out.push(UnreadSummary {
                chat_id: chat.id,
                unread,
                newest,
            });
        }
        Ok(out)
    }

    /// The total unread count across every cached chat.
    pub fn unread_total(&self) -> Result<usize> {
        Ok(self.unread_summaries()?.into_iter().map(|s| s.unread).sum())
    }

    /// A cross-chat activity feed of unread messages, newest first, at most
    /// `limit` items, with each item flagged when it mentions us.
    pub fn activity(&self, ctx: &NotificationContext, limit: usize) -> Result<Vec<ActivityItem>> {
        let chats = self.store.list_chats(&self.profile, CHAT_SCAN_LIMIT)?;
        let mut items = Vec::new();
        for chat in chats {
            for message in self.store.unread_messages(&self.profile, &chat.id, limit)? {
                let mention = message_mentions(&message, ctx);
                items.push(ActivityItem {
                    chat_id: chat.id.clone(),
                    message,
                    mention,
                });
            }
        }
        items.sort_by(|a, b| {
            b.message
                .timestamp
                .cmp(&a.message.timestamp)
                .then_with(|| b.message.id.cmp(&a.message.id))
        });
        items.truncate(limit);
        Ok(items)
    }
}

impl fmt::Debug for Notifications {
    /// Redacting: identity and store only, never notification contents.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notifications")
            .field("profile", &self.profile)
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SendState;

    fn message(
        id: &str,
        chat_id: &str,
        sender_mri: &str,
        sender_name: &str,
        raw: &str,
        reply_to: Option<&str>,
    ) -> Message {
        Message {
            id: id.into(),
            chat_id: chat_id.into(),
            sender_mri: sender_mri.into(),
            sender_name: sender_name.into(),
            timestamp: format!("2026-01-01T00:00:{:0>2}Z", id.len()),
            content: crate::text::strip_html(raw).trim().to_string(),
            raw: raw.into(),
            reactions: Vec::new(),
            reply_to: reply_to.map(str::to_string),
            client_message_id: None,
            edited: false,
            is_own: false,
            send_state: SendState::Sent,
        }
    }

    fn new_message(chat_id: &str, message: Message) -> RealtimeEvent {
        RealtimeEvent::NewMessage {
            chat_id: chat_id.into(),
            message,
        }
    }

    const SELF: &str = "8:orgid:self";

    // ----- routing rules --------------------------------------------------

    #[test]
    fn own_message_never_notifies() {
        let ctx = NotificationContext::new().with_self(SELF, "Me");
        let from_us = message("m1", "c1", SELF, "Me", "<p>hi</p>", None);
        assert!(route(&new_message("c1", from_us), &ctx).is_none());

        // `is_own` (set by an optimistic send) is enough even without an MRI.
        let mut optimistic = message("m2", "c1", "", "Me", "<p>hi</p>", None);
        optimistic.is_own = true;
        assert!(route(&new_message("c1", optimistic), &ctx).is_none());

        // A bare-OID self matches a full MRI.
        let ctx = NotificationContext::new().with_self_mri("self");
        let from_us = message("m3", "c1", SELF, "Me", "<p>hi</p>", None);
        assert!(route(&new_message("c1", from_us), &ctx).is_none());
    }

    #[test]
    fn muted_chat_never_notifies_even_on_a_mention() {
        let ctx = NotificationContext::new().with_self(SELF, "Me").mute("c1");
        let mention = message(
            "m1",
            "c1",
            "8:orgid:other",
            "Alice",
            r#"<p>hey <at mri="8:orgid:self">Me</at></p>"#,
            None,
        );
        assert!(route(&new_message("c1", mention), &ctx).is_none());
        // A different chat still notifies.
        let other = message("m2", "c2", "8:orgid:other", "Alice", "<p>hi</p>", None);
        assert!(route(&new_message("c2", other), &ctx).is_some());
    }

    #[test]
    fn mention_by_mri_flags_a_mention_notification() {
        let ctx = NotificationContext::new().with_self(SELF, "Me");
        let msg = message(
            "m1",
            "c1",
            "8:orgid:other",
            "Alice",
            r#"<p>please review <at id="0" mri="8:orgid:self">Me</at></p>"#,
            None,
        );
        let n = route(&new_message("c1", msg), &ctx).expect("notification");
        assert_eq!(n.kind, NotificationKind::Mention);
        assert!(n.mention);
        assert_eq!(n.title, "Alice");
        assert_eq!(n.chat_id, "c1");
    }

    #[test]
    fn mention_by_display_name_flags_a_mention() {
        let ctx = NotificationContext::new().with_self(SELF, "Alice Smith");
        let msg = message(
            "m1",
            "c1",
            "8:orgid:other",
            "Bob",
            "<p>cc @Alice Smith</p>",
            None,
        );
        // `@Alice` is the `@name` token; the trailing " Smith" is plain text, so
        // the token is "Alice" — which does not equal "Alice Smith".
        let n = route(&new_message("c1", msg), &ctx).expect("notification");
        assert_eq!(n.kind, NotificationKind::NewMessage);
        assert!(!n.mention);

        // A single-word name is matched.
        let ctx = NotificationContext::new().with_self(SELF, "Bob");
        let msg = message("m2", "c1", "8:orgid:other", "Eve", "<p>hi @Bob</p>", None);
        let n = route(&new_message("c1", msg), &ctx).expect("notification");
        assert_eq!(n.kind, NotificationKind::Mention);
        assert!(n.mention);
    }

    #[test]
    fn reply_to_own_message_is_a_reply_notification() {
        let ctx = NotificationContext::new()
            .with_self(SELF, "Me")
            .own_message("parent-1");
        let msg = message(
            "m1",
            "c1",
            "8:orgid:other",
            "Alice",
            "<p>sure</p>",
            Some("parent-1"),
        );
        let n = route(&new_message("c1", msg), &ctx).expect("notification");
        assert_eq!(n.kind, NotificationKind::Reply);
        assert!(!n.mention);

        // A reply to someone else's message is an ordinary notification.
        let ctx = NotificationContext::new().with_self(SELF, "Me");
        let msg = message(
            "m2",
            "c1",
            "8:orgid:other",
            "Alice",
            "<p>ok</p>",
            Some("someone-else"),
        );
        let n = route(&new_message("c1", msg), &ctx).expect("notification");
        assert_eq!(n.kind, NotificationKind::NewMessage);
    }

    #[test]
    fn ordinary_message_is_a_new_message_notification() {
        let ctx = NotificationContext::new().with_self(SELF, "Me");
        let msg = message(
            "m1",
            "c1",
            "8:orgid:other",
            "Alice",
            "<p>standup at 5</p>",
            None,
        );
        let n = route(&new_message("c1", msg), &ctx).expect("notification");
        assert_eq!(n.kind, NotificationKind::NewMessage);
        assert!(!n.mention);
        assert_eq!(n.body, "standup at 5");
    }

    #[test]
    fn disabled_notifications_never_route() {
        let ctx = NotificationContext::new()
            .with_self(SELF, "Me")
            .with_notifications_enabled(false);
        let msg = message("m1", "c1", "8:orgid:other", "Alice", "<p>hi</p>", None);
        assert!(route(&new_message("c1", msg), &ctx).is_none());

        // A mention is dropped too when notifications are off.
        let mention = message(
            "m2",
            "c1",
            "8:orgid:other",
            "Alice",
            r#"<p>hey <at mri="8:orgid:self">Me</at></p>"#,
            None,
        );
        assert!(route(&new_message("c1", mention), &ctx).is_none());

        // The switch is readable back.
        assert!(!ctx.notifications_enabled());
    }

    #[test]
    fn notify_only_mentions_drops_ordinary_messages() {
        let ctx = NotificationContext::new()
            .with_self(SELF, "Me")
            .with_notify_only_mentions(true)
            .own_message("parent-1");

        // Ordinary message: dropped.
        let ordinary = message("m1", "c1", "8:orgid:other", "Alice", "<p>hi</p>", None);
        assert!(route(&new_message("c1", ordinary), &ctx).is_none());

        // A mention still notifies.
        let mention = message(
            "m2",
            "c1",
            "8:orgid:other",
            "Alice",
            r#"<p>hey <at mri="8:orgid:self">Me</at></p>"#,
            None,
        );
        assert_eq!(
            route(&new_message("c1", mention), &ctx).unwrap().kind,
            NotificationKind::Mention
        );

        // A reply to one of our messages still notifies.
        let reply = message(
            "m3",
            "c1",
            "8:orgid:other",
            "Alice",
            "<p>sure</p>",
            Some("parent-1"),
        );
        assert_eq!(
            route(&new_message("c1", reply), &ctx).unwrap().kind,
            NotificationKind::Reply
        );

        assert!(ctx.notify_only_mentions());
    }

    #[test]
    fn non_new_message_events_never_notify() {
        let ctx = NotificationContext::new().with_self(SELF, "Me");
        for event in [
            RealtimeEvent::MessageEdited {
                chat_id: "c1".into(),
                message_id: "m1".into(),
                content: "edited".into(),
                edited_id: None,
            },
            RealtimeEvent::MessageDeleted {
                chat_id: "c1".into(),
                message_id: "m1".into(),
            },
            RealtimeEvent::Typing {
                chat_id: "c1".into(),
                sender_mri: "8:orgid:other".into(),
                sender_name: "Alice".into(),
            },
            RealtimeEvent::ReadState {
                chat_id: "c1".into(),
                viewer_mri: "8:orgid:other".into(),
                horizon: "1;1;m1".into(),
                message_id: Some("m1".into()),
            },
            RealtimeEvent::Resync {
                reason: "loss".into(),
            },
            RealtimeEvent::Unknown {
                resource_type: None,
                raw: "{}".into(),
            },
        ] {
            assert!(route(&event, &ctx).is_none(), "event = {event:?}");
        }
    }

    #[test]
    fn body_is_truncated_and_debug_hides_it() {
        let long = format!("<p>{}</p>", "x".repeat(BODY_MAX + 50));
        let ctx = NotificationContext::new().with_self(SELF, "Me");
        let msg = message("m1", "c1", "8:orgid:other", "Alice", &long, None);
        let n = route(&new_message("c1", msg), &ctx).unwrap();
        assert_eq!(
            n.body.chars().count(),
            BODY_MAX + 1,
            "truncated with ellipsis"
        );
        assert!(n.body.ends_with('…'));

        let dbg = format!("{n:?}");
        assert!(dbg.contains("body_len"));
        assert!(!dbg.contains("xxxx"), "body leaked into Debug: {dbg}");
    }

    // ----- delivery -------------------------------------------------------

    #[test]
    fn recording_notifier_captures_routed_notifications() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let recorder = Arc::new(RecordingNotifier::new());
        let service = Notifications::new("default", store, recorder.clone());
        let ctx = NotificationContext::new().with_self(SELF, "Me");

        let event = new_message(
            "c1",
            message("m1", "c1", "8:orgid:other", "Alice", "<p>hi</p>", None),
        );
        let delivered = service.notify_event(&event, &ctx).expect("delivered");
        assert_eq!(delivered.kind, NotificationKind::NewMessage);
        assert_eq!(recorder.len(), 1);
        assert_eq!(recorder.notifications()[0].chat_id, "c1");

        // A non-notifying event delivers nothing.
        let edited = RealtimeEvent::MessageEdited {
            chat_id: "c1".into(),
            message_id: "m1".into(),
            content: "x".into(),
            edited_id: None,
        };
        assert!(service.notify_event(&edited, &ctx).is_none());
        assert_eq!(recorder.len(), 1);

        // The log notifier is a harmless no-op.
        LogNotifier.notify(&delivered);
    }

    // ----- unread derivation ----------------------------------------------

    fn seed(store: &Store, chat_id: &str, ids: &[&str]) {
        let messages: Vec<Message> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let mut m = message(id, chat_id, "8:orgid:other", "Alice", "<p>hi</p>", None);
                m.timestamp = format!("2026-01-01T00:00:{i:0>2}Z");
                m
            })
            .collect();
        store.upsert_messages("default", &messages).unwrap();
        store
            .upsert_chats(
                "default",
                &[crate::model::Chat {
                    id: chat_id.into(),
                    name: "Chat".into(),
                    is_group: false,
                    last_message_time: Some("2026-01-01T00:01:00Z".into()),
                    last_message_sender: Some("Alice".into()),
                    last_message_preview: Some("hi".into()),
                    unread: false,
                    pinned: false,
                }],
            )
            .unwrap();
    }

    #[test]
    fn unread_count_follows_the_read_state_horizon() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        seed(&store, "c1", &["m1", "m2", "m3", "m4"]);

        // No horizon yet: everything is unread.
        assert_eq!(store.unread_count("default", "c1").unwrap(), 4);

        // Read up to m2: m3 and m4 remain.
        store.set_read_state("default", "c1", "m2").unwrap();
        assert_eq!(store.unread_count("default", "c1").unwrap(), 2);
        let unread = store.unread_messages("default", "c1", 10).unwrap();
        assert_eq!(
            unread.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["m4", "m3"],
            "newest first"
        );

        // Read up to the newest: nothing unread.
        store.set_read_state("default", "c1", "m4").unwrap();
        assert_eq!(store.unread_count("default", "c1").unwrap(), 0);
        assert!(
            store
                .unread_messages("default", "c1", 10)
                .unwrap()
                .is_empty()
        );

        // A horizon pointing at a message that is no longer cached cannot be
        // resolved: we treat the whole chat as unread rather than silently
        // dropping the badge (documented, not a panic).
        store.set_read_state("default", "c1", "gone").unwrap();
        assert_eq!(store.unread_count("default", "c1").unwrap(), 4);
    }

    #[test]
    fn unread_summaries_and_activity_derive_from_the_store() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        seed(&store, "c1", &["m1", "m2", "m3"]);
        seed(&store, "c2", &["n1", "n2"]);
        store.set_read_state("default", "c1", "m1").unwrap();

        let service = Notifications::new("default", Arc::clone(&store), Arc::new(LogNotifier));
        let summaries = service.unread_summaries().unwrap();
        let c1 = summaries.iter().find(|s| s.chat_id == "c1").unwrap();
        assert_eq!(c1.unread, 2);
        assert_eq!(c1.newest.as_ref().unwrap().id, "m3");
        let c2 = summaries.iter().find(|s| s.chat_id == "c2").unwrap();
        assert_eq!(c2.unread, 2, "no horizon means everything is unread");
        assert_eq!(service.unread_total().unwrap(), 4);

        let ctx = NotificationContext::new().with_self("8:orgid:self", "Me");
        let activity = service.activity(&ctx, 10).unwrap();
        assert_eq!(activity.len(), 4);
        assert!(!activity[0].mention);

        // The feed is newest-first; a limit is honoured.
        let limited = service.activity(&ctx, 1).unwrap();
        assert_eq!(limited.len(), 1);
    }

    #[test]
    fn notifications_is_send_and_sync_and_debug_is_redacting() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Notifications>();
        let service = Notifications::new(
            "default",
            Arc::new(Store::open_in_memory().unwrap()),
            Arc::new(LogNotifier),
        );
        let dbg = format!("{service:?}");
        assert!(dbg.contains("Notifications"));
        assert!(dbg.contains("default"));
    }
}
