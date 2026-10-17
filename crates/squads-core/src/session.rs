//! Session lifecycle: the state machine the UI observes.
//!
//! Milestone M0 lays down only the *shape* of a session: where it comes from,
//! what states it can be in, and how it resolves directories. The actual
//! authentication (M1), protocol clients (M2), store (M3) and realtime feed (M4)
//! attach to this type in later milestones.

use std::sync::Arc;

use crate::error::Result;
use crate::paths::AppDirs;

/// High-level state of the local client, independent of rendering.
///
/// The UI mirrors this to decide which screen to show (login vs. shell).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// No credentials stored; the user must sign in.
    SignedOut,
    /// A sign-in flow is in progress (device-code or PKCE browser).
    SigningIn {
        /// Human-readable hint, e.g. the device-code user code.
        hint: String,
    },
    /// Credentials exist and the client is loading initial data.
    Loading,
    /// Signed in and ready to serve chats, teams and presence.
    Ready,
    /// A previous sign-in failed and needs user attention.
    Failed(String),
}

impl SessionState {
    /// Whether the session can serve authenticated requests right now.
    pub fn is_ready(&self) -> bool {
        matches!(self, SessionState::Ready)
    }

    /// A short label suitable for the window title / status bar.
    pub fn label(&self) -> &str {
        match self {
            SessionState::SignedOut => "Signed out",
            SessionState::SigningIn { .. } => "Signing in…",
            SessionState::Loading => "Loading…",
            SessionState::Ready => "Ready",
            SessionState::Failed(_) => "Error",
        }
    }
}

/// A signed-in (or sign-in-in-progress) client session.
///
/// For now this only carries configuration; later milestones add the protocol
/// client, realtime connection, event bus and store handles.
pub struct Session {
    dirs: Arc<AppDirs>,
    state: SessionState,
}

impl Session {
    /// Create a session rooted at the given directories without touching the
    /// network. The initial state is always [`SessionState::SignedOut`]; M1
    /// will upgrade this to `Loading`/`Ready` when a stored token is found.
    pub fn new(dirs: Arc<AppDirs>) -> Result<Self> {
        dirs.ensure()?;
        Ok(Self {
            dirs,
            state: SessionState::SignedOut,
        })
    }

    /// Current session state.
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Transition to a new state. (Deliberately unrestricted for now; M1 will
    /// replace this with explicit transition methods.)
    pub fn set_state(&mut self, state: SessionState) {
        tracing::debug!(from = ?self.state, to = ?state, "session state change");
        self.state = state;
    }

    /// Directories backing this session.
    pub fn dirs(&self) -> &AppDirs {
        &self.dirs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_is_signed_out_and_creates_dirs() {
        let tmp = std::env::temp_dir().join(format!("squads-session-{}", uuid::Uuid::new_v4()));
        let dirs = Arc::new(AppDirs::rooted_at(&tmp));
        let mut session = Session::new(dirs).expect("session");
        assert_eq!(session.state(), &SessionState::SignedOut);
        session.set_state(SessionState::Loading);
        assert_eq!(session.state().label(), "Loading…");
        assert!(!session.state().is_ready());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
