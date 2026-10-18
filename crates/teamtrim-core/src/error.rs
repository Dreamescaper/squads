//! Error type for the core crate.

use std::path::PathBuf;

/// Convenience alias used across the core crate.
pub type Result<T> = std::result::Result<T, CoreError>;

/// Errors surfaced by the TeamTrim core.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// Something in the vendored protocol layer failed.
    #[error("protocol error: {0}")]
    Protocol(#[from] anyhow::Error),

    /// Filesystem / config IO failed.
    #[error("io error at {path}: {source}")]
    Io {
        /// The path involved in the failing operation.
        path: PathBuf,
        /// The underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// JSON (de)serialization failed.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// The requested account is not signed in.
    #[error("not signed in")]
    NotSignedIn,

    /// An authentication/OAuth failure. Carries AAD error codes and short
    /// descriptions only — never token values.
    #[error("auth error: {0}")]
    Auth(String),

    /// The session is not ready to serve the request yet.
    #[error("session not ready: {0}")]
    NotReady(String),

    /// The local SQLite store failed (open, migrate or query).
    ///
    /// Carries a short message only: `rusqlite`'s own error text (SQL + schema
    /// detail, never row *values*), and never chat/message content.
    #[error("store error: {0}")]
    Store(String),

    /// Inline media fetch/cache failed (M13).
    ///
    /// Carries a short message (URL host + transport/status detail), never the
    /// token.
    #[error("media error: {0}")]
    Media(String),

    /// The on-disk database has a schema version this build cannot handle.
    #[error(
        "store schema version {found} is not supported by this build (supports up to {supported})"
    )]
    StoreSchemaVersion {
        /// The `PRAGMA user_version` found in the database file.
        found: i32,
        /// The newest schema version this build knows how to migrate to.
        supported: i32,
    },
}

impl CoreError {
    /// Build an [`CoreError::Io`] tagged with the path involved.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        CoreError::Io {
            path: path.into(),
            source,
        }
    }
}

impl From<rusqlite::Error> for CoreError {
    fn from(e: rusqlite::Error) -> Self {
        // rusqlite error text carries SQL/schema detail, never parameter
        // values, so it is safe (and useful) to keep the message.
        CoreError::Store(e.to_string())
    }
}
