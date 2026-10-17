//! Call recording (milestone **M9**).
//!
//! Teams records a call by injecting Microsoft's **recorder bot** into the
//! conversation, then driving the FlightProxy call-recorder service (start
//! transcription, start recording). The vendored
//! [`ost::calling::recording`] module implements that flow; this module **wraps**
//! it (ADR-001) behind a small service.
//!
//! ## What is callable offline
//!
//! The vendored flow is **not** macOS/AVFoundation-gated — it is plain HTTP — but
//! the *orchestrator* [`ost::calling::recording::start_call_recording`] needs a
//! **live Trouter socket** and a **live recorder service** (the recorder
//! endpoint and session id are assigned per call and arrive in the
//! `addParticipantSuccess` callback, so they cannot be hard-coded). We therefore:
//!
//! * wrap the two socket-free steps as [`CallRecorder::add_bot`] and
//!   [`CallRecorder::stop`], and **test their exact request shapes** against a
//!   local TCP stub (no live service);
//! * expose the full orchestrator as [`CallRecorder::start`], documented as
//!   **live-only** (it takes a `&mut TrouterSocket` and is not unit-tested here);
//! * keep a pure [`derive_add_participant_url`] / [`extract_conversation_id`]
//!   pair (mirroring the vendored private helpers) so the URL joins are tested
//!   without a socket.
//!
//! Recording live cannot be exercised in this environment (B-002): there is no
//! tenant, no live recorder service and no socket. See `docs/BLOCKERS.md` (B-011).

use ost::calling::recording::{self, RecordingParams, RecordingSession};
use ost::calling::signaling::TeamsRegion;
use ost::trouter::websocket::TrouterSocket;

use crate::calls::signaling::CallIdentity;
use crate::error::Result;

/// A running recording, kept so it can be stopped.
///
/// Wraps the vendored [`RecordingSession`] (which carries the recorder base URL,
/// session id and the tokens the stop request needs) plus the thread it belongs
/// to and when it started.
pub struct CallRecording {
    session: RecordingSession,
    thread_id: String,
    started_at: i64,
}

impl CallRecording {
    /// Wrap a vendored recording session.
    pub fn from_session(session: RecordingSession, thread_id: impl Into<String>) -> Self {
        Self {
            session,
            thread_id: thread_id.into(),
            started_at: crate::auth::tokens::now_unix() as i64,
        }
    }

    /// The thread this recording belongs to.
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Unix seconds when the recording started.
    pub fn started_at(&self) -> i64 {
        self.started_at
    }

    /// The recorder conversation/session id.
    pub fn conversation_id(&self) -> &str {
        &self.session.conversation_id
    }
}

/// Everything the recording flow needs beyond the tokens.
pub struct RecordingRequest<'a> {
    /// Who we are (caller MRI, display name, participant id is separate).
    pub identity: &'a CallIdentity,
    /// The thread the call is in.
    pub thread_id: &'a str,
    /// Our endpoint id for the call leg.
    pub endpoint_id: &'a str,
    /// Our participant id for the call leg.
    pub participant_id: &'a str,
    /// The call chain id.
    pub chain_id: &'a str,
    /// The call message id.
    pub message_id: &'a str,
    /// The conversation controller URL from call placement.
    pub conversation_controller: &'a str,
    /// The exact `addParticipant` URL from the epconv response, when present
    /// (otherwise it is derived from the controller with
    /// [`derive_add_participant_url`]).
    pub add_participant_url: Option<&'a str>,
    /// The IC3 token for the conversation API.
    pub ic3_token: &'a str,
    /// The recorder (FlightProxy) token.
    pub recorder_token: &'a str,
    /// The skype token.
    pub skype_token: &'a str,
    /// The Trouter callback path prefix.
    pub trouter_surl: &'a str,
    /// Teams cloud region.
    pub region: &'a TeamsRegion,
}

/// The recording service: an injectable HTTP client wrapping the vendored flow.
pub struct CallRecorder {
    http: reqwest::Client,
}

impl CallRecorder {
    /// Build with the shared default HTTP client.
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    /// Build with an injected HTTP client (tests point it at a local stub).
    pub fn with_http(http: reqwest::Client) -> Self {
        Self { http }
    }

    /// Step 1 (socket-free): inject the recorder bot as a call participant.
    ///
    /// Returns the response body, which carries the recorder bot's participant
    /// entry (the recorder service URL is mined from it by the vendored flow).
    pub async fn add_bot(&self, req: &RecordingRequest<'_>) -> Result<String> {
        let add_url = req
            .add_participant_url
            .map(str::to_string)
            .unwrap_or_else(|| derive_add_participant_url(req.conversation_controller));
        let conversation_id =
            extract_conversation_id(req.conversation_controller).unwrap_or_default();
        let params = RecordingParams {
            caller_mri: req.identity.mri.as_str(),
            participant_id: req.participant_id,
            endpoint_id: req.endpoint_id,
            chain_id: req.chain_id,
            message_id: req.message_id,
            thread_id: req.thread_id,
            display_name: req.identity.display_name.as_str(),
            trouter_surl: req.trouter_surl,
            ic3_token: req.ic3_token,
            recorder_token: req.recorder_token,
            skype_token: req.skype_token,
            conversation_id: &conversation_id,
            add_participant_url: &add_url,
            region: req.region,
        };
        let body = recording::add_recorder_bot(&self.http, &params).await?;
        Ok(body)
    }

    /// Step 2 (socket-free): stop recording and transcription for an active
    /// session (two POSTs to the recorder service).
    pub async fn stop(&self, recording: &CallRecording) -> Result<()> {
        recording::stop_call_recording(&self.http, &recording.session).await?;
        Ok(())
    }

    /// The full flow: add the bot, wait for the recorder service on Trouter, start
    /// transcription, then start recording.
    ///
    /// **Live-only.** Takes a connected [`TrouterSocket`] and a live recorder
    /// service; not exercised offline (B-002/B-011). The vendored function
    /// reassembles the [`RecordingParams`] itself, so the two socket-free steps
    /// above are the tested surface.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        &self,
        ws: &mut TrouterSocket,
        req: &RecordingRequest<'_>,
    ) -> Result<CallRecording> {
        let session = recording::start_call_recording(
            &self.http,
            ws,
            req.conversation_controller,
            &req.identity.mri,
            req.participant_id,
            req.endpoint_id,
            req.chain_id,
            req.message_id,
            req.thread_id,
            &req.identity.display_name,
            req.trouter_surl,
            req.ic3_token,
            req.recorder_token,
            req.skype_token,
            req.add_participant_url,
            req.region,
        )
        .await?;
        Ok(CallRecording::from_session(session, req.thread_id))
    }
}

impl Default for CallRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CallRecorder {
    /// Redacting: the HTTP client holds no secrets we print.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallRecorder").finish_non_exhaustive()
    }
}

/// Derive the bot `addParticipant` URL from a conversation controller URL.
///
/// Mirrors the vendored private helper: append `/addParticipant`, preserving any
/// query string. Documented here because the vendored helper is not public.
pub fn derive_add_participant_url(conversation_controller: &str) -> String {
    if let Some(idx) = conversation_controller.find('?') {
        let (path, query) = conversation_controller.split_at(idx);
        format!("{}/addParticipant{}", path.trim_end_matches('/'), query)
    } else {
        format!(
            "{}/addParticipant",
            conversation_controller.trim_end_matches('/')
        )
    }
}

/// Extract the conversation id from a conversation controller URL
/// (`…/conv/{id}`), mirroring the vendored private helper.
pub fn extract_conversation_id(url: &str) -> Option<String> {
    let idx = url.find("/conv/")?;
    let after = &url[idx + "/conv/".len()..];
    let end = after.find(['/', '?']).unwrap_or(after.len());
    let id = &after[..end];
    (!id.is_empty()).then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{StubResponse, http_stub};
    use crate::calls::signaling::CallIdentity;

    /// Microsoft's well-known recorder bot MRI (from the vendored module).
    const RECORDER_BOT_MRI: &str = "28:bdd75849-e0a6-4cce-8fc1-d7c0d4da43e5";

    fn request<'a>(
        base_url: &'a str,
        identity: &'a CallIdentity,
        region: &'a TeamsRegion,
    ) -> RecordingRequest<'a> {
        RecordingRequest {
            identity,
            thread_id: "19:av-test@thread.tacv2",
            endpoint_id: "endpoint-1",
            participant_id: "participant-1",
            chain_id: "chain-1",
            message_id: "message-1",
            conversation_controller: base_url,
            add_participant_url: None,
            ic3_token: "ic3-token",
            recorder_token: "recorder-token",
            skype_token: "skype-token",
            trouter_surl: "/surl/",
            region,
        }
    }

    #[test]
    fn add_participant_url_is_derived_with_query_preserved() {
        assert_eq!(
            derive_add_participant_url("https://conv/abc"),
            "https://conv/abc/addParticipant"
        );
        assert_eq!(
            derive_add_participant_url("https://conv/abc/"),
            "https://conv/abc/addParticipant"
        );
        assert_eq!(
            derive_add_participant_url("https://conv/abc?k=v"),
            "https://conv/abc/addParticipant?k=v"
        );
    }

    #[test]
    fn conversation_id_is_extracted() {
        assert_eq!(
            extract_conversation_id("https://amer03-1.conv.skype.com/conv/abc123-def"),
            Some("abc123-def".to_string())
        );
        assert_eq!(
            extract_conversation_id("https://amer03-1.conv.skype.com/conv/abc123/more"),
            Some("abc123".to_string())
        );
        assert_eq!(extract_conversation_id("https://x/no-marker"), None);
    }

    #[tokio::test]
    async fn add_bot_posts_the_recorder_participant_shape() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok("{}")]).await;
        let recorder = CallRecorder::new();
        let identity = CallIdentity::from_mri("8:orgid:aaaa", "Alice", "tenant-1");
        let region = TeamsRegion::default();

        let controller = format!("{base}/conv/abc123");
        let req = request(&controller, &identity, &region);
        recorder.add_bot(&req).await.expect("add bot");

        task.await.expect("stub drains");
        let requests = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(requests.len(), 1);
        let r = &requests[0];
        assert!(r.starts_with("POST /conv/abc123/addParticipant "), "{r}");
        assert!(
            r.to_ascii_lowercase()
                .contains("authorization: bearer ic3-token"),
            "{r}"
        );
        assert!(r.contains(RECORDER_BOT_MRI), "{r}");
        assert!(r.contains("\"mode\":\"RecordingAndTranscription\""), "{r}");
        assert!(r.contains("\"callId\":\"chain-1\""), "{r}");
        assert!(
            r.contains("\"threadId\":\"19:av-test@thread.tacv2\""),
            "{r}"
        );
    }

    #[tokio::test]
    async fn add_bot_uses_an_explicit_add_participant_url_when_given() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok("{}")]).await;
        let recorder = CallRecorder::new();
        let identity = CallIdentity::from_mri("8:orgid:aaaa", "Alice", "tenant-1");
        let region = TeamsRegion::default();

        let controller = format!("{base}/conv/abc123");
        let explicit = format!("{base}/conv/abc123/add");
        let mut req = request(&controller, &identity, &region);
        req.add_participant_url = Some(&explicit);
        recorder.add_bot(&req).await.expect("add bot");

        task.await.expect("stub drains");
        let requests = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            requests[0].starts_with("POST /conv/abc123/add "),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn stop_posts_recording_then_transcription() {
        let (base, seen, task) =
            http_stub(vec![StubResponse::ok("{}"), StubResponse::ok("{}")]).await;

        let recorder = CallRecorder::new();
        let session = RecordingSession {
            recorder_base: format!("{base}/recorder"),
            conversation_id: "conv-1".to_string(),
            recorder_token: "recorder-token".to_string(),
            skype_token: "skype-token".to_string(),
            chain_id: "chain-1".to_string(),
            caller_mri: "8:orgid:aaaa".to_string(),
            participant_id: "participant-1".to_string(),
        };
        let recording = CallRecording::from_session(session, "19:av-test@thread.tacv2");
        assert_eq!(recording.thread_id(), "19:av-test@thread.tacv2");
        assert_eq!(recording.conversation_id(), "conv-1");

        recorder.stop(&recording).await.expect("stop");

        task.await.expect("stub drains");
        let requests = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0].starts_with("POST /recorder/v2/oncommand/conv-1 "),
            "{}",
            requests[0]
        );
        assert!(
            requests[0].contains("\"action\":\"stop\""),
            "{}",
            requests[0]
        );
        assert!(
            requests[0].contains("\"processingModes\":[\"recording\",\"realTimeTranscript\"]"),
            "{}",
            requests[0]
        );
        // The transcription stop follows.
        assert!(
            requests[1].contains("\"mode\":\"transcription\""),
            "{}",
            requests[1]
        );
    }
}
