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
pub mod paths;
pub mod session;

pub use api::{ApiClient, tokens_to_config};
pub use auth::{AuthDriver, AuthTransport, SignInProgress, TokenKind, TokenSet};
pub use error::{CoreError, Result};
pub use paths::AppDirs;
pub use session::{Session, SessionState};
