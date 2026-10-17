//! Realtime domain events (milestone **M4**).
//!
//! Trouter pushes chat-service event JSON into the process-global
//! [`ost::event_hub`](ost::event_hub); this module turns those JSON payloads into
//! a small, owned, UI-facing [`RealtimeEvent`] enum. Nothing here talks to the
//! network and nothing here can panic: [`decode`] / [`decode_all`] are pure and
//! total, so an unexpected wire shape degrades to [`RealtimeEvent::Unknown`]
//! carrying the raw JSON rather than being dropped.
//!
//! ## Wire shapes
//!
//! The payloads the hub receives are exactly the strings
//! [`ost::trouter::data_frame_event`](ost::trouter::data_frame_event) produces:
//! either a single event object (`{"resourceType": …, "resource": { … }}`) or an
//! `eventMessages` array of such objects. Keys are matched
//! **case-insensitively** and missing fields are tolerated. The native message
//! fields we read are `id`, `content`, `imdisplayname`,
//! `originalarrivaltime`/`composetime`, `from`, `messagetype`, `clientmessageid`
//! and `properties`.
//!
//! ## Batches
//!
//! [`decode_all`] returns one event per `eventMessages` entry. [`decode`] is the
//! single-event convenience: for a batch it returns the **first** entry (server
//! order). The M4 collector always uses [`decode_all`] so no batched event is
//! dropped.
//!
//! ## What never reaches us
//!
//! Call events (`NGCallManagerWin`, `SkypeSpacesWeb`) are filtered out by
//! `data_frame_event` before they reach the hub, so call signalling is not
//! representable here — see ADR-013 and B-008.

use std::fmt;

use serde_json::{Map, Value};

use crate::model::{Message, Reaction};

/// The maximum number of fields we scan for a case-insensitive key is bounded by
/// the object size; these helpers never recurse without a bound.
type MapOf = Map<String, Value>;

/// A decoded realtime event from the Teams chat service.
///
/// Every variant is data-complete for M4's needs; shapes we do not model (or
/// cannot classify) fall back to [`RealtimeEvent::Unknown`], which keeps the raw
/// JSON so nothing is silently lost.
#[derive(Clone, PartialEq, Eq)]
pub enum RealtimeEvent {
    /// A new message arrived in `chat_id`.
    NewMessage {
        /// Owning chat / thread id.
        chat_id: String,
        /// The decoded message (with the owning `chat_id` attached).
        message: Message,
    },
    /// An existing message's body changed.
    MessageEdited {
        /// Owning chat / thread id.
        chat_id: String,
        /// Server id of the message that changed.
        message_id: String,
        /// The new plain-text body (HTML stripped).
        content: String,
        /// An optional secondary id some payloads carry for the edit
        /// (`editedMessageId` / `editMessageId` / `originalMessageId`).
        edited_id: Option<String>,
    },
    /// A message was deleted.
    MessageDeleted {
        /// Owning chat / thread id.
        chat_id: String,
        /// Server id of the deleted message.
        message_id: String,
    },
    /// Someone is typing in `chat_id`.
    Typing {
        /// Owning chat / thread id.
        chat_id: String,
        /// Sender MRI (`8:orgid:<guid>`), `""` when the wire omitted it.
        sender_mri: String,
        /// Sender display name, `""` when unknown.
        sender_name: String,
    },
    /// A peer read position (consumption horizon) moved.
    ReadState {
        /// Owning chat / thread id.
        chat_id: String,
        /// The viewer whose read position moved.
        viewer_mri: String,
        /// The raw horizon value `<t1>;<t2>;<message_id>`.
        horizon: String,
        /// The read-frontier message id parsed from the horizon, when present.
        message_id: Option<String>,
    },
    /// A conversation / thread's own metadata changed.
    ThreadUpdate {
        /// Owning chat / thread id.
        chat_id: String,
        /// What changed (best effort; see [`ThreadChange`]).
        change: ThreadChange,
    },
    /// The client may have missed pushes and should re-fetch (message loss, bus
    /// lag, or an explicit resync).
    Resync {
        /// Short human-readable reason.
        reason: String,
    },
    /// A payload we could not classify. Kept verbatim so no data is lost.
    Unknown {
        /// The `resourceType` we saw, when any.
        resource_type: Option<String>,
        /// The raw event JSON.
        raw: String,
    },
}

/// Best-effort description of a conversation / thread change.
#[derive(Clone, PartialEq, Eq)]
pub enum ThreadChange {
    /// The conversation was renamed (topic / name).
    Topic {
        /// The new topic, when the payload named one.
        name: Option<String>,
    },
    /// The roster membership changed.
    Members,
    /// Some other conversation property changed.
    Properties,
}

impl fmt::Debug for ThreadChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThreadChange::Topic { name } => f.debug_struct("Topic").field("name", name).finish(),
            ThreadChange::Members => f.write_str("Members"),
            ThreadChange::Properties => f.write_str("Properties"),
        }
    }
}

/// Decode a single realtime event.
///
/// For an `eventMessages` batch this returns the **first** entry; use
/// [`decode_all`] to consume every entry. Never panics: anything unparseable or
/// unclassifiable becomes [`RealtimeEvent::Unknown`].
pub fn decode(json: &str) -> RealtimeEvent {
    decode_all(json)
        .into_iter()
        .next()
        .unwrap_or_else(|| unknown(None, json))
}

/// Decode every event in a payload.
///
/// A single event object yields one element; an `eventMessages` array yields one
/// element per entry. The result is never empty (a malformed/`null` payload
/// yields a single [`RealtimeEvent::Unknown`]).
pub fn decode_all(json: &str) -> Vec<RealtimeEvent> {
    match serde_json::from_str::<Value>(json) {
        Ok(value) => decode_value_all(&value, json),
        Err(_) => vec![unknown(None, json)],
    }
}

fn decode_value_all(value: &Value, raw: &str) -> Vec<RealtimeEvent> {
    let Some(obj) = value.as_object() else {
        return vec![unknown(None, raw)];
    };

    // A batch of events.
    if let Some(entries) = field(obj, "eventMessages").and_then(Value::as_array) {
        if entries.is_empty() {
            return vec![unknown(Some("EventMessages".into()), raw)];
        }
        return entries
            .iter()
            .map(|entry| decode_one(entry, &entry.to_string()))
            .collect();
    }

    // Defensive wrapper unwrap: the hub already unwraps `data`/`body`/`cp`/`gp`,
    // but a nested event here must not be lost.
    if field(obj, "resourceType").is_none() {
        for key in ["data", "body", "payload"] {
            if let Some(inner) = field(obj, key).filter(|v| is_event_like(v)) {
                let inner_raw = inner.to_string();
                return decode_value_all(inner, &inner_raw);
            }
        }
    }

    vec![decode_one(value, raw)]
}

/// Whether `value` looks like an event object (carries `resourceType` or
/// `eventMessages`).
fn is_event_like(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|o| field(o, "resourceType").is_some() || field(o, "eventMessages").is_some())
}

fn decode_one(value: &Value, raw: &str) -> RealtimeEvent {
    let Some(obj) = value.as_object() else {
        return unknown(None, raw);
    };
    let resource_type = str_field(obj, "resourceType");
    let resource = field(obj, "resource").and_then(Value::as_object);
    // Fields live on `resource` when present, otherwise directly on the event.
    let body = resource.unwrap_or(obj);
    let kind = resource_type.as_deref().unwrap_or("").to_ascii_lowercase();

    if kind.contains("loss") || kind.contains("resync") {
        return RealtimeEvent::Resync {
            reason: resource_type.unwrap_or_else(|| "message loss".to_string()),
        };
    }
    if kind.is_empty() && (raw.contains("message_loss") || raw.contains("MessageLoss")) {
        return RealtimeEvent::Resync {
            reason: "message loss".to_string(),
        };
    }

    match kind.as_str() {
        "newmessage" | "messagecreated" => {
            let id = str_field(body, "id").unwrap_or_default();
            if id.is_empty() {
                return unknown(resource_type, raw);
            }
            let chat_id = chat_id_of(resource, obj).unwrap_or_default();
            let message = parse_message(&chat_id, body);
            RealtimeEvent::NewMessage { chat_id, message }
        }
        "messageupdate" | "messageedited" | "editmessage" | "messageedit" => {
            let message_id = str_field(body, "id")
                .or_else(|| str_field(body, "messageId"))
                .unwrap_or_default();
            if message_id.is_empty() {
                return unknown(resource_type, raw);
            }
            let chat_id = chat_id_of(resource, obj).unwrap_or_default();
            let new_raw = str_field(body, "content").unwrap_or_default();
            let content = strip_html(&new_raw).trim().to_string();
            let edited_id = str_field(body, "editedMessageId")
                .or_else(|| str_field(body, "editMessageId"))
                .or_else(|| str_field(body, "originalMessageId"));
            RealtimeEvent::MessageEdited {
                chat_id,
                message_id,
                content,
                edited_id,
            }
        }
        "messagedelete" | "messagedeleted" | "deletemessage" | "messagedeletion" => {
            let message_id = str_field(body, "id")
                .or_else(|| str_field(body, "messageId"))
                .unwrap_or_default();
            if message_id.is_empty() {
                return unknown(resource_type, raw);
            }
            let chat_id = chat_id_of(resource, obj).unwrap_or_default();
            RealtimeEvent::MessageDeleted {
                chat_id,
                message_id,
            }
        }
        "typing" => {
            let chat_id = chat_id_of(resource, obj).unwrap_or_default();
            let sender_mri = sender_mri_of(body);
            let sender_name = str_field(body, "imdisplayname")
                .or_else(|| str_field(body, "displayName"))
                .or_else(|| str_field(body, "sender"))
                .unwrap_or_default();
            RealtimeEvent::Typing {
                chat_id,
                sender_mri,
                sender_name,
            }
        }
        "messageseen" | "consumptionhorizon" | "readreceipt" | "messageread" => {
            let chat_id = chat_id_of(resource, obj).unwrap_or_default();
            let viewer_mri = sender_mri_of(body);
            let horizon = str_field(body, "consumptionhorizon")
                .or_else(|| str_field(body, "horizon"))
                .or_else(|| str_field(body, "value"))
                .unwrap_or_default();
            let message_id = ost::api::receipt_message_id(&horizon);
            RealtimeEvent::ReadState {
                chat_id,
                viewer_mri,
                horizon,
                message_id,
            }
        }
        "conversationupdate"
        | "threadupdate"
        | "chatupdate"
        | "channelupdate"
        | "conversationpropertyupdate" => {
            let chat_id = chat_id_of(resource, obj).unwrap_or_default();
            RealtimeEvent::ThreadUpdate {
                chat_id,
                change: thread_change(body),
            }
        }
        _ => unknown(resource_type, raw),
    }
}

/// Build a domain [`Message`] from a native message-shaped object.
///
/// `chat_id` is the owning thread. `edited` and `is_own` default to `false`
/// (the wire cannot tell us; see [`crate::model`]).
fn parse_message(chat_id: &str, o: &MapOf) -> Message {
    let id = str_field(o, "id").unwrap_or_default();
    let raw = str_field(o, "content").unwrap_or_default();
    let content = strip_html(&raw).trim().to_string();
    let sender_name = str_field(o, "imdisplayname")
        .or_else(|| str_field(o, "displayName"))
        .or_else(|| str_field(o, "sender"))
        .unwrap_or_else(|| "?".to_string());
    let sender_mri = sender_mri_of(o);
    let timestamp = str_field(o, "originalarrivaltime")
        .or_else(|| str_field(o, "composetime"))
        .unwrap_or_default();
    let props = field(o, "properties").and_then(Value::as_object);
    let client_message_id = str_field(o, "clientmessageid")
        .or_else(|| props.and_then(|p| str_field(p, "clientmessageid")));
    // Reply parent: the public vendored splitter handles the `<quote guid>` form.
    let reply_to = ost::api::split_reply_quote(&raw)
        .0
        .filter(|p| !p.trim().is_empty() && p != &id);
    let reactions: Vec<Reaction> = ost::api::reaction_counts_from_values(
        field(o, "reactions"),
        props.and_then(|p| field(p, "emotions")),
    )
    .iter()
    .map(Reaction::from)
    .collect();

    Message {
        id,
        chat_id: chat_id.to_string(),
        sender_mri,
        sender_name,
        timestamp,
        content,
        raw,
        reactions,
        reply_to,
        client_message_id,
        edited: false,
        is_own: false,
    }
}

/// Chat id from the resource, then the event object (`conversationId` and
/// friends, case-insensitive).
fn chat_id_of(resource: Option<&MapOf>, event: &MapOf) -> Option<String> {
    const KEYS: &[&str] = &[
        "conversationId",
        "chatId",
        "threadId",
        "targetConversationId",
    ];
    for source in [resource, Some(event)].into_iter().flatten() {
        for key in KEYS {
            if let Some(value) = str_field(source, key) {
                return Some(value);
            }
        }
    }
    None
}

/// Sender MRI: a `from` user link / object, then `mri` / `userId` / `owner`.
fn sender_mri_of(o: &MapOf) -> String {
    let from = mri_from_from(field(o, "from"));
    if !from.is_empty() {
        return from;
    }
    str_field(o, "mri")
        .or_else(|| str_field(o, "userId"))
        .or_else(|| str_field(o, "owner"))
        .unwrap_or_default()
}

/// MRI from the two `from` shapes: a user-link string
/// (`…/contacts/8:orgid:<guid>`) or an object carrying `id`/`mri`.
fn mri_from_from(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => percent_decode(s.rsplit('/').next().unwrap_or("").trim()),
        Some(Value::Object(o)) => str_field(o, "id")
            .or_else(|| str_field(o, "mri"))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Classify what a conversation update changed (best effort).
fn thread_change(body: &MapOf) -> ThreadChange {
    let name = str_field(body, "topic")
        .or_else(|| str_field(body, "name"))
        .or_else(|| {
            field(body, "threadProperties")
                .and_then(Value::as_object)
                .and_then(|tp| str_field(tp, "topic").or_else(|| str_field(tp, "name")))
        });
    if let Some(name) = name {
        return ThreadChange::Topic { name: Some(name) };
    }
    if field(body, "members").is_some() {
        return ThreadChange::Members;
    }
    ThreadChange::Properties
}

fn unknown(resource_type: Option<String>, raw: &str) -> RealtimeEvent {
    RealtimeEvent::Unknown {
        resource_type,
        raw: raw.to_string(),
    }
}

/// Case-insensitive object field lookup.
fn field<'a>(o: &'a MapOf, key: &str) -> Option<&'a Value> {
    o.get(key).or_else(|| {
        o.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    })
}

/// A non-empty trimmed string, or a stringified number.
fn str_field(o: &MapOf, key: &str) -> Option<String> {
    match field(o, key)? {
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Strip HTML for display, with the vendored block-boundary spacing rule
/// (`</p><p>` becomes a space, inline tags vanish) and common-entity decoding.
///
/// This mirrors the parsing in `ost::api` (whose helpers are private) closely
/// enough for M4's decode; M5 owns the full HTML/RichText model.
fn strip_html(html: &str) -> String {
    const BLOCK_TAGS: &[&str] = &[
        "p",
        "div",
        "br",
        "section",
        "article",
        "header",
        "footer",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "ul",
        "ol",
        "li",
        "dl",
        "dt",
        "dd",
        "table",
        "tr",
        "td",
        "th",
        "blockquote",
        "pre",
        "hr",
    ];
    let mut out = String::with_capacity(html.len());
    let mut tag = String::new();
    let mut in_tag = false;
    let mut pending_space = false;
    for ch in html.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let name = tag
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .next()
                    .unwrap_or("");
                if BLOCK_TAGS.iter().any(|t| t.eq_ignore_ascii_case(name)) {
                    pending_space = true;
                }
                tag.clear();
            } else {
                tag.push(ch);
            }
        } else if ch == '<' {
            in_tag = true;
        } else {
            if pending_space {
                pending_space = false;
                if !out.is_empty() && !out.ends_with(char::is_whitespace) && !ch.is_whitespace() {
                    out.push(' ');
                }
            }
            out.push(ch);
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Minimal `%XX` percent-decode (MRIs are colon-separated GUIDs; only `%3A` and
/// friends ever appear).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Redacting-ish `Debug`: message content follows [`Message`]'s own (already
/// raw-free) `Debug`; `Unknown` prints the raw length, never the JSON body.
impl fmt::Debug for RealtimeEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RealtimeEvent::NewMessage { chat_id, message } => f
                .debug_struct("NewMessage")
                .field("chat_id", chat_id)
                .field("message", message)
                .finish(),
            RealtimeEvent::MessageEdited {
                chat_id,
                message_id,
                content,
                edited_id,
            } => f
                .debug_struct("MessageEdited")
                .field("chat_id", chat_id)
                .field("message_id", message_id)
                .field("content", content)
                .field("edited_id", edited_id)
                .finish(),
            RealtimeEvent::MessageDeleted {
                chat_id,
                message_id,
            } => f
                .debug_struct("MessageDeleted")
                .field("chat_id", chat_id)
                .field("message_id", message_id)
                .finish(),
            RealtimeEvent::Typing {
                chat_id,
                sender_mri,
                sender_name,
            } => f
                .debug_struct("Typing")
                .field("chat_id", chat_id)
                .field("sender_mri", sender_mri)
                .field("sender_name", sender_name)
                .finish(),
            RealtimeEvent::ReadState {
                chat_id,
                viewer_mri,
                horizon,
                message_id,
            } => f
                .debug_struct("ReadState")
                .field("chat_id", chat_id)
                .field("viewer_mri", viewer_mri)
                .field("horizon", horizon)
                .field("message_id", message_id)
                .finish(),
            RealtimeEvent::ThreadUpdate { chat_id, change } => f
                .debug_struct("ThreadUpdate")
                .field("chat_id", chat_id)
                .field("change", change)
                .finish(),
            RealtimeEvent::Resync { reason } => {
                f.debug_struct("Resync").field("reason", reason).finish()
            }
            RealtimeEvent::Unknown { resource_type, raw } => f
                .debug_struct("Unknown")
                .field("resource_type", resource_type)
                .field("raw_len", &raw.len())
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap a native message body in a single `NewMessage` event.
    fn new_message_event(resource: Value) -> String {
        serde_json::json!({ "resourceType": "NewMessage", "resource": resource }).to_string()
    }

    fn full_message_resource() -> Value {
        serde_json::json!({
            "id": "1700000000000",
            "type": "Message",
            "conversationId": "19:alice@unq.gbl.spaces",
            "from": "https://teams.microsoft.com/api/v1/users/ME/contacts/8:orgid:aaaa",
            "imdisplayname": "Alice",
            "content": "<p>hello <b>world</b></p>",
            "messagetype": "RichText/Html",
            "originalarrivaltime": "2026-01-02T03:04:05Z",
            "composetime": "2026-01-02T03:04:04Z",
            "clientmessageid": "1234567890123456789"
        })
    }

    #[test]
    fn new_message_decodes_every_field() {
        let event = decode(&new_message_event(full_message_resource()));
        let RealtimeEvent::NewMessage { chat_id, message } = event else {
            panic!("expected NewMessage, got {event:?}");
        };
        assert_eq!(chat_id, "19:alice@unq.gbl.spaces");
        assert_eq!(message.id, "1700000000000");
        assert_eq!(message.chat_id, "19:alice@unq.gbl.spaces");
        assert_eq!(message.sender_mri, "8:orgid:aaaa");
        assert_eq!(message.sender_name, "Alice");
        assert_eq!(message.timestamp, "2026-01-02T03:04:05Z");
        assert_eq!(message.content, "hello world");
        assert_eq!(message.raw, "<p>hello <b>world</b></p>");
        assert_eq!(
            message.client_message_id.as_deref(),
            Some("1234567890123456789")
        );
        assert!(!message.edited);
        assert!(!message.is_own);
        assert!(message.reactions.is_empty());
    }

    #[test]
    fn new_message_missing_optional_fields_uses_defaults() {
        let event = decode(&new_message_event(serde_json::json!({ "id": "m1" })));
        let RealtimeEvent::NewMessage { chat_id, message } = event else {
            panic!("expected NewMessage");
        };
        assert!(chat_id.is_empty());
        assert_eq!(message.id, "m1");
        assert_eq!(message.sender_name, "?");
        assert_eq!(message.sender_mri, "");
        assert_eq!(message.timestamp, "");
        assert_eq!(message.content, "");
        assert_eq!(message.raw, "");
    }

    #[test]
    fn new_message_under_event_messages_array_for_a_batch() {
        let payload = serde_json::json!({
            "eventMessages": [
                { "resourceType": "NewMessage",
                  "resource": { "id": "a", "conversationId": "c1", "content": "one" } },
                { "resourceType": "NewMessage",
                  "resource": { "id": "b", "conversationId": "c2", "content": "two" } }
            ]
        })
        .to_string();

        let all = decode_all(&payload);
        assert_eq!(all.len(), 2);
        assert!(matches!(
            &all[0],
            RealtimeEvent::NewMessage { message, .. } if message.id == "a"
        ));
        assert!(matches!(
            &all[1],
            RealtimeEvent::NewMessage { message, .. } if message.id == "b"
        ));
        // `decode` returns the first (documented) entry.
        assert!(matches!(
            decode(&payload),
            RealtimeEvent::NewMessage { message, .. } if message.id == "a"
        ));
    }

    #[test]
    fn keys_are_matched_case_insensitively() {
        let payload = serde_json::json!({
            "ResourceType": "NewMessage",
            "Resource": {
                "Id": "m1",
                "ConversationId": "c1",
                "From": "https://x/contacts/8:orgid:bbbb",
                "IMDisplayName": "Bob",
                "Content": "hi",
                "OriginalArrivalTime": "2026-01-01T00:00:00Z"
            }
        })
        .to_string();
        let RealtimeEvent::NewMessage { chat_id, message } = decode(&payload) else {
            panic!("expected NewMessage");
        };
        assert_eq!(chat_id, "c1");
        assert_eq!(message.id, "m1");
        assert_eq!(message.sender_mri, "8:orgid:bbbb");
        assert_eq!(message.sender_name, "Bob");
        assert_eq!(message.content, "hi");
        assert_eq!(message.timestamp, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn message_edit_decodes_content_and_edited_id() {
        let payload = serde_json::json!({
            "resourceType": "MessageUpdate",
            "resource": {
                "id": "m1",
                "conversationId": "c1",
                "content": "<p>edited <i>body</i></p>",
                "editedMessageId": "m1"
            }
        })
        .to_string();
        let RealtimeEvent::MessageEdited {
            chat_id,
            message_id,
            content,
            edited_id,
        } = decode(&payload)
        else {
            panic!("expected MessageEdited");
        };
        assert_eq!(chat_id, "c1");
        assert_eq!(message_id, "m1");
        assert_eq!(content, "edited body");
        assert_eq!(edited_id.as_deref(), Some("m1"));
    }

    #[test]
    fn message_delete_decodes() {
        let payload = serde_json::json!({
            "resourceType": "MessageDelete",
            "resource": { "id": "m9", "conversationId": "c1" }
        })
        .to_string();
        assert_eq!(
            decode(&payload),
            RealtimeEvent::MessageDeleted {
                chat_id: "c1".into(),
                message_id: "m9".into(),
            }
        );
    }

    #[test]
    fn typing_decodes_sender() {
        let payload = serde_json::json!({
            "resourceType": "Typing",
            "resource": {
                "conversationId": "c1",
                "from": "https://x/contacts/8:orgid:cccc",
                "imdisplayname": "Carol"
            }
        })
        .to_string();
        assert_eq!(
            decode(&payload),
            RealtimeEvent::Typing {
                chat_id: "c1".into(),
                sender_mri: "8:orgid:cccc".into(),
                sender_name: "Carol".into(),
            }
        );
    }

    #[test]
    fn read_state_decodes_horizon_and_frontier() {
        let payload = serde_json::json!({
            "resourceType": "MessageSeen",
            "resource": {
                "conversationId": "c1",
                "mri": "8:orgid:dddd",
                "consumptionhorizon": "1700000000000;1700000000000;m7"
            }
        })
        .to_string();
        assert_eq!(
            decode(&payload),
            RealtimeEvent::ReadState {
                chat_id: "c1".into(),
                viewer_mri: "8:orgid:dddd".into(),
                horizon: "1700000000000;1700000000000;m7".into(),
                message_id: Some("m7".into()),
            }
        );
    }

    #[test]
    fn thread_update_topic_members_and_properties() {
        let topic = serde_json::json!({
            "resourceType": "ConversationUpdate",
            "resource": { "conversationId": "c1", "threadProperties": { "topic": "New name" } }
        })
        .to_string();
        assert_eq!(
            decode(&topic),
            RealtimeEvent::ThreadUpdate {
                chat_id: "c1".into(),
                change: ThreadChange::Topic {
                    name: Some("New name".into())
                },
            }
        );

        let members = serde_json::json!({
            "resourceType": "ThreadUpdate",
            "resource": { "conversationId": "c1", "members": ["8:orgid:aaaa"] }
        })
        .to_string();
        assert_eq!(
            decode(&members),
            RealtimeEvent::ThreadUpdate {
                chat_id: "c1".into(),
                change: ThreadChange::Members,
            }
        );

        let properties = serde_json::json!({
            "resourceType": "ChatUpdate",
            "resource": { "conversationId": "c1", "someFlag": true }
        })
        .to_string();
        assert_eq!(
            decode(&properties),
            RealtimeEvent::ThreadUpdate {
                chat_id: "c1".into(),
                change: ThreadChange::Properties,
            }
        );
    }

    #[test]
    fn message_loss_becomes_resync() {
        let payload = serde_json::json!({
            "resourceType": "MessageLoss",
            "resource": {}
        })
        .to_string();
        assert_eq!(
            decode(&payload),
            RealtimeEvent::Resync {
                reason: "MessageLoss".into(),
            }
        );

        // A bare frame-ish string still surfaces as a resync.
        assert!(matches!(
            decode(r#"{"note":"trouter.message_loss"}"#),
            RealtimeEvent::Resync { .. }
        ));
    }

    #[test]
    fn unknown_resource_type_keeps_raw() {
        let payload = serde_json::json!({
            "resourceType": "SomethingWeird",
            "resource": { "x": 1 }
        })
        .to_string();
        let RealtimeEvent::Unknown { resource_type, raw } = decode(&payload) else {
            panic!("expected Unknown");
        };
        assert_eq!(resource_type.as_deref(), Some("SomethingWeird"));
        assert_eq!(raw, payload, "the raw JSON must be preserved verbatim");
    }

    #[test]
    fn invalid_json_is_unknown_not_a_panic() {
        let payload = "definitely not json";
        let RealtimeEvent::Unknown { resource_type, raw } = decode(payload) else {
            panic!("expected Unknown");
        };
        assert_eq!(resource_type, None);
        assert_eq!(raw, payload);
    }

    #[test]
    fn new_message_without_id_is_unknown() {
        let payload = serde_json::json!({
            "resourceType": "NewMessage",
            "resource": { "conversationId": "c1" }
        })
        .to_string();
        assert!(matches!(decode(&payload), RealtimeEvent::Unknown { .. }));
    }

    #[test]
    fn wrapper_body_object_is_unwrapped() {
        let payload = serde_json::json!({
            "id": 9,
            "body": {
                "resourceType": "NewMessage",
                "resource": { "id": "m1", "conversationId": "c1", "content": "hi" }
            }
        })
        .to_string();
        assert!(matches!(
            decode(&payload),
            RealtimeEvent::NewMessage { message, .. } if message.id == "m1"
        ));
    }

    #[test]
    fn properties_client_message_id_and_reactions_are_read() {
        let payload = serde_json::json!({
            "resourceType": "NewMessage",
            "resource": {
                "id": "m1",
                "conversationId": "c1",
                "content": "with a reaction",
                "properties": {
                    "clientmessageid": "42",
                    "emotions": [
                        { "key": "like", "users": [{ "mri": "8:orgid:bbbb", "displayName": "Bob" }] }
                    ]
                }
            }
        })
        .to_string();
        let RealtimeEvent::NewMessage { message, .. } = decode(&payload) else {
            panic!("expected NewMessage");
        };
        assert_eq!(message.client_message_id.as_deref(), Some("42"));
        assert_eq!(message.reactions.len(), 1);
        assert_eq!(message.reactions[0].count, 1);
    }

    #[test]
    fn number_client_message_id_stringifies() {
        let payload = serde_json::json!({
            "resourceType": "NewMessage",
            "resource": { "id": "m1", "conversationId": "c1", "clientmessageid": 9988 }
        })
        .to_string();
        let RealtimeEvent::NewMessage { message, .. } = decode(&payload) else {
            panic!("expected NewMessage");
        };
        assert_eq!(message.client_message_id.as_deref(), Some("9988"));
    }

    #[test]
    fn decode_never_panics_on_odd_shapes() {
        for payload in [
            "null",
            "[]",
            "42",
            r#""a string""#,
            r#"{"eventMessages":[]}"#,
            r#"{"resourceType":null}"#,
            r#"{"resource":{}}"#,
        ] {
            // Must always produce at least one event and must not panic.
            let all = decode_all(payload);
            assert!(!all.is_empty(), "empty decode for {payload}");
            let _ = decode(payload);
        }
    }

    #[test]
    fn percent_encoded_mri_is_decoded() {
        let payload = serde_json::json!({
            "resourceType": "NewMessage",
            "resource": {
                "id": "m1",
                "conversationId": "c1",
                "from": "https://x/contacts/8%3Aorgid%3Aaaaa"
            }
        })
        .to_string();
        let RealtimeEvent::NewMessage { message, .. } = decode(&payload) else {
            panic!("expected NewMessage");
        };
        assert_eq!(message.sender_mri, "8:orgid:aaaa");
    }

    #[test]
    fn debug_does_not_leak_raw_json() {
        let payload = serde_json::json!({
            "resourceType": "SomethingWeird",
            "resource": { "secret": "sensitive" }
        })
        .to_string();
        let dbg = format!("{:?}", decode(&payload));
        assert!(dbg.contains("raw_len"));
        assert!(!dbg.contains("sensitive"), "raw leaked into Debug: {dbg}");
    }
}
