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
pub mod messaging;
pub mod model;
pub mod paths;
pub mod realtime;
pub mod session;
pub mod settings;
pub mod store;
pub mod text;

pub use api::{ApiClient, tokens_to_config};
pub use auth::{AuthDriver, AuthTransport, SignInProgress, TokenKind, TokenSet};
pub use error::{CoreError, Result};
pub use messaging::{HistoryPage, Messaging};
pub use model::{Chat, Message, Reaction, Reactor, SendState};
pub use paths::AppDirs;
pub use realtime::{EventBus, RealtimeEvent, RealtimeService};
pub use session::{Session, SessionState};
pub use settings::{Settings, Theme, WindowSettings};
pub use store::{Account, Counts, Store};
pub use text::{html_escape, strip_html};
