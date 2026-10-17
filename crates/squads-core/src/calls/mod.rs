//! Audio/video calls (milestone **M9**).
//!
//! This module is the call layer: the UI-facing [`CallState`] machine, the
//! epconv signalling service, the media-session seam, incoming-call handling and
//! call recording. It **wraps** the vendored `ost::calling` protocol code
//! (ADR-001) rather than reimplementing any wire shape.
//!
//! ```text
//!   place/accept/end ──▶ CallService ──▶ CallSignaller ──▶ vendored epconv
//!                          │  │
//!                          │  └──▶ MediaSession (LoopbackMedia by default)
//!                          └─────▶ Store::upsert_call_log (schema v6)
//!   Trouter frame ──▶ CallService::on_notification ──▶ IncomingCall
//! ```
//!
//! ## What is tested offline
//!
//! Everything except a live call: the state machine, the SDP parse/answer, the
//! SRTP/RTP primitives, the full media loopback (`macav::call_dry_run`), the
//! signalling and recording **request shapes** against local TCP stubs, and the
//! incoming-call parser. Real device capture/playback is behind default-off
//! features ([`AudioMedia`]/[`VideoMedia`]) and is never constructed in tests.
//!
//! ## Known limits
//!
//! * **Incoming calls cannot arrive through the M4 bus** — the vendored Trouter
//!   filter drops call frames (B-008). See [`incoming`] for the recommended fix.
//! * The real `trouter_surl` comes from the negotiated Trouter session, not from
//!   `region_gtms`; see [`signaling`] and B-011.
//! * Live call acceptance/recording cannot be exercised here (no tenant, no
//!   device, no socket — B-002).

pub mod incoming;
pub mod media;
pub mod recording;
pub mod signaling;
pub mod state;

pub use incoming::{IncomingCall, parse_incoming};
pub use media::{FRAME_SAMPLES, LoopbackMedia, MediaKind, MediaSession};
pub use recording::{CallRecorder, CallRecording, RecordingRequest};
pub use signaling::{
    AcceptedCall, CallEndpoints, CallIdentity, CallSignaller, PlaceCallRequest, PlacedCall,
};
pub use state::{CallMachine, CallState, EndReason, TransitionError};

#[cfg(feature = "audio")]
pub use media::AudioMedia;
#[cfg(feature = "video-capture")]
pub use media::VideoMedia;

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use ost::calling::signaling::TeamsRegion;

use crate::auth::tokens::now_unix;
use crate::error::{CoreError, Result};
use crate::model::{CallDirection, CallLogEntry, CallOutcome};
use crate::notifications::{LogNotifier, Notification, NotificationKind, Notifier};
use crate::store::Store;

/// The tokens a call needs (kept together so the service is one constructor
/// argument). Never printed: the manual [`fmt::Debug`] for [`CallService`]
/// redacts them.
#[derive(Clone, Default)]
pub struct CallCredentials {
    /// The IC3 token (epconv `Bearer`).
    pub ic3_token: String,
    /// The skype token (acceptance/media-answer `X-Skypetoken`).
    pub skype_token: String,
    /// The recorder (FlightProxy) token.
    pub recorder_token: String,
}

/// The call service the UI drives: signalling + state + call history.
pub struct CallService {
    profile: String,
    store: Arc<Store>,
    signaller: CallSignaller,
    credentials: CallCredentials,
    identity: Option<CallIdentity>,
    local_ip: String,
    notifier: Arc<dyn Notifier>,
    recorder: CallRecorder,
    machine: Mutex<CallMachine>,
    current: Mutex<Option<ActiveCall>>,
}

/// The in-flight call's bookkeeping (for the log and the state machine).
#[derive(Debug, Clone)]
struct ActiveCall {
    log_id: String,
    thread_id: String,
    direction: CallDirection,
    started_at: i64,
    participants: Vec<String>,
}

impl CallService {
    /// Build over a shared store and a signalling service.
    pub fn new(profile: impl Into<String>, store: Arc<Store>, signaller: CallSignaller) -> Self {
        Self {
            profile: profile.into(),
            store,
            signaller,
            credentials: CallCredentials::default(),
            identity: None,
            local_ip: "127.0.0.1".to_string(),
            notifier: Arc::new(LogNotifier),
            recorder: CallRecorder::new(),
            machine: Mutex::new(CallMachine::new()),
            current: Mutex::new(None),
        }
    }

    /// Set the tokens used by signalling.
    pub fn with_credentials(mut self, credentials: CallCredentials) -> Self {
        self.credentials = credentials;
        self
    }

    /// Set who we are (required to place a call).
    pub fn with_identity(mut self, identity: CallIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Set the local IP advertised in generated SDP.
    pub fn with_local_ip(mut self, local_ip: impl Into<String>) -> Self {
        self.local_ip = local_ip.into();
        self
    }

    /// Set the notifier (missed-call notifications are delivered through it).
    pub fn with_notifier(mut self, notifier: Arc<dyn Notifier>) -> Self {
        self.notifier = notifier;
        self
    }

    /// Set the recorder service.
    pub fn with_recorder(mut self, recorder: CallRecorder) -> Self {
        self.recorder = recorder;
        self
    }

    /// The account profile.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The shared local store.
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// The signalling service.
    pub fn signaller(&self) -> &CallSignaller {
        &self.signaller
    }

    /// The recording service.
    pub fn recorder(&self) -> &CallRecorder {
        &self.recorder
    }

    /// The current call state.
    pub fn state(&self) -> CallState {
        self.machine().state().clone()
    }

    /// The direction of the current/last call, when established.
    pub fn direction(&self) -> Option<CallDirection> {
        self.machine().direction()
    }

    fn machine(&self) -> MutexGuard<'_, CallMachine> {
        self.machine.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn current_call(&self) -> Option<ActiveCall> {
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Handle a raw incoming-call frame: parse it, move to `Ringing` and record
    /// the start of the call. Returns `None` for anything that is not a call
    /// invitation.
    ///
    /// This is the seam a realtime producer would call — but no producer exists
    /// today because the M4 bus filters call frames out (B-008, see
    /// [`incoming`]).
    pub fn on_notification(&self, json: &str) -> Option<IncomingCall> {
        let incoming = incoming::parse_incoming(json)?;
        // A second simultaneous call is surfaced but does not disturb an active
        // call's state; the UI decides what to do.
        let _ = self.machine().ring();

        let active = ActiveCall {
            log_id: incoming
                .call_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            // The invitation does not carry the thread id; it is filled in when
            // the call is accepted (or left empty when it is missed).
            thread_id: String::new(),
            direction: CallDirection::Incoming,
            started_at: now_unix() as i64,
            participants: participant_labels(&incoming),
        };
        self.record_start(&active);
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(active);
        Some(incoming)
    }

    /// Place a call: create the conversation, join it with our SDP offer and
    /// (for a 1:1) invite the callee. The state moves `Idle`/`Ended` → `Placing`
    /// → `Connecting`; a failure ends the call with [`EndReason::Failed`].
    ///
    /// The remote *answer* is not observable over the M4 bus (B-008), so the
    /// caller drives [`CallService::mark_connected`] once media is up.
    pub async fn place(
        &self,
        thread_id: &str,
        callee_mri: Option<&str>,
        video: bool,
    ) -> Result<PlacedCall> {
        let identity = self.identity.as_ref().ok_or_else(|| {
            CoreError::NotReady("call service has no identity to place a call with".into())
        })?;
        self.machine()
            .place()
            .map_err(|e| CoreError::NotReady(e.to_string()))?;

        let active = ActiveCall {
            log_id: uuid::Uuid::new_v4().to_string(),
            thread_id: thread_id.to_string(),
            direction: CallDirection::Outgoing,
            started_at: now_unix() as i64,
            participants: callee_mri.map(|m| vec![m.to_string()]).unwrap_or_default(),
        };
        self.record_start(&active);
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(active);

        let region = TeamsRegion::default();
        let mut request = PlaceCallRequest::new(&self.credentials.ic3_token, identity, thread_id)
            .with_video(video)
            .with_local_ip(self.local_ip.as_str())
            .with_region(&region);
        if let Some(callee) = callee_mri {
            request = request.with_callee(callee);
        }

        match self.signaller.place_call(&request).await {
            Ok(placed) => {
                let _ = self.machine().connecting();
                Ok(placed)
            }
            Err(error) => {
                self.fail(&error);
                Err(error)
            }
        }
    }

    /// Accept an incoming call: POST the acceptance, generate an SDP answer from
    /// the invitation and send the media answer. The state moves `Ringing` →
    /// `Connecting`; the caller drives [`CallService::mark_connected`] once media
    /// is up.
    pub async fn accept(&self, incoming: &IncomingCall, video: bool) -> Result<AcceptedCall> {
        self.machine()
            .connecting()
            .map_err(|e| CoreError::NotReady(e.to_string()))?;

        match self
            .signaller
            .accept_incoming(
                &incoming.notification,
                &self.credentials.skype_token,
                &self.local_ip,
                video,
            )
            .await
        {
            Ok(accepted) => Ok(accepted),
            Err(error) => {
                self.fail(&error);
                Err(error)
            }
        }
    }

    /// Mark the call connected (media is up). `Connecting` → `Connected`.
    pub fn mark_connected(&self) -> Result<()> {
        self.machine()
            .connected()
            .map_err(|e| CoreError::NotReady(e.to_string()))
    }

    /// Decline a ringing incoming call: POST the end link and end the call with
    /// [`EndReason::Declined`].
    pub async fn decline(&self, incoming: &IncomingCall) -> Result<()> {
        self.signaller
            .end_call(&incoming.notification, &self.credentials.skype_token)
            .await?;
        let _ = self.machine().end(EndReason::Declined);
        self.record_end(CallOutcome::Declined);
        Ok(())
    }

    /// Hang up a connected call: POST the end link and end the call with
    /// [`EndReason::LocalHangup`], recording it as completed.
    pub async fn end(&self, incoming: &IncomingCall) -> Result<()> {
        self.signaller
            .end_call(&incoming.notification, &self.credentials.skype_token)
            .await?;
        let _ = self.machine().end(EndReason::LocalHangup);
        self.record_end(CallOutcome::Completed);
        Ok(())
    }

    /// Record an unanswered incoming call as **missed** and deliver a
    /// [`NotificationKind::MissedCall`] notification (M8's placeholder kind,
    /// finally produced — see ADR-022).
    pub fn missed(&self, incoming: &IncomingCall) -> Option<Notification> {
        let _ = self.machine().end(EndReason::Missed);
        self.record_end(CallOutcome::Missed);

        let notification = Notification {
            kind: NotificationKind::MissedCall,
            chat_id: incoming.from_mri.clone().unwrap_or_default(),
            title: incoming.caller_label().to_string(),
            body: "Missed call".to_string(),
            mention: false,
        };
        self.notifier.notify(&notification);
        Some(notification)
    }

    /// Reset to `Idle` after a finished call (the UI dismisses it).
    pub fn reset(&self) {
        self.machine().reset();
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// End the current call with a failure and record it.
    fn fail(&self, error: impl fmt::Display) {
        let _ = self.machine().end(EndReason::Failed(error.to_string()));
        self.record_end(CallOutcome::Failed);
    }

    /// Write the "call started" row.
    fn record_start(&self, active: &ActiveCall) {
        let entry = CallLogEntry::started(
            active.log_id.clone(),
            active.thread_id.clone(),
            active.direction,
            active.started_at,
        )
        .with_participants(active.participants.clone());
        if let Err(error) = self.store.upsert_call_log(&self.profile, &[entry]) {
            tracing::warn!(%error, "failed to record call start");
        }
    }

    /// Update the current call's row with its terminal outcome.
    fn record_end(&self, outcome: CallOutcome) {
        let Some(active) = self.current_call() else {
            return;
        };
        let entry = CallLogEntry::started(
            active.log_id,
            active.thread_id,
            active.direction,
            active.started_at,
        )
        .with_participants(active.participants)
        .finished(now_unix() as i64, outcome);
        if let Err(error) = self.store.upsert_call_log(&self.profile, &[entry]) {
            tracing::warn!(%error, "failed to record call end");
        }
    }
}

impl fmt::Debug for CallService {
    /// Redacting: identity, state and endpoints only — never the tokens.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallService")
            .field("profile", &self.profile)
            .field("state", &self.state())
            .field("has_identity", &self.identity.is_some())
            .field("signaller", &self.signaller)
            .finish_non_exhaustive()
    }
}

/// Participant labels for the call log: the caller first, then the others.
fn participant_labels(incoming: &IncomingCall) -> Vec<String> {
    let mut labels = Vec::new();
    if let Some(from) = incoming
        .from_name
        .clone()
        .or_else(|| incoming.from_mri.clone())
    {
        labels.push(from);
    }
    labels.extend(incoming.participants.iter().cloned());
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{StubResponse, http_stub, http_stub_with};
    use crate::notifications::RecordingNotifier;

    fn identity() -> CallIdentity {
        CallIdentity::from_mri("8:orgid:self-guid", "Alice", "tenant-1")
    }

    fn credentials() -> CallCredentials {
        CallCredentials {
            ic3_token: "ic3-token".into(),
            skype_token: "skype-token".into(),
            recorder_token: "recorder-token".into(),
        }
    }

    fn service(base: &str, store: Arc<Store>) -> CallService {
        CallService::new(
            "default",
            store,
            CallSignaller::with_http(
                reqwest::Client::new(),
                CallEndpoints::new(format!("{base}/epconv"), String::new()),
            ),
        )
        .with_credentials(credentials())
        .with_identity(identity())
        .with_local_ip("127.0.0.1")
    }

    fn invitation_json(base: &str) -> String {
        serde_json::json!({
            "callInvitation": {
                "callModalities": ["Audio", "Video"],
                "links": {
                    "acceptance": format!("{base}/acceptance"),
                    "end": format!("{base}/end"),
                    "mediaAnswer": format!("{base}/mediaAnswer")
                },
                "mediaContent": {
                    "contentType": "application/sdp",
                    "blob": "v=0\r\no=- 0 0 IN IP4 10.0.0.1\r\nm=audio 30000 RTP/SAVP 0\r\na=ice-ufrag:abcd\r\na=ice-pwd:password123456789012\r\na=candidate:1 1 UDP 2130706431 10.0.0.1 30000 typ host\r\na=crypto:2 AES_CM_128_HMAC_SHA1_80 inline:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA|2^31\r\n"
                }
            },
            "participants": {
                "from": { "id": "8:orgid:caller", "displayName": "Bob Jones" }
            },
            "debugContent": { "callId": "call-9" }
        })
        .to_string()
    }

    #[test]
    fn fresh_service_is_idle() {
        let service = service(
            "http://127.0.0.1:9",
            Arc::new(Store::open_in_memory().unwrap()),
        );
        assert_eq!(service.state(), CallState::Idle);
        assert!(service.direction().is_none());
        assert_eq!(service.profile(), "default");
        // Debug redacts the tokens.
        let dbg = format!("{service:?}");
        assert!(dbg.contains("CallService"));
        assert!(!dbg.contains("ic3-token"));
        assert!(!dbg.contains("skype-token"));
    }

    #[tokio::test]
    async fn place_moves_to_connecting_and_records_the_call() {
        let (base, _seen, task) = http_stub_with(|base| {
            vec![
                StubResponse::ok(format!(r#"{{"conversationController":"{base}/conv/abc"}}"#)),
                StubResponse::ok("{}"),
                StubResponse::ok("{}"),
            ]
        })
        .await;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let service = service(&base, Arc::clone(&store));

        let placed = service
            .place("19:alice_bob@unq.gbl.spaces", Some("8:orgid:bob"), false)
            .await
            .expect("place");
        assert_eq!(placed.thread_id, "19:alice_bob@unq.gbl.spaces");
        assert_eq!(service.state(), CallState::Connecting);
        assert_eq!(service.direction(), Some(CallDirection::Outgoing));

        service.mark_connected().expect("connected");
        assert_eq!(service.state(), CallState::Connected);

        // A row was recorded for the call, in progress.
        let log = store.call_log("default", 10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].direction, CallDirection::Outgoing);
        assert_eq!(log[0].thread_id, "19:alice_bob@unq.gbl.spaces");
        assert_eq!(log[0].participants, vec!["8:orgid:bob".to_string()]);
        assert_eq!(log[0].outcome, CallOutcome::Unknown);
        assert!(log[0].ended_at.is_none());

        task.await.expect("stub drains");
    }

    #[tokio::test]
    async fn a_failed_placement_ends_the_call_and_logs_failure() {
        let (base, _seen, task) = http_stub(vec![StubResponse::status(500)]).await;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let service = service(&base, Arc::clone(&store));

        assert!(
            service
                .place("19:a_b@unq.gbl.spaces", None, false)
                .await
                .is_err()
        );
        assert!(service.state().is_terminal());
        assert!(matches!(
            service.state().end_reason(),
            Some(EndReason::Failed(_))
        ));
        let log = store.call_log("default", 10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].outcome, CallOutcome::Failed);
        assert!(log[0].ended_at.is_some());

        task.await.expect("stub drains");
    }

    #[tokio::test]
    async fn on_notification_rings_and_accept_connects() {
        let (base, seen, task) =
            http_stub(vec![StubResponse::ok("{}"), StubResponse::ok("{}")]).await;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let service = service(&base, Arc::clone(&store));

        let incoming = service
            .on_notification(&invitation_json(&base))
            .expect("incoming call");
        assert_eq!(service.state(), CallState::Ringing);
        assert_eq!(service.direction(), Some(CallDirection::Incoming));
        assert_eq!(incoming.from_name.as_deref(), Some("Bob Jones"));
        assert!(incoming.has_video);

        let accepted = service.accept(&incoming, true).await.expect("accept");
        assert!(accepted.sdp_answer.contains("m=audio"));
        assert_eq!(service.state(), CallState::Connecting);
        service.mark_connected().unwrap();
        assert_eq!(service.state(), CallState::Connected);

        // The call was recorded with the caller as a participant.
        let log = store.call_log("default", 10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].id, "call-9");
        assert_eq!(log[0].participants, vec!["Bob Jones".to_string()]);

        task.await.expect("stub drains");
        let requests = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(requests.len(), 2, "acceptance + media answer");
    }

    #[tokio::test]
    async fn decline_ends_the_call_as_declined() {
        let (base, _seen, task) = http_stub(vec![StubResponse::ok("{}")]).await;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let service = service(&base, Arc::clone(&store));

        let incoming = service
            .on_notification(&invitation_json(&base))
            .expect("incoming call");
        service.decline(&incoming).await.expect("decline");
        assert_eq!(service.state().end_reason(), Some(&EndReason::Declined));

        let log = store.call_log("default", 10).unwrap();
        assert_eq!(log[0].outcome, CallOutcome::Declined);
        task.await.expect("stub drains");
    }

    #[test]
    fn missed_records_and_notifies() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let recorder = Arc::new(RecordingNotifier::new());
        let service =
            service("http://127.0.0.1:9", Arc::clone(&store)).with_notifier(recorder.clone());

        let incoming = service
            .on_notification(&invitation_json("http://127.0.0.1:9"))
            .expect("incoming call");
        let notification = service.missed(&incoming).expect("notification");
        assert_eq!(notification.kind, NotificationKind::MissedCall);
        assert_eq!(notification.title, "Bob Jones");
        assert_eq!(recorder.len(), 1);
        assert_eq!(service.state().end_reason(), Some(&EndReason::Missed));

        let log = store.call_log("default", 10).unwrap();
        assert_eq!(log[0].outcome, CallOutcome::Missed);
        assert!(log[0].duration_secs.is_some());
    }

    #[tokio::test]
    async fn end_hangs_up_a_connected_call() {
        let (base, _seen, task) = http_stub(vec![
            StubResponse::ok("{}"),
            StubResponse::ok("{}"),
            StubResponse::ok("{}"),
        ])
        .await;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let service = service(&base, Arc::clone(&store));
        let incoming = service
            .on_notification(&invitation_json(&base))
            .expect("incoming");
        service.accept(&incoming, false).await.expect("accept");
        service.mark_connected().unwrap();

        service.end(&incoming).await.expect("end");
        assert_eq!(service.state().end_reason(), Some(&EndReason::LocalHangup));
        assert_eq!(
            store.call_log("default", 10).unwrap()[0].outcome,
            CallOutcome::Completed
        );
        task.await.expect("stub drains");
    }

    #[test]
    fn non_call_notification_is_ignored() {
        let service = service(
            "http://127.0.0.1:9",
            Arc::new(Store::open_in_memory().unwrap()),
        );
        assert!(service.on_notification("{}").is_none());
        assert_eq!(service.state(), CallState::Idle);
    }
}
