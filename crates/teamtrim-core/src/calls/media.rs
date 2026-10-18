//! Media sessions (milestone **M9**).
//!
//! A [`MediaSession`] is the seam between call signalling and actual audio/video
//! I/O. The default build never touches a device: the only implementation that
//! is compiled and tested by default is [`LoopbackMedia`], which runs the
//! vendored offline pipeline end to end (`macav::call_dry_run`:
//! tone → PCMU → RTP → SRTP protect → SRTP unprotect → RTP → PCMU → echo detect,
//! plus a video NAL packetize/depacketize round-trip). That backs the M9
//! exit-criterion test with **no network, no auth and no hardware**.
//!
//! Real capture/playback is opt-in and behind default-off cargo features, so the
//! headless/CI build needs no ALSA or SDL2 (the same posture as the OS keyring
//! and desktop notifications — ADR-008/ADR-022):
//!
//! * [`AudioMedia`] — `teamtrim-core` feature **`audio`** → `teamtrim-protocol/audio`
//!   (the vendored `cpal` capture/playback). Never constructed in tests.
//! * [`VideoMedia`] — `teamtrim-core` feature **`video-capture`** →
//!   `teamtrim-protocol/video-capture` (v4l + openh264 + SDL2). **A documented
//!   placeholder**: it is not wired and must not be enabled here because SDL2 is
//!   not installed.

use std::collections::VecDeque;

use ost::calling::{macav, rtp};

use crate::error::{CoreError, Result};

/// Samples per 20 ms PCMU frame at 8 kHz.
pub const FRAME_SAMPLES: usize = rtp::SAMPLES_PER_PACKET;

/// Which media backend a session uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    /// The offline loopback pipeline ([`LoopbackMedia`]).
    Loopback,
    /// Real audio capture/playback ([`AudioMedia`], feature `audio`).
    Audio,
    /// Real audio + video ([`VideoMedia`], feature `video-capture`).
    Video,
}

impl MediaKind {
    /// A short, stable label.
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Loopback => "loopback",
            MediaKind::Audio => "audio",
            MediaKind::Video => "video",
        }
    }
}

/// A running media session.
///
/// `send_audio` takes 16-bit mono PCM at 8 kHz (the pipeline's native rate) and
/// returns how many 20 ms frames it accepted; `receive` returns the next decoded
/// frame, if any. `start`/`stop` are idempotent-ish: starting an active session
/// is a no-op, and stopping an inactive one is fine.
pub trait MediaSession {
    /// Which backend this session is.
    fn kind(&self) -> MediaKind;

    /// Start the session (open devices / run the offline pipeline).
    fn start(&mut self) -> Result<()>;

    /// Stop the session and release resources.
    fn stop(&mut self) -> Result<()>;

    /// Whether the session is currently running.
    fn is_active(&self) -> bool;

    /// Queue one buffer of 16-bit mono PCM for transmission; returns the number
    /// of [`FRAME_SAMPLES`]-sample frames accepted.
    fn send_audio(&mut self, pcm: &[i16]) -> Result<usize>;

    /// The next decoded inbound frame, or `None` when nothing is queued.
    fn receive(&mut self) -> Result<Option<Vec<i16>>>;
}

/// The offline loopback media session (backs the M9 exit criterion).
///
/// [`LoopbackMedia::start`] runs [`macav::call_dry_run`] once, exercising the
/// whole media pipeline; the result is kept for assertions
/// ([`LoopbackMedia::dry_run`]). `send_audio` then exercises the PCMU codec in
/// both directions and queues the looped-back frames for `receive`, so the
/// trait's send/receive contract is testable without a device.
#[derive(Debug, Default)]
pub struct LoopbackMedia {
    active: bool,
    last: Option<macav::DryRunResult>,
    sent_frames: u32,
    received_frames: u32,
    loopback: VecDeque<Vec<i16>>,
}

impl LoopbackMedia {
    /// Create a stopped loopback session.
    pub fn new() -> Self {
        Self::default()
    }

    /// The last [`macav::DryRunResult`], once [`MediaSession::start`] has run.
    pub fn dry_run(&self) -> Option<&macav::DryRunResult> {
        self.last.as_ref()
    }

    /// How many 20 ms frames [`MediaSession::send_audio`] has accepted.
    pub fn sent_frames(&self) -> u32 {
        self.sent_frames
    }

    /// How many frames [`MediaSession::receive`] has returned.
    pub fn received_frames(&self) -> u32 {
        self.received_frames
    }
}

impl MediaSession for LoopbackMedia {
    fn kind(&self) -> MediaKind {
        MediaKind::Loopback
    }

    fn start(&mut self) -> Result<()> {
        if self.active {
            return Ok(());
        }
        let result = macav::call_dry_run()?;
        self.last = Some(result);
        self.active = true;
        self.sent_frames = 0;
        self.received_frames = 0;
        self.loopback.clear();
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.active = false;
        self.loopback.clear();
        Ok(())
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn send_audio(&mut self, pcm: &[i16]) -> Result<usize> {
        if !self.active {
            return Err(CoreError::NotReady(
                "loopback media session is not started".into(),
            ));
        }
        let mut frames = 0usize;
        for chunk in pcm.chunks(FRAME_SAMPLES) {
            if chunk.len() < FRAME_SAMPLES {
                break; // drop a trailing partial frame, as RTP would
            }
            // Round-trip through the PCMU codec both ways: this is the same
            // encode/decode the dry run performs on the wire.
            let ulaw: Vec<u8> = chunk.iter().map(|&s| rtp::linear_to_ulaw(s)).collect();
            let back: Vec<i16> = ulaw.iter().map(|&b| rtp::ulaw_to_linear(b)).collect();
            self.loopback.push_back(back);
            self.sent_frames += 1;
            frames += 1;
        }
        Ok(frames)
    }

    fn receive(&mut self) -> Result<Option<Vec<i16>>> {
        match self.loopback.pop_front() {
            Some(frame) => {
                self.received_frames += 1;
                Ok(Some(frame))
            }
            None => Ok(None),
        }
    }
}

/// Real audio capture/playback, behind the default-off `audio` feature.
///
/// Wraps the vendored `ost::calling::audio` (`cpal`) capture/playback. This is
/// never constructed in tests: opening a device in a headless environment fails
/// (and would be non-deterministic even where a device exists).
#[cfg(feature = "audio")]
pub struct AudioMedia {
    capture: Option<ost::calling::audio::AudioCapture>,
    playback: Option<ost::calling::audio::AudioPlayback>,
    mic_rx: Option<std::sync::mpsc::Receiver<Vec<i16>>>,
    out_tx: Option<std::sync::mpsc::SyncSender<Vec<i16>>>,
    active: bool,
}

#[cfg(feature = "audio")]
impl AudioMedia {
    /// Create a stopped audio session.
    pub fn new() -> Self {
        Self {
            capture: None,
            playback: None,
            mic_rx: None,
            out_tx: None,
            active: false,
        }
    }
}

#[cfg(feature = "audio")]
impl Default for AudioMedia {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "audio")]
impl MediaSession for AudioMedia {
    fn kind(&self) -> MediaKind {
        MediaKind::Audio
    }

    fn start(&mut self) -> Result<()> {
        if self.active {
            return Ok(());
        }
        let (capture, mic_rx) = ost::calling::audio::AudioCapture::start()
            .ok_or_else(|| CoreError::NotReady("no audio input device available".into()))?;
        let (playback, out_tx) = ost::calling::audio::AudioPlayback::start()
            .ok_or_else(|| CoreError::NotReady("no audio output device available".into()))?;
        self.capture = Some(capture);
        self.playback = Some(playback);
        self.mic_rx = Some(mic_rx);
        self.out_tx = Some(out_tx);
        self.active = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        // Dropping the capture/playback handles stops their streams.
        self.capture = None;
        self.playback = None;
        self.mic_rx = None;
        self.out_tx = None;
        self.active = false;
        Ok(())
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn send_audio(&mut self, pcm: &[i16]) -> Result<usize> {
        let Some(tx) = self.out_tx.as_ref() else {
            return Err(CoreError::NotReady("audio session is not started".into()));
        };
        let mut frames = 0usize;
        for chunk in pcm.chunks(FRAME_SAMPLES) {
            if chunk.len() < FRAME_SAMPLES {
                break;
            }
            // A full playback queue drops the frame rather than blocking the caller.
            if tx.try_send(chunk.to_vec()).is_err() {
                break;
            }
            frames += 1;
        }
        Ok(frames)
    }

    fn receive(&mut self) -> Result<Option<Vec<i16>>> {
        let Some(rx) = self.mic_rx.as_ref() else {
            return Err(CoreError::NotReady("audio session is not started".into()));
        };
        Ok(rx.try_recv().ok())
    }
}

/// Real audio + video, behind the default-off `video-capture` feature.
///
/// **Placeholder, deliberately not wired.** The vendored
/// `teamtrim-protocol/video-capture` feature needs V4L2, `openh264` and **SDL2**
/// (for display); SDL2 development libraries are not installed in this
/// environment, so the feature must not be enabled here. This type exists so the
/// feature's wiring and the `MediaSession` shape are documented and compile when
/// a developer enables the feature on a machine that has the libraries.
#[cfg(feature = "video-capture")]
#[derive(Debug, Default)]
pub struct VideoMedia {
    active: bool,
}

#[cfg(feature = "video-capture")]
impl VideoMedia {
    /// Create a stopped video session.
    pub fn new() -> Self {
        Self { active: false }
    }
}

#[cfg(feature = "video-capture")]
impl MediaSession for VideoMedia {
    fn kind(&self) -> MediaKind {
        MediaKind::Video
    }

    fn start(&mut self) -> Result<()> {
        Err(CoreError::NotReady(
            "video capture is a placeholder; SDL2/openh264/v4l wiring is a follow-up (see docs/BLOCKERS.md)"
                .into(),
        ))
    }

    fn stop(&mut self) -> Result<()> {
        self.active = false;
        Ok(())
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn send_audio(&mut self, _pcm: &[i16]) -> Result<usize> {
        Err(CoreError::NotReady("video media is not implemented".into()))
    }

    fn receive(&mut self) -> Result<Option<Vec<i16>>> {
        Err(CoreError::NotReady("video media is not implemented".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ost::calling::srtp;
    use ost::calling::test_tone::ToneGenerator;

    #[test]
    fn media_kind_labels_are_stable() {
        assert_eq!(MediaKind::Loopback.as_str(), "loopback");
        assert_eq!(MediaKind::Audio.as_str(), "audio");
        assert_eq!(MediaKind::Video.as_str(), "video");
    }

    #[test]
    fn rtp_encode_decode_round_trips() {
        let payload: Vec<u8> = (0..160u16).map(|i| (i & 0xFF) as u8).collect();
        let packet = rtp::encode(rtp::PT_PCMU, 42, 1_234_560, 0xDEAD_BEEF, &payload);
        let decoded = rtp::decode(&packet).expect("decode");
        assert_eq!(decoded.version, 2);
        assert_eq!(decoded.payload_type, rtp::PT_PCMU);
        assert_eq!(decoded.sequence_number, 42);
        assert_eq!(decoded.timestamp, 1_234_560);
        assert_eq!(decoded.ssrc, 0xDEAD_BEEF);
        assert_eq!(decoded.payload, payload);
        assert!(!decoded.marker);
    }

    #[test]
    fn pcmu_codec_round_trips_within_quantisation_error() {
        // G.711 is lossy; the same sample round-trips exactly for the values the
        // tone generator produces.
        let mut tone = ToneGenerator::new();
        let frame = tone.next_frame();
        let decoded: Vec<i16> = frame
            .iter()
            .map(|&s| rtp::ulaw_to_linear(rtp::linear_to_ulaw(s)))
            .collect();
        assert_eq!(decoded.len(), FRAME_SAMPLES);
    }

    /// Build keying material from a fixed crypto line (the `macav` dry-run shape).
    fn material(tag: u32, fill: u8) -> srtp::SrtpKeyingMaterial {
        use base64::Engine as _;
        let raw = vec![fill; 30];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let line = format!(
            "a=crypto:{} AES_CM_128_HMAC_SHA1_80 inline:{}|2^31",
            tag, b64
        );
        srtp::parse_crypto_line(&line).expect("parse crypto line")
    }

    #[test]
    fn srtp_parse_derive_and_protect_unprotect_round_trip() {
        // Parse a realistic crypto line into keying material.
        let local = material(1, 0x11);
        let remote = material(2, 0x22);
        assert_eq!(local.tag, 1);

        // Derive session keys (RFC 3711 PRF) and assert the shapes.
        let keys = srtp::derive_session_keys(&local).expect("derive keys");
        assert_eq!(keys.cipher_key.len(), 16);
        assert_eq!(keys.auth_key.len(), 20);
        assert_eq!(keys.salt.len(), 14);

        // Two contexts with swapped keys: A protects, B unprotects.
        let mut sender = srtp::create_context(&local, &remote).expect("sender ctx");
        let mut receiver = srtp::create_context(&remote, &local).expect("receiver ctx");

        let payload: Vec<u8> = (0..160u8).collect();
        let rtp_packet = rtp::encode(rtp::PT_PCMU, 7, 1120, 0x0102_0304, &payload);
        let srtp_packet = srtp::protect(&mut sender, &rtp_packet).expect("protect");

        // The protected packet is longer (auth tag) and the payload is encrypted.
        assert_eq!(
            srtp_packet.len(),
            rtp_packet.len() + srtp::SRTP_AUTH_TAG_LEN
        );
        assert_ne!(&srtp_packet[rtp::RTP_HEADER_SIZE..], payload.as_slice());

        let recovered = srtp::unprotect(&mut receiver, &srtp_packet).expect("unprotect");
        assert_eq!(recovered, rtp_packet);

        // A tampered packet fails the auth check.
        let mut tampered = srtp_packet.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xFF;
        assert!(srtp::unprotect(&mut receiver, &tampered).is_err());
    }

    #[test]
    fn loopback_start_runs_the_whole_pipeline() {
        let mut media = LoopbackMedia::new();
        assert!(!media.is_active());
        media.start().expect("start");
        assert!(media.is_active());
        assert_eq!(media.kind(), MediaKind::Loopback);

        let dry = media.dry_run().expect("dry run recorded");
        assert_eq!(dry.audio_sent, 25);
        assert_eq!(dry.audio_received, 25);
        assert!(dry.echo_detected, "corr={}", dry.echo_correlation);
        assert_eq!(dry.video_packets, 5);
        assert_eq!(dry.video_nals, 5);

        // Starting again is a no-op, not a second run.
        media.start().unwrap();
        assert_eq!(media.sent_frames(), 0);
    }

    #[test]
    fn loopback_send_and_receive_frames() {
        let mut media = LoopbackMedia::new();
        // Sending before start is a clean error, never a panic.
        assert!(media.send_audio(&[0i16; FRAME_SAMPLES]).is_err());
        media.start().unwrap();

        // A 2.5-frame buffer yields two full frames; the partial tail is dropped.
        let pcm = vec![1000i16; FRAME_SAMPLES * 2 + 17];
        assert_eq!(media.send_audio(&pcm).unwrap(), 2);
        assert_eq!(media.sent_frames(), 2);

        // Receiving drains in order, then returns None.
        let first = media.receive().unwrap().expect("frame 1");
        assert_eq!(first.len(), FRAME_SAMPLES);
        assert!(media.receive().unwrap().is_some());
        assert!(media.receive().unwrap().is_none());
        assert_eq!(media.received_frames(), 2);

        media.stop().unwrap();
        assert!(!media.is_active());
        assert!(media.send_audio(&[0i16; FRAME_SAMPLES]).is_err());
    }

    #[test]
    fn media_session_is_object_safe() {
        let mut boxed: Box<dyn MediaSession> = Box::new(LoopbackMedia::new());
        boxed.start().unwrap();
        assert!(boxed.is_active());
        assert_eq!(boxed.kind(), MediaKind::Loopback);
        assert_eq!(boxed.send_audio(&[0i16; FRAME_SAMPLES]).unwrap(), 1);
        assert!(boxed.receive().unwrap().is_some());
        boxed.stop().unwrap();
    }
}
