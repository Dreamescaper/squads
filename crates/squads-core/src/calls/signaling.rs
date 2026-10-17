//! Call signalling (milestone **M9**).
//!
//! This module **wraps** the vendored `ost::calling::signaling` two-phase epconv
//! flow (ADR-001) behind a small, transport-injectable service. It never
//! reimplements a wire shape: every request goes through the vendored functions,
//! so a protocol fix upstream is inherited.
//!
//! ```text
//!   place:  create_conversation ──▶ join_conversation_with_sdp[_video] ──▶ invite_user
//!   accept: accept_call_with_video ──▶ send_media_answer
//!   end:    end_call
//! ```
//!
//! ## Testability
//!
//! [`CallSignaller`] takes an **injectable `reqwest::Client`** and **injectable
//! endpoints** ([`CallEndpoints`]: `epconv_url` + `trouter_surl`). Tests point
//! both at a local TCP stub and assert the exact method/path/headers/body, so the
//! whole signalling request shape is pinned offline.
//!
//! ## Endpoint convention
//!
//! [`CallEndpoints::from_region_gtms`] derives the endpoints from the M1
//! `region_gtms` blob:
//!
//! * `epconvUrl` (our convention) — else the vendored
//!   `calling_conversationServiceUrl` — else a derivation from
//!   `calling_potentialCallRequestUrl` (`…/api/v2/epconv`) — else the documented
//!   fallback [`DEFAULT_EPCONV_URL`] (a placeholder: the real epconv host is
//!   region-specific and comes from `region_gtms`).
//! * `trouterSurl` (our convention) — the Trouter callback path prefix the
//!   negotiated session supplies (`SessionResponse::surl`). We do **not** derive
//!   it from `region_gtms` because it is not there; the fallback is the empty
//!   string (relative callback paths), which is only meaningful once the real
//!   surl is known. Documented in `docs/BLOCKERS.md` (B-011).
//!
//! ## Media ports
//!
//! The SDP this module generates advertises **placeholder** host candidates
//! (`DEFAULT_AUDIO_PORT`/`DEFAULT_VIDEO_PORT`) because signalling and media are
//! separate concerns here; [`crate::calls::media`] owns the actual pipeline. A
//! caller that binds real sockets can pass its own ports to
//! [`PlaceCallRequest::with_ports`].

use std::fmt;

use serde_json::Value;

use ost::calling::signaling::{self, ConversationCallParams, TeamsRegion};
use ost::calling::{CallNotification, sdp};

use crate::error::{CoreError, Result};

/// Documented fallback epconv URL, used only when `region_gtms` names none.
///
/// The real epconv host is region-specific (a FlightProxy endpoint) and is
/// carried in `region_gtms`; this placeholder exists so endpoint derivation is
/// total, not because it is known to work.
pub const DEFAULT_EPCONV_URL: &str = "https://go.trouter.teams.microsoft.com/api/v2/epconv";

/// Placeholder local audio port advertised in generated SDP.
pub const DEFAULT_AUDIO_PORT: u16 = 20_000;
/// Placeholder local video port advertised in generated SDP.
pub const DEFAULT_VIDEO_PORT: u16 = 20_002;

/// The two signalling endpoints a call needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallEndpoints {
    /// Phase 1/2 epconv URL (conversation creation + join).
    pub epconv_url: String,
    /// Trouter callback path prefix (`SessionResponse::surl`).
    pub trouter_surl: String,
}

impl CallEndpoints {
    /// Explicit endpoints (tests, a caller that negotiated its own Trouter surl).
    pub fn new(epconv_url: impl Into<String>, trouter_surl: impl Into<String>) -> Self {
        Self {
            epconv_url: epconv_url.into(),
            trouter_surl: trouter_surl.into(),
        }
    }

    /// Derive the endpoints from the `region_gtms` blob (see the module docs).
    pub fn from_region_gtms(region: &Value) -> Self {
        Self {
            epconv_url: derive_epconv_url(region).unwrap_or_else(|| DEFAULT_EPCONV_URL.to_string()),
            trouter_surl: region
                .get("trouterSurl")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }
    }

    /// Override the epconv URL.
    pub fn with_epconv_url(mut self, url: impl Into<String>) -> Self {
        self.epconv_url = url.into();
        self
    }

    /// Override the Trouter callback prefix.
    pub fn with_trouter_surl(mut self, surl: impl Into<String>) -> Self {
        self.trouter_surl = surl.into();
        self
    }
}

/// Derive the epconv URL from `region_gtms`, preferring our own `epconvUrl`
/// convention, then the vendored keys (mirrors the vendored `derive_epconv_url`).
fn derive_epconv_url(region: &Value) -> Option<String> {
    if let Some(url) = region.get("epconvUrl").and_then(Value::as_str) {
        return Some(url.to_string());
    }
    if let Some(url) = region
        .get("calling_conversationServiceUrl")
        .and_then(Value::as_str)
    {
        return Some(url.to_string());
    }
    let potential = region
        .get("calling_potentialCallRequestUrl")
        .and_then(Value::as_str)?;
    if let Some(idx) = potential.find("/api/v2/") {
        Some(format!("{}/api/v2/epconv", &potential[..idx]))
    } else {
        Some(potential.replace("/cc/v1/potentialcall", "/epconv"))
    }
}

/// Who we are, as the signalling requests need us (caller MRI, display name,
/// OID and tenant).
///
/// [`CallIdentity::from_skype_token`] decodes the M1 skype JWT (a pure,
/// offline operation) to recover the MRI and tenant, so a `Session` can build an
/// identity without an extra network call. `Debug` is redacting: it prints the
/// MRI/name but never the token (the token is not stored here at all).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallIdentity {
    /// Our MRI (`8:orgid:<guid>`).
    pub mri: String,
    /// Our display name.
    pub display_name: String,
    /// Our AAD object id (the guid part of the MRI).
    pub oid: String,
    /// Our tenant id (`tid` claim), when known.
    pub tenant_id: String,
}

impl CallIdentity {
    /// Build an identity from an MRI + display name + tenant, deriving the OID
    /// from the MRI (`8:orgid:<guid>` → `<guid>`, else the whole string).
    pub fn from_mri(
        mri: impl Into<String>,
        display_name: impl Into<String>,
        tenant_id: impl Into<String>,
    ) -> Self {
        let mri = mri.into();
        let oid = mri
            .strip_prefix("8:orgid:")
            .unwrap_or(mri.as_str())
            .to_string();
        Self {
            mri,
            display_name: display_name.into(),
            oid,
            tenant_id: tenant_id.into(),
        }
    }

    /// Decode a skype JWT and build the identity, or `None` when the token is
    /// not a JWT / names no user.
    ///
    /// Mirrors the vendored `extract_mri_from_skype_token`: the `skypeid` claim
    /// (`orgid:<guid>` → `8:orgid:<guid>`) wins, then the `oid` claim. The
    /// display name comes from `name` (falling back to an empty string) and the
    /// tenant from `tid`.
    pub fn from_skype_token(token: &str) -> Option<Self> {
        let claims = jwt_claims(token)?;
        let mri = match claims.get("skypeid").and_then(Value::as_str) {
            Some(skypeid)
                if skypeid.starts_with("orgid:") || skypeid.starts_with("teamsvisitor:") =>
            {
                format!("8:{}", skypeid)
            }
            Some(skypeid) => skypeid.to_string(),
            None => format!("8:orgid:{}", claims.get("oid").and_then(Value::as_str)?),
        };
        let display_name = claims
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let tenant_id = claims
            .get("tid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Some(CallIdentity::from_mri(mri, display_name, tenant_id))
    }
}

/// Decode a JWT payload (base64url, no padding) into its JSON claims.
fn jwt_claims(token: &str) -> Option<Value> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(payload))
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

/// Everything needed to place a call.
///
/// Build with [`PlaceCallRequest::new`] and the `with_*` builders; the defaults
/// are audio-only, no callee (a group/meeting thread call), `127.0.0.1` and the
/// AMER [`TeamsRegion`].
pub struct PlaceCallRequest<'a> {
    /// The IC3 token (`Bearer`) for epconv.
    pub ic3_token: &'a str,
    /// Who we are.
    pub identity: &'a CallIdentity,
    /// The 1:1/group thread to call.
    pub thread_id: &'a str,
    /// The callee MRI to invite (a 1:1 call); `None` for a group/meeting thread.
    pub callee_mri: Option<&'a str>,
    /// Whether to offer audio + video.
    pub video: bool,
    /// Local IP advertised in the SDP.
    pub local_ip: &'a str,
    /// Local audio port advertised in the SDP.
    pub audio_port: u16,
    /// Local video port advertised in the SDP.
    pub video_port: u16,
    /// Teams cloud region for the `ms-teams-*` headers.
    pub region: &'a TeamsRegion,
}

impl<'a> PlaceCallRequest<'a> {
    /// A minimal request: audio-only, no callee, `127.0.0.1`, AMER region.
    pub fn new(ic3_token: &'a str, identity: &'a CallIdentity, thread_id: &'a str) -> Self {
        static AMER: std::sync::OnceLock<TeamsRegion> = std::sync::OnceLock::new();
        Self {
            ic3_token,
            identity,
            thread_id,
            callee_mri: None,
            video: false,
            local_ip: "127.0.0.1",
            audio_port: DEFAULT_AUDIO_PORT,
            video_port: DEFAULT_VIDEO_PORT,
            region: AMER.get_or_init(TeamsRegion::default),
        }
    }

    /// Invite a specific callee (a 1:1 call).
    pub fn with_callee(mut self, callee_mri: &'a str) -> Self {
        self.callee_mri = Some(callee_mri);
        self
    }

    /// Offer audio + video.
    pub fn with_video(mut self, video: bool) -> Self {
        self.video = video;
        self
    }

    /// Advertise a specific local IP.
    pub fn with_local_ip(mut self, local_ip: &'a str) -> Self {
        self.local_ip = local_ip;
        self
    }

    /// Advertise specific local media ports.
    pub fn with_ports(mut self, audio_port: u16, video_port: u16) -> Self {
        self.audio_port = audio_port;
        self.video_port = video_port;
        self
    }

    /// Use a specific cloud region.
    pub fn with_region(mut self, region: &'a TeamsRegion) -> Self {
        self.region = region;
        self
    }
}

/// A placed call: the conversation controller, the negotiated media credentials
/// and the SDP offer that was sent.
#[derive(Debug, Clone)]
pub struct PlacedCall {
    /// The phase-2 conversation controller URL.
    pub conversation_controller: String,
    /// The CC active URL from the phase-2 response headers, when present.
    pub cc_active_url: Option<String>,
    /// Our endpoint id for this call leg.
    pub endpoint_id: String,
    /// Our participant id for this call leg.
    pub participant_id: String,
    /// The thread the call was placed in.
    pub thread_id: String,
    /// Whether the offer carried video.
    pub video: bool,
    /// The SDP offer sent in phase 2.
    pub sdp_offer: String,
    /// The audio SRTP crypto line from our offer.
    pub audio_crypto_line: String,
    /// The video SRTP crypto line from our offer (video only).
    pub video_crypto_line: Option<String>,
    /// Our audio ICE ufrag.
    pub audio_ice_ufrag: String,
    /// Our audio ICE password.
    pub audio_ice_pwd: String,
    /// Our video ICE ufrag (video only).
    pub video_ice_ufrag: Option<String>,
    /// Our video ICE password (video only).
    pub video_ice_pwd: Option<String>,
}

/// An accepted call: the SDP answer that was sent and the credentials to use for
/// the media leg.
#[derive(Debug, Clone)]
pub struct AcceptedCall {
    /// The SDP answer sent to the media-answer URL.
    pub sdp_answer: String,
    /// The audio SRTP crypto line from our answer.
    pub audio_crypto_line: String,
    /// The video SRTP crypto line from our answer (video only).
    pub video_crypto_line: Option<String>,
    /// Our audio ICE ufrag.
    pub audio_ice_ufrag: String,
    /// Our audio ICE password.
    pub audio_ice_pwd: String,
    /// Our video ICE ufrag (video only).
    pub video_ice_ufrag: Option<String>,
    /// Our video ICE password (video only).
    pub video_ice_pwd: Option<String>,
}

/// The signalling service: injectable HTTP client + endpoints, wrapping the
/// vendored epconv flow.
pub struct CallSignaller {
    http: reqwest::Client,
    endpoints: CallEndpoints,
}

impl CallSignaller {
    /// Build with the shared default HTTP client and explicit endpoints.
    pub fn new(endpoints: CallEndpoints) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoints,
        }
    }

    /// Build with an injected HTTP client (tests point it at a local stub).
    pub fn with_http(http: reqwest::Client, endpoints: CallEndpoints) -> Self {
        Self { http, endpoints }
    }

    /// The endpoints in use.
    pub fn endpoints(&self) -> &CallEndpoints {
        &self.endpoints
    }

    /// Replace the endpoints (e.g. after a Trouter session supplies the real
    /// `surl`).
    pub fn set_endpoints(&mut self, endpoints: CallEndpoints) {
        self.endpoints = endpoints;
    }

    /// Place a call: create the conversation, join it with our SDP offer, then
    /// (for a 1:1) invite the callee.
    pub async fn place_call(&self, req: &PlaceCallRequest<'_>) -> Result<PlacedCall> {
        let endpoint_id = uuid::Uuid::new_v4().to_string();
        let participant_id = uuid::Uuid::new_v4().to_string();
        let chain_id = uuid::Uuid::new_v4().to_string();
        let message_id = uuid::Uuid::new_v4().to_string();

        let offer = self.build_offer(req)?;

        let params = ConversationCallParams {
            ic3_token: req.ic3_token,
            trouter_surl: self.endpoints.trouter_surl.as_str(),
            caller_mri: req.identity.mri.as_str(),
            caller_display_name: req.identity.display_name.as_str(),
            endpoint_id: endpoint_id.as_str(),
            participant_id: participant_id.as_str(),
            thread_id: req.thread_id,
            chain_id: chain_id.as_str(),
            message_id: message_id.as_str(),
            caller_oid: req.identity.oid.as_str(),
            tenant_id: req.identity.tenant_id.as_str(),
            region: req.region,
        };

        let created =
            signaling::create_conversation(&self.http, &self.endpoints.epconv_url, &params).await?;
        let joined = signaling::join_conversation_with_sdp_video(
            &self.http,
            &created.conversation_controller,
            &params,
            &offer.sdp,
            req.video,
        )
        .await?;

        if let Some(callee) = req.callee_mri {
            signaling::invite_user(
                &self.http,
                &created.conversation_controller,
                &params,
                callee,
                req.video,
            )
            .await?;
        }

        Ok(PlacedCall {
            conversation_controller: created.conversation_controller,
            cc_active_url: joined.cc_active_url,
            endpoint_id,
            participant_id,
            thread_id: req.thread_id.to_string(),
            video: req.video,
            sdp_offer: offer.sdp,
            audio_crypto_line: offer.audio_crypto_line,
            video_crypto_line: offer.video_crypto_line,
            audio_ice_ufrag: offer.audio_ice_ufrag,
            audio_ice_pwd: offer.audio_ice_pwd,
            video_ice_ufrag: offer.video_ice_ufrag,
            video_ice_pwd: offer.video_ice_pwd,
        })
    }

    /// Build the SDP offer for a placement request (pure).
    fn build_offer(&self, req: &PlaceCallRequest<'_>) -> Result<OfferMaterial> {
        if req.video {
            let audio_ufrag = sdp::generate_ice_ufrag();
            let audio_pwd = sdp::generate_ice_pwd();
            let video_ufrag = sdp::generate_ice_ufrag();
            let video_pwd = sdp::generate_ice_pwd();
            let result = sdp::generate_av_sdp_offer(&sdp::AvSdpParams {
                local_ip: req.local_ip,
                audio_port: req.audio_port,
                video_port: req.video_port,
                audio_ufrag: &audio_ufrag,
                audio_pwd: &audio_pwd,
                video_ufrag: &video_ufrag,
                video_pwd: &video_pwd,
                audio_candidates: &[],
                video_candidates: &[],
                video_ssrc_base: ost::calling::video::generate_ssrc(),
                audio_ssrc: ost::calling::video::generate_ssrc(),
            });
            Ok(OfferMaterial {
                sdp: result.sdp,
                audio_crypto_line: result.audio_crypto_line,
                video_crypto_line: Some(result.video_crypto_line),
                audio_ice_ufrag: result.audio_ufrag,
                audio_ice_pwd: result.audio_pwd,
                video_ice_ufrag: Some(result.video_ufrag),
                video_ice_pwd: Some(result.video_pwd),
            })
        } else {
            let ufrag = sdp::generate_ice_ufrag();
            let pwd = sdp::generate_ice_pwd();
            let result = sdp::generate_sdp_offer(req.local_ip, req.audio_port, &ufrag, &pwd, &[]);
            Ok(OfferMaterial {
                sdp: result.sdp,
                audio_crypto_line: result.crypto_line,
                video_crypto_line: None,
                audio_ice_ufrag: ufrag,
                audio_ice_pwd: pwd,
                video_ice_ufrag: None,
                video_ice_pwd: None,
            })
        }
    }

    /// Accept an incoming call: POST the acceptance, generate an SDP answer from
    /// the invitation's offer, and send the media answer.
    pub async fn accept_incoming(
        &self,
        notification: &CallNotification,
        skype_token: &str,
        local_ip: &str,
        video: bool,
    ) -> Result<AcceptedCall> {
        let offer = offer_sdp(notification)?;
        let parsed = sdp::parse_sdp_offer(offer)?;
        let answer = sdp::generate_sdp_answer_full(
            local_ip,
            DEFAULT_AUDIO_PORT,
            DEFAULT_VIDEO_PORT,
            &parsed,
            &[],
            &[],
        );

        signaling::accept_call_with_video(&self.http, skype_token, notification, video).await?;
        signaling::send_media_answer(&self.http, skype_token, notification, &answer.sdp).await?;

        Ok(AcceptedCall {
            sdp_answer: answer.sdp,
            audio_crypto_line: answer.audio_crypto_line,
            video_crypto_line: answer.video_crypto_line,
            audio_ice_ufrag: answer.audio_ice_ufrag,
            audio_ice_pwd: answer.audio_ice_pwd,
            video_ice_ufrag: answer.video_ice_ufrag,
            video_ice_pwd: answer.video_ice_pwd,
        })
    }

    /// Send an SDP media answer for an incoming call (without the acceptance
    /// POST), for callers that accept separately.
    pub async fn send_media_answer(
        &self,
        notification: &CallNotification,
        skype_token: &str,
        sdp_answer: &str,
    ) -> Result<()> {
        signaling::send_media_answer(&self.http, skype_token, notification, sdp_answer).await?;
        Ok(())
    }

    /// End a call (decline a ringing call, or hang up a connected one) via the
    /// invitation's `end` link.
    pub async fn end_call(&self, notification: &CallNotification, skype_token: &str) -> Result<()> {
        signaling::end_call(&self.http, skype_token, notification).await?;
        Ok(())
    }
}

impl fmt::Debug for CallSignaller {
    /// Redacting: shows the endpoints, never the HTTP client internals.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallSignaller")
            .field("epconv_url", &self.endpoints.epconv_url)
            .field("trouter_surl", &self.endpoints.trouter_surl)
            .finish_non_exhaustive()
    }
}

/// The generated offer plus its credentials (internal).
struct OfferMaterial {
    sdp: String,
    audio_crypto_line: String,
    video_crypto_line: Option<String>,
    audio_ice_ufrag: String,
    audio_ice_pwd: String,
    video_ice_ufrag: Option<String>,
    video_ice_pwd: Option<String>,
}

/// The SDP offer blob carried by a call invitation, if any.
pub(crate) fn offer_sdp(notification: &CallNotification) -> Result<&str> {
    notification
        .call_invitation
        .as_ref()
        .and_then(|invitation| invitation.media_content.as_ref())
        .and_then(|content| content.blob.as_deref())
        .ok_or_else(|| CoreError::Protocol(anyhow::anyhow!("call invitation carries no SDP offer")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{StubResponse, http_stub, http_stub_with};
    use ost::calling::{CallInvitation, CallLinks, MediaContent};

    fn identity() -> CallIdentity {
        CallIdentity::from_mri("8:orgid:aaaa-bbbb", "Alice", "tenant-1")
    }

    fn jwt(claims: Value) -> String {
        use base64::Engine as _;
        let enc = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let header = enc(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = enc(claims.to_string().as_bytes());
        format!("{header}.{payload}.signature")
    }

    // ----- endpoint derivation --------------------------------------------

    #[test]
    fn endpoints_prefer_our_epconv_convention_then_vendored_keys() {
        // Our `epconvUrl` key wins.
        let ours = CallEndpoints::from_region_gtms(&serde_json::json!({
            "epconvUrl": "https://stub/epconv",
            "calling_conversationServiceUrl": "https://other/epconv",
            "trouterSurl": "/v4/f/x/",
        }));
        assert_eq!(ours.epconv_url, "https://stub/epconv");
        assert_eq!(ours.trouter_surl, "/v4/f/x/");

        // Then the vendored conversation-service key.
        let vendored = CallEndpoints::from_region_gtms(&serde_json::json!({
            "calling_conversationServiceUrl": "https://svc/epconv",
        }));
        assert_eq!(vendored.epconv_url, "https://svc/epconv");
        assert_eq!(vendored.trouter_surl, "");
    }

    #[test]
    fn endpoints_derive_from_the_potential_call_url() {
        // The `/api/v2/` form keeps the host and swaps the path.
        let a = CallEndpoints::from_region_gtms(&serde_json::json!({
            "calling_potentialCallRequestUrl": "https://fp.region/foo/api/v2/cc/v1/potentialcall",
        }));
        assert_eq!(a.epconv_url, "https://fp.region/foo/api/v2/epconv");

        // The legacy form replaces the suffix.
        let b = CallEndpoints::from_region_gtms(&serde_json::json!({
            "calling_potentialCallRequestUrl": "https://fp.region/cc/v1/potentialcall",
        }));
        assert_eq!(b.epconv_url, "https://fp.region/epconv");
    }

    #[test]
    fn endpoints_fall_back_to_the_documented_default() {
        let e = CallEndpoints::from_region_gtms(&serde_json::json!({}));
        assert_eq!(e.epconv_url, DEFAULT_EPCONV_URL);
        assert_eq!(e.trouter_surl, "");
        // Builders override either field.
        let e = CallEndpoints::default()
            .with_epconv_url("http://127.0.0.1:1/epconv")
            .with_trouter_surl("/surl/");
        assert_eq!(e.epconv_url, "http://127.0.0.1:1/epconv");
        assert_eq!(e.trouter_surl, "/surl/");
    }

    // ----- identity --------------------------------------------------------

    #[test]
    fn identity_from_mri_derives_the_oid() {
        let id = CallIdentity::from_mri("8:orgid:aaaa-bbbb", "Alice", "tenant-1");
        assert_eq!(id.mri, "8:orgid:aaaa-bbbb");
        assert_eq!(id.oid, "aaaa-bbbb");
        assert_eq!(id.display_name, "Alice");
        assert_eq!(id.tenant_id, "tenant-1");
    }

    #[test]
    fn identity_from_skype_token_reads_skypeid_then_oid() {
        let id = CallIdentity::from_skype_token(&jwt(serde_json::json!({
            "skypeid": "orgid:aaaa-bbbb",
            "name": "Alice",
            "tid": "tenant-1",
        })))
        .expect("identity");
        assert_eq!(id.mri, "8:orgid:aaaa-bbbb");
        assert_eq!(id.oid, "aaaa-bbbb");
        assert_eq!(id.display_name, "Alice");
        assert_eq!(id.tenant_id, "tenant-1");

        // Falls back to the `oid` claim when there is no skypeid.
        let id = CallIdentity::from_skype_token(&jwt(serde_json::json!({
            "oid": "cccc-dddd",
        })))
        .expect("identity");
        assert_eq!(id.mri, "8:orgid:cccc-dddd");
        assert_eq!(id.display_name, "");

        // A non-JWT or a token naming no user yields nothing (no panic).
        assert!(CallIdentity::from_skype_token("not-a-jwt").is_none());
        assert!(CallIdentity::from_skype_token(&jwt(serde_json::json!({"sub": "x"}))).is_none());
    }

    // ----- SDP parse + answer ---------------------------------------------

    /// A realistic incoming offer: audio + video, separate ICE credentials and
    /// SRTP crypto per m-line, host candidates.
    fn offer_fixture() -> String {
        [
            "v=0",
            "o=- 0 0 IN IP4 10.20.30.40",
            "s=session",
            "c=IN IP4 10.20.30.40",
            "b=CT:99980",
            "t=0 0",
            "a=x-mediabw:main-video send=12000;recv=12000",
            "m=audio 21730 RTP/SAVP 0",
            "a=rtpmap:0 PCMU/8000",
            "a=sendrecv",
            "a=rtcp-mux",
            "a=label:main-audio",
            "a=ice-ufrag:aUfr",
            "a=ice-pwd:aPwd1234567890123456",
            "a=candidate:1 1 UDP 2130706431 10.20.30.40 21730 typ host",
            "a=crypto:2 AES_CM_128_HMAC_SHA1_80 inline:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA|2^31",
            "m=video 21732 RTP/SAVP 122 121 123",
            "a=rtpmap:122 X-H264UC/90000",
            "a=sendrecv",
            "a=rtcp-mux",
            "a=label:main-video",
            "a=ice-ufrag:vUfr",
            "a=ice-pwd:vPwd1234567890123456",
            "a=candidate:1 1 UDP 2130706431 10.20.30.40 21732 typ host",
            "a=crypto:2 AES_CM_128_HMAC_SHA1_80 inline:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB|2^31",
            "",
        ]
        .join("\r\n")
    }

    #[test]
    fn parses_a_realistic_offer_and_generates_an_audio_video_answer() {
        let parsed = sdp::parse_sdp_offer(&offer_fixture()).expect("parse offer");
        assert_eq!(parsed.ice_ufrag, "aUfr");
        assert_eq!(parsed.ice_pwd, "aPwd1234567890123456");
        assert_eq!(parsed.candidate_ip, "10.20.30.40");
        assert_eq!(parsed.candidate_port, 21730);
        assert_eq!(parsed.crypto_lines.len(), 1);
        let video = parsed.video.as_ref().expect("video section");
        assert_eq!(video.ice_ufrag, "vUfr");
        assert_eq!(video.candidate_port, 21732);

        let answer = sdp::generate_sdp_answer_full("192.168.1.50", 20000, 20002, &parsed, &[], &[]);
        let sdp = &answer.sdp;
        // Both media lines are present with the right codecs.
        assert!(sdp.contains("m=audio 20000 RTP/SAVP 0"), "{sdp}");
        assert!(sdp.contains("a=rtpmap:0 PCMU/8000"), "{sdp}");
        assert!(sdp.contains("m=video 20002 RTP/SAVP 122 121 123"), "{sdp}");
        assert!(sdp.contains("a=rtpmap:122 X-H264UC/90000"), "{sdp}");
        // Fresh ICE credentials and a candidate per section.
        assert!(sdp.contains(&format!("a=ice-ufrag:{}", answer.audio_ice_ufrag)));
        assert!(sdp.contains(&format!(
            "a=ice-ufrag:{}",
            answer.video_ice_ufrag.clone().unwrap()
        )));
        assert!(sdp.contains("a=candidate:1 1 UDP 2130706431 192.168.1.50 20000 typ host"));
        assert!(sdp.contains("a=candidate:1 1 UDP 2130706431 192.168.1.50 20002 typ host"));
        // Our own SRTP keys, distinct per media.
        assert!(sdp.contains(&answer.audio_crypto_line));
        assert!(sdp.contains(answer.video_crypto_line.as_ref().unwrap()));
        assert_ne!(
            answer.audio_crypto_line,
            answer.video_crypto_line.clone().unwrap()
        );
    }

    #[test]
    fn audio_only_offer_generates_an_audio_only_answer() {
        let audio_only = "v=0\r\n\
             o=- 0 0 IN IP4 10.0.0.1\r\n\
             m=audio 30000 RTP/SAVP 0\r\n\
             a=ice-ufrag:abcd\r\n\
             a=ice-pwd:password123456789012\r\n\
             a=candidate:1 1 UDP 2130706431 10.0.0.1 30000 typ host\r\n\
             a=crypto:2 AES_CM_128_HMAC_SHA1_80 inline:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA|2^31\r\n";
        let parsed = sdp::parse_sdp_offer(audio_only).expect("parse");
        assert!(parsed.video.is_none());
        let answer = sdp::generate_sdp_answer_full("127.0.0.1", 20000, 20002, &parsed, &[], &[]);
        assert!(answer.sdp.contains("m=audio"));
        assert!(!answer.sdp.contains("m=video"));
        assert!(answer.video_crypto_line.is_none());
    }

    // ----- request shapes against a local stub ----------------------------

    fn incoming_notification(base: &str) -> CallNotification {
        CallNotification {
            call_invitation: Some(CallInvitation {
                call_modalities: Some(vec!["Audio".into(), "Video".into()]),
                links: Some(CallLinks {
                    acceptance: Some(format!("{base}/acceptance")),
                    end: Some(format!("{base}/end")),
                    media_answer: Some(format!("{base}/mediaAnswer")),
                    p2p_fork_notification: None,
                }),
                media_content: Some(MediaContent {
                    blob: Some(offer_fixture()),
                    content_type: Some("application/sdp".into()),
                }),
            }),
            participants: None,
            conversation_request: None,
            debug_content: None,
        }
    }

    #[tokio::test]
    async fn place_call_creates_a_conversation_then_joins_with_sdp() {
        let (base, seen, task) = http_stub_with(|base| {
            vec![
                // Phase 1: the conversation controller.
                StubResponse::ok(format!(r#"{{"conversationController":"{base}/conv/abc"}}"#)),
                // Phase 2: an empty 200 with the CC-active header.
                StubResponse::ok("{}"),
            ]
        })
        .await;

        let signaller = CallSignaller::with_http(
            reqwest::Client::new(),
            CallEndpoints::new(format!("{base}/api/v2/epconv"), format!("{base}/surl/")),
        );
        let id = identity();
        let placed = signaller
            .place_call(
                &PlaceCallRequest::new("ic3-token", &id, "19:alice_bob@unq.gbl.spaces")
                    .with_video(true)
                    .with_local_ip("10.0.0.5"),
            )
            .await
            .expect("place call");

        assert_eq!(placed.conversation_controller, format!("{base}/conv/abc"));
        assert!(placed.video);
        assert!(placed.sdp_offer.contains("m=video"));
        assert!(placed.video_crypto_line.is_some());
        assert_eq!(placed.thread_id, "19:alice_bob@unq.gbl.spaces");

        task.await.expect("stub drains");
        let requests = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(requests.len(), 2, "phase 1 + phase 2");

        let phase1 = &requests[0];
        assert!(phase1.starts_with("POST /api/v2/epconv "), "{phase1}");
        assert!(
            phase1
                .to_ascii_lowercase()
                .contains("authorization: bearer ic3-token"),
            "{phase1}"
        );
        assert!(
            phase1.contains("x-microsoft-skype-client: SkypeSpaces/"),
            "{phase1}"
        );
        assert!(phase1.contains("ms-teams-partition: amer03"), "{phase1}");
        assert!(
            phase1.contains("\"threadId\":\"19:alice_bob@unq.gbl.spaces\""),
            "{phase1}"
        );
        assert!(phase1.contains("\"id\":\"8:orgid:aaaa-bbbb\""), "{phase1}");

        let phase2 = &requests[1];
        assert!(phase2.starts_with("POST /conv/abc "), "{phase2}");
        // The join carries our SDP offer and the video modality.
        assert!(phase2.contains("application/sdp"), "{phase2}");
        assert!(phase2.contains("m=video"), "{phase2}");
        assert!(
            phase2.contains("\"callModalities\":[\"Audio\",\"Video\"]"),
            "{phase2}"
        );
    }

    #[tokio::test]
    async fn place_call_invites_a_callee_for_a_one_to_one_call() {
        let (base, seen, task) = http_stub_with(|base| {
            vec![
                StubResponse::ok(format!(r#"{{"conversationController":"{base}/conv/abc"}}"#)),
                StubResponse::ok("{}"),
                StubResponse::ok("{}"),
            ]
        })
        .await;

        let signaller = CallSignaller::with_http(
            reqwest::Client::new(),
            CallEndpoints::new(format!("{base}/epconv"), String::new()),
        );
        let id = identity();
        signaller
            .place_call(
                &PlaceCallRequest::new("ic3", &id, "19:a_b@unq.gbl.spaces")
                    .with_callee("8:orgid:callee"),
            )
            .await
            .expect("place call");

        task.await.expect("stub drains");
        let requests = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(requests.len(), 3, "create + join + invite");
        let invite = &requests[2];
        assert!(invite.starts_with("POST /conv/abc/add "), "{invite}");
        assert!(invite.contains("\"id\":\"8:orgid:callee\""), "{invite}");
        assert!(
            invite.contains("\"callDirection\":\"Outgoing\""),
            "{invite}"
        );
    }

    #[tokio::test]
    async fn accept_incoming_posts_acceptance_then_media_answer() {
        let (base, seen, task) =
            http_stub(vec![StubResponse::ok("{}"), StubResponse::ok("{}")]).await;

        let signaller = CallSignaller::with_http(
            reqwest::Client::new(),
            CallEndpoints::new(format!("{base}/epconv"), String::new()),
        );
        let notification = incoming_notification(&base);
        let accepted = signaller
            .accept_incoming(&notification, "skype-token", "127.0.0.1", true)
            .await
            .expect("accept");

        assert!(accepted.sdp_answer.contains("m=audio"));
        assert!(accepted.sdp_answer.contains("m=video"), "offer had video");
        assert!(accepted.video_crypto_line.is_some());

        task.await.expect("stub drains");
        let requests = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(requests.len(), 2);

        let acceptance = &requests[0];
        assert!(acceptance.starts_with("POST /acceptance "), "{acceptance}");
        assert!(
            acceptance
                .to_ascii_lowercase()
                .contains("x-skypetoken: skype-token"),
            "{acceptance}"
        );
        assert!(
            acceptance.contains("\"acceptedCallModalities\":[\"Audio\",\"Video\"]"),
            "{acceptance}"
        );

        let media = &requests[1];
        assert!(media.starts_with("POST /mediaAnswer "), "{media}");
        assert!(
            media.contains("\"contentType\":\"application/sdp\""),
            "{media}"
        );
        assert!(
            media.contains("a=crypto:2 AES_CM_128_HMAC_SHA1_80"),
            "{media}"
        );
    }

    #[tokio::test]
    async fn end_call_posts_to_the_end_link() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok("{}")]).await;
        let signaller =
            CallSignaller::new(CallEndpoints::new(format!("{base}/epconv"), String::new()));
        let notification = incoming_notification(&base);
        signaller
            .end_call(&notification, "skype-token")
            .await
            .expect("end");

        task.await.expect("stub drains");
        let requests = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST /end "), "{}", requests[0]);
    }

    #[tokio::test]
    async fn accept_without_an_sdp_offer_fails_before_any_request() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok("{}")]).await;
        let signaller =
            CallSignaller::new(CallEndpoints::new(format!("{base}/epconv"), String::new()));
        let mut notification = incoming_notification(&base);
        notification.call_invitation.as_mut().unwrap().media_content = None;

        let err = signaller
            .accept_incoming(&notification, "skype-token", "127.0.0.1", false)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("no SDP offer"), "{err}");

        // The stub never saw a request; drop the task.
        task.abort();
        assert!(seen.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    }

    #[test]
    fn signaller_debug_shows_endpoints_only() {
        let signaller = CallSignaller::new(CallEndpoints::new("https://e/epconv", "/surl/"));
        let dbg = format!("{signaller:?}");
        assert!(dbg.contains("CallSignaller"));
        assert!(dbg.contains("https://e/epconv"));
    }
}
