//! Squads core: the non-UI half of the client.
//!
//! This crate owns everything the GPUI application needs that is not rendering:
//! configuration paths, secure token storage, the Teams session/client lifecycle,
//! the realtime event bus, the local (offline-first) store, and the call layer
//! (signalling, media seam, call history).
//!
//! The heavy protocol lifting lives in the vendored [`ost`] crate
//! (`squads-protocol`); this crate wraps it into a stable, testable API for the UI.

pub mod api;
pub mod auth;
pub mod calendar;
pub mod calls;
pub mod chats;
pub mod error;
pub mod files;
pub mod folders;
pub mod media;
pub mod messaging;
pub mod model;
pub mod notifications;
pub mod paths;
pub mod people;
pub mod pins;
pub mod presence;
pub mod realtime;
pub mod recordings;
pub mod search;
pub mod session;
pub mod settings;
pub mod store;
pub mod teams;
pub mod text;

pub use api::{ApiClient, media_needs_auth, own_member_mri, tokens_to_config};
pub use auth::{AuthDriver, AuthTransport, SignInProgress, TokenKind, TokenSet};
pub use calendar::{Calendar, Lobby, lobby_step};
pub use calls::{
    AcceptedCall, CallCredentials, CallEndpoints, CallIdentity, CallMachine, CallRecorder,
    CallRecording, CallService, CallSignaller, CallState, EndReason, IncomingCall, LoopbackMedia,
    MediaKind, MediaSession, PlaceCallRequest, PlacedCall, RecordingRequest, parse_incoming,
};
pub use chats::Chats;
pub use error::{CoreError, Result};
pub use files::Files;
pub use folders::Folders;
pub use media::{CachedMedia, MediaCache};
pub use messaging::{HistoryPage, Messaging};
pub use model::{
    CallDirection, CallLogEntry, CallOutcome, Channel, Chat, ChatFolder, ChatMember, FileEntry,
    FileVersion, Identity, Meeting, Message, Person, PinnedMessage, PublicTeam, Reaction, Reactor,
    Recording, RecordingSource, SearchHit, SendState, SharedLink, Team, TeamMember, TeamSettings,
    Transcript, TranscriptSource, is_channel_conversation_id, is_system_folder,
};
pub use notifications::{
    ActivityItem, LogNotifier, Notification, NotificationContext, NotificationKind, Notifications,
    Notifier, RecordingNotifier, UnreadSummary, route,
};
pub use paths::AppDirs;
pub use people::People;
pub use pins::Pins;
pub use presence::{
    Activity, Availability, ContactPresence, GraphBackend, OwnPresence, Presence, PresenceBackend,
    UnifiedBackend,
};
pub use realtime::{EventBus, RealtimeEvent, RealtimeService};
pub use recordings::Recordings;
pub use search::{Search, escape_like, like_pattern, snippet};
pub use session::{Session, SessionState};
pub use settings::{Accent, Settings, Theme, WindowSettings};
pub use store::{Account, Counts, SearchRow, Store};
pub use teams::{Teams, split_thread_conversation};
pub use text::{Mention, html_escape, mentions, strip_html};

#[cfg(feature = "desktop-notifications")]
pub use notifications::DesktopNotifier;

#[cfg(feature = "audio")]
pub use calls::AudioMedia;
#[cfg(feature = "video-capture")]
pub use calls::VideoMedia;
