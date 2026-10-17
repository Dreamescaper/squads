//! Error type for the core crate.

use std::path::PathBuf;

/// Convenience alias used across the core crate.
pub type Result<T> = std::result::Result<T, CoreError>;

/// Errors surfaced by the Squads core.
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

    /// The session is not ready to serve the request yet.
    #[error("session not ready: {0}")]
    NotReady(String),
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
