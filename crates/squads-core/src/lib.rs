//! Squads core: the non-UI half of the client.
//!
//! This crate owns everything the GPUI application needs that is not rendering:
//! configuration paths, secure token storage, the Teams session/client lifecycle,
//! the realtime event bus, and the local (offline-first) store.
//!
//! The heavy protocol lifting lives in the vendored [`ost`] crate
//! (`squads-protocol`); this crate wraps it into a stable, testable API for the UI.

pub mod api;
pub mod auth;
pub mod error;
pub mod files;
pub mod messaging;
pub mod model;
pub mod notifications;
pub mod paths;
pub mod presence;
pub mod realtime;
pub mod session;
pub mod settings;
pub mod store;
pub mod teams;
pub mod text;

pub use api::{ApiClient, tokens_to_config};
pub use auth::{AuthDriver, AuthTransport, SignInProgress, TokenKind, TokenSet};
pub use error::{CoreError, Result};
pub use files::Files;
pub use messaging::{HistoryPage, Messaging};
pub use model::{
    Channel, Chat, FileEntry, FileVersion, Message, Reaction, Reactor, SendState, SharedLink, Team,
    TeamMember, is_channel_conversation_id,
};
pub use notifications::{
    ActivityItem, LogNotifier, Notification, NotificationContext, NotificationKind, Notifications,
    Notifier, RecordingNotifier, UnreadSummary, route,
};
pub use paths::AppDirs;
pub use presence::{
    Activity, Availability, ContactPresence, GraphBackend, OwnPresence, Presence, PresenceBackend,
    UnifiedBackend,
};
pub use realtime::{EventBus, RealtimeEvent, RealtimeService};
pub use session::{Session, SessionState};
pub use settings::{Settings, Theme, WindowSettings};
pub use store::{Account, Counts, Store};
pub use teams::Teams;
pub use text::{Mention, html_escape, mentions, strip_html};

#[cfg(feature = "desktop-notifications")]
pub use notifications::DesktopNotifier;
