//! Incoming calls (milestone **M9**).
//!
//! Teams pushes an incoming call as a Trouter frame carrying a `callInvitation`
//! (an SDP offer, the participant roster and the acceptance/media-answer/end
//! links). The vendored [`ost::calling::parse_call_notification`] parses that
//! frame; this module turns it into an owned [`IncomingCall`] the UI and
//! [`CallService`](crate::calls::CallService) can use, and is the one place that
//! documents the **B-008** limitation.
//!
//! ## B-008: incoming calls cannot arrive through the M4 bus
//!
//! `squads-core`'s M4 realtime feed wraps the vendored Trouter loop, but the
//! vendored `trouter::data_frame_event` **deliberately filters call frames out**
//! (`NGCallManagerWin` / `SkypeSpacesWeb` → `None`) before they reach
//! `ost::event_hub`. So no [`RealtimeEvent`](crate::realtime::RealtimeEvent)
//! variant can represent ringing, and this parser has no producer through the
//! bus. Incoming calls therefore take the **vendored** path today (auto-answer
//! unless `TEAMS_MANUAL_CALLS` is set), and this module is exercised directly:
//! [`CallService::on_notification`](crate::calls::CallService::on_notification)
//! is the seam a future producer calls with the raw frame JSON.
//!
//! **Recommended fix** (a `squads-protocol` change, tracked as B-008): stop
//! filtering call frames in `data_frame_event`, add a
//! `RealtimeEvent::IncomingCall { raw }` (or a dedicated call hub), and have the
//! M4 collector hand the raw JSON to `on_notification`. That also needs a
//! `TEAMS_MANUAL_CALLS`-style opt-out so the M4 feed and M9 handling do not fight
//! over the same frames.
//!
//! [`RealtimeEvent`]: crate::realtime::RealtimeEvent

use ost::calling::{CallNotification, Participant, parse_call_notification};

/// An incoming call invitation, owned and ready for the UI.
#[derive(Debug, Clone)]
pub struct IncomingCall {
    /// The wire call id (`debugContent.callId`), when present.
    pub call_id: Option<String>,
    /// The caller's MRI, when present.
    pub from_mri: Option<String>,
    /// The caller's display name, when present.
    pub from_name: Option<String>,
    /// The other participants' display names / MRIs, when present.
    pub participants: Vec<String>,
    /// The offered `callModalities` (e.g. `["Audio", "Video"]`).
    pub modalities: Vec<String>,
    /// Whether the offer includes video.
    pub has_video: bool,
    /// Whether the offer includes screen sharing.
    pub has_screen_share: bool,
    /// The SDP offer blob, when present.
    pub sdp_offer: Option<String>,
    /// The `acceptance` link, when present.
    pub acceptance_url: Option<String>,
    /// The `mediaAnswer` link, when present.
    pub media_answer_url: Option<String>,
    /// The `end` link, when present.
    pub end_url: Option<String>,
    /// The full parsed notification (kept so accept/decline can pass it on).
    pub notification: CallNotification,
}

impl IncomingCall {
    /// The best display label for the caller.
    pub fn caller_label(&self) -> &str {
        match self.from_name.as_deref() {
            Some(name) if !name.trim().is_empty() => name,
            _ => self.from_mri.as_deref().unwrap_or("Unknown caller"),
        }
    }

    /// Whether the invitation names a usable acceptance link.
    pub fn can_accept(&self) -> bool {
        self.acceptance_url.is_some()
    }
}

/// Parse an incoming-call frame into an [`IncomingCall`], or `None` when the
/// JSON is not a call invitation (the parser is total — it never panics).
///
/// A notification without a `callInvitation` (e.g. a conversation-update frame)
/// yields `None`.
pub fn parse_incoming(json: &str) -> Option<IncomingCall> {
    let notification = parse_call_notification(json)?;
    let invitation = notification.call_invitation.as_ref()?;

    let modalities = invitation.call_modalities.clone().unwrap_or_default();
    let has_video = modalities.iter().any(|m| m.eq_ignore_ascii_case("video"));
    let has_screen_share = modalities
        .iter()
        .any(|m| m.to_ascii_lowercase().contains("screen"));

    let links = invitation.links.as_ref();
    let (from_mri, from_name, participants) = match notification.participants.as_ref() {
        Some(participants) => {
            let from = participants.from.as_ref();
            let names = participants
                .to
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(participant_label)
                .collect();
            (
                from.and_then(|p| p.id.clone()),
                from.and_then(|p| p.display_name.clone()),
                names,
            )
        }
        None => (None, None, Vec::new()),
    };

    Some(IncomingCall {
        call_id: notification
            .debug_content
            .as_ref()
            .and_then(|d| d.call_id.clone()),
        from_mri,
        from_name,
        participants,
        modalities,
        has_video,
        has_screen_share,
        sdp_offer: invitation
            .media_content
            .as_ref()
            .and_then(|m| m.blob.clone()),
        acceptance_url: links.and_then(|l| l.acceptance.clone()),
        media_answer_url: links.and_then(|l| l.media_answer.clone()),
        end_url: links.and_then(|l| l.end.clone()),
        notification,
    })
}

/// A participant's display name, falling back to its id.
fn participant_label(participant: &Participant) -> String {
    match participant.display_name.as_deref() {
        Some(name) if !name.trim().is_empty() => name.to_string(),
        _ => participant.id.clone().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic incoming-call frame: a video offer with acceptance,
    /// media-answer and end links.
    fn invitation_json() -> String {
        serde_json::json!({
            "callInvitation": {
                "callModalities": ["Audio", "Video"],
                "links": {
                    "acceptance": "https://conv.example/acceptance",
                    "end": "https://conv.example/end",
                    "mediaAnswer": "https://conv.example/mediaAnswer",
                    "p2pForkNotification": null
                },
                "mediaContent": {
                    "contentType": "application/sdp",
                    "blob": "v=0\r\no=- 0 0 IN IP4 10.0.0.1\r\nm=audio 21730 RTP/SAVP 0\r\n"
                }
            },
            "participants": {
                "from": {
                    "id": "8:orgid:caller-guid",
                    "displayName": "Bob Jones",
                    "endpointId": "endpoint-1"
                },
                "to": [
                    { "id": "8:orgid:self-guid", "displayName": "Alice Smith" }
                ]
            },
            "debugContent": { "callId": "call-1234" }
        })
        .to_string()
    }

    #[test]
    fn parses_a_realistic_video_invitation() {
        let call = parse_incoming(&invitation_json()).expect("incoming call");
        assert_eq!(call.call_id.as_deref(), Some("call-1234"));
        assert_eq!(call.from_mri.as_deref(), Some("8:orgid:caller-guid"));
        assert_eq!(call.from_name.as_deref(), Some("Bob Jones"));
        assert_eq!(call.caller_label(), "Bob Jones");
        assert_eq!(call.participants, vec!["Alice Smith".to_string()]);
        assert_eq!(
            call.modalities,
            vec!["Audio".to_string(), "Video".to_string()]
        );
        assert!(call.has_video);
        assert!(!call.has_screen_share);
        assert!(call.sdp_offer.as_deref().unwrap().contains("m=audio"));
        assert_eq!(
            call.acceptance_url.as_deref(),
            Some("https://conv.example/acceptance")
        );
        assert_eq!(
            call.media_answer_url.as_deref(),
            Some("https://conv.example/mediaAnswer")
        );
        assert_eq!(call.end_url.as_deref(), Some("https://conv.example/end"));
        assert!(call.can_accept());
    }

    #[test]
    fn parses_a_screen_share_modality() {
        let json = serde_json::json!({
            "callInvitation": { "callModalities": ["Audio", "ScreenViewer"] },
            "participants": { "from": { "id": "8:orgid:x" } }
        })
        .to_string();
        let call = parse_incoming(&json).expect("incoming call");
        assert!(!call.has_video);
        assert!(call.has_screen_share);
        // No links: the call is visible but not accept-able through the wire.
        assert!(!call.can_accept());
        assert_eq!(call.caller_label(), "8:orgid:x");
    }

    #[test]
    fn non_call_and_malformed_frames_yield_none() {
        assert!(parse_incoming("not json").is_none());
        assert!(parse_incoming("{}").is_none());
        // A notification without a callInvitation (e.g. a conversation update).
        assert!(parse_incoming(&serde_json::json!({"participants": {}}).to_string()).is_none());
    }

    #[test]
    fn parses_a_stringified_body_wrapper() {
        // The vendored parser also unwraps a `body`-as-string envelope.
        let inner = invitation_json();
        let wrapped = serde_json::json!({ "body": inner }).to_string();
        let call = parse_incoming(&wrapped).expect("unwrapped");
        assert_eq!(call.call_id.as_deref(), Some("call-1234"));
    }
}
