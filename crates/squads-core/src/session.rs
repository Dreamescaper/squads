//! Session lifecycle: the state machine the UI observes.
//!
//! M0 laid down the *shape* of a session; **M1** adds authentication:
//! [`Session::restore`], [`Session::begin_sign_in`], [`Session::poll_sign_in`]
//! and [`Session::sign_out`]. Protocol clients (M2), the store (M3) and the
//! realtime feed (M4) attach to this type in later milestones.
//!
//! Network access only ever happens through the sign-in driver; with a
//! [`MemoryTokenStore`] and no `begin_sign_in`, a `Session` is fully usable
//! offline (as the tests demonstrate).

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};

use crate::auth::flow::{AuthDriver, AuthTransport, HttpTransport, SignInProgress};
use crate::auth::store::{DEFAULT_PROFILE, FileTokenStore, TokenStore};
use crate::auth::tokens::TokenSet;
use crate::error::{CoreError, Result};
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

/// A sign-in running on the tokio runtime, observed by polling its channel.
struct SignInTask {
    rx: Receiver<SignInProgress>,
    handle: tokio::task::JoinHandle<()>,
}

/// A signed-in (or sign-in-in-progress) client session.
///
/// M1 makes the session the owner of authentication: it resolves the account
/// profile, persists tokens through a [`TokenStore`] and drives sign-in on a
/// background task, exposing progress through [`Session::poll_sign_in`].
pub struct Session {
    dirs: Arc<AppDirs>,
    state: SessionState,
    profile: String,
    store: Arc<dyn TokenStore>,
    tokens: Option<TokenSet>,
    sign_in: Option<SignInTask>,
}

impl Session {
    /// Create a session rooted at `dirs`, using the default profile and the
    /// default [`FileTokenStore`]. Does not touch the network; the initial state
    /// is [`SessionState::SignedOut`] until [`Session::restore`] or sign-in.
    pub fn new(dirs: Arc<AppDirs>) -> Result<Self> {
        let store = Arc::new(FileTokenStore::new(&dirs));
        Self::with_store(dirs, DEFAULT_PROFILE, store)
    }

    /// Create a session with an explicit profile and token store (tests,
    /// multi-account).
    pub fn with_store(
        dirs: Arc<AppDirs>,
        profile: impl Into<String>,
        store: Arc<dyn TokenStore>,
    ) -> Result<Self> {
        dirs.ensure()?;
        Ok(Self {
            dirs,
            state: SessionState::SignedOut,
            profile: profile.into(),
            store,
            tokens: None,
            sign_in: None,
        })
    }

    /// Current session state.
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Transition to a new state.
    pub fn set_state(&mut self, state: SessionState) {
        tracing::debug!(from = ?self.state, to = ?state, "session state change");
        self.state = state;
    }

    /// Directories backing this session.
    pub fn dirs(&self) -> &AppDirs {
        &self.dirs
    }

    /// The account profile this session is keyed by (`"default"` unless
    /// overridden via [`Session::with_store`]).
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The token store backing this session.
    pub fn store(&self) -> &Arc<dyn TokenStore> {
        &self.store
    }

    /// The current token set, if any has been loaded or minted.
    pub fn tokens(&self) -> Option<&TokenSet> {
        self.tokens.as_ref()
    }

    /// Load any stored token set for this profile. On success the state becomes
    /// [`SessionState::Loading`] when credentials were found, otherwise
    /// [`SessionState::SignedOut`].
    pub fn restore(&mut self) -> Result<()> {
        match self.store.load(&self.profile)? {
            Some(tokens) => {
                tracing::info!(profile = %self.profile, "restored stored credentials");
                self.tokens = Some(tokens);
                self.state = SessionState::Loading;
            }
            None => {
                self.tokens = None;
                self.state = SessionState::SignedOut;
            }
        }
        Ok(())
    }

    /// Mark a restored/loaded session as fully ready (called by M2 once initial
    /// data has loaded).
    pub fn mark_ready(&mut self) {
        if self.tokens.is_some() {
            self.state = SessionState::Ready;
        }
    }

    /// Begin a device-code sign-in against the real AAD endpoints.
    ///
    /// Requires a tokio runtime (the flow runs on a background task) and
    /// network access; progress is observed via [`Session::poll_sign_in`].
    pub fn begin_sign_in(&mut self, use_personal: bool) -> Result<()> {
        let driver = AuthDriver::new(HttpTransport::new());
        self.begin_sign_in_with_driver(use_personal, driver)
    }

    /// Begin sign-in with an explicit driver (used by tests with a fake
    /// transport).
    pub fn begin_sign_in_with_driver<T>(
        &mut self,
        use_personal: bool,
        driver: AuthDriver<T>,
    ) -> Result<()>
    where
        T: AuthTransport + 'static,
    {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| CoreError::NotReady("begin_sign_in requires an async runtime".into()))?;

        // Cancel any previous attempt.
        if let Some(previous) = self.sign_in.take() {
            previous.handle.abort();
        }

        let (tx, rx) = mpsc::channel::<SignInProgress>();
        let store = Arc::clone(&self.store);
        let profile = self.profile.clone();
        let tx_progress = tx.clone();

        self.state = SessionState::SigningIn {
            hint: "Requesting device code…".into(),
        };

        let handle = runtime.spawn(async move {
            let progress = move |p: SignInProgress| {
                let _ = tx_progress.send(p);
            };
            if let Err(e) = driver
                .sign_in(use_personal, &profile, store.as_ref(), progress)
                .await
            {
                // Report the failure through the same channel the UI polls.
                let _ = tx.send(SignInProgress::Failed(e.to_string()));
            }
        });

        self.sign_in = Some(SignInTask { rx, handle });
        Ok(())
    }

    /// Drain and apply any sign-in progress; returns the most recent event.
    ///
    /// Non-terminal events move the state to [`SessionState::SigningIn`]; a
    /// completed flow moves it to [`SessionState::Ready`] and stores the token
    /// set; a failure moves it to [`SessionState::Failed`].
    pub fn poll_sign_in(&mut self) -> Option<SignInProgress> {
        let mut drained = Vec::new();
        if let Some(task) = self.sign_in.as_ref() {
            while let Ok(progress) = task.rx.try_recv() {
                drained.push(progress);
            }
        }
        let last = drained.last().cloned();
        for progress in drained {
            self.apply_progress(&progress);
        }
        last
    }

    fn apply_progress(&mut self, progress: &SignInProgress) {
        match progress {
            SignInProgress::Completed(tokens) => {
                self.tokens = Some(tokens.clone());
                self.state = SessionState::Ready;
                self.sign_in = None;
            }
            SignInProgress::Failed(message) => {
                self.state = SessionState::Failed(message.clone());
                self.sign_in = None;
            }
            SignInProgress::AwaitingUser { user_code, .. } => {
                self.state = SessionState::SigningIn {
                    hint: user_code.clone(),
                };
            }
            other => {
                self.state = SessionState::SigningIn { hint: other.hint() };
            }
        }
    }

    /// Sign out: cancel any running sign-in, drop in-memory tokens, delete the
    /// stored set for this profile, and return to [`SessionState::SignedOut`].
    pub fn sign_out(&mut self) -> Result<()> {
        if let Some(task) = self.sign_in.take() {
            task.handle.abort();
        }
        self.tokens = None;
        self.store.delete(&self.profile)?;
        self.state = SessionState::SignedOut;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::flow::{GrantedToken, SkypeExchange};
    use crate::auth::store::MemoryTokenStore;
    use crate::auth::tokens::{StoredToken, TokenKind};
    use crate::auth::{DeviceCodeStart, OAuthParams, PollResult, TokenResponse};
    use std::future::Future;
    use std::path::PathBuf;

    fn dirs() -> (PathBuf, Arc<AppDirs>) {
        let tmp = std::env::temp_dir().join(format!("squads-session-{}", uuid::Uuid::new_v4()));
        (tmp.clone(), Arc::new(AppDirs::rooted_at(tmp)))
    }

    fn sample_set() -> TokenSet {
        let t = StoredToken::with_expiry("tok", 4_000_000_000);
        TokenSet {
            refresh_token: "refresh".into(),
            tenant: "common".into(),
            region_gtms: serde_json::json!({"r": 1}),
            personal: false,
            spaces: t.clone(),
            skype: t.clone(),
            graph: t.clone(),
            ic3: t.clone(),
            recorder: t,
        }
    }

    #[test]
    fn new_session_is_signed_out_and_creates_dirs() {
        let (tmp, dirs) = dirs();
        let mut session = Session::new(dirs).expect("session");
        assert_eq!(session.state(), &SessionState::SignedOut);
        assert_eq!(session.profile(), DEFAULT_PROFILE);
        session.set_state(SessionState::Loading);
        assert_eq!(session.state().label(), "Loading…");
        assert!(!session.state().is_ready());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn restore_without_tokens_stays_signed_out() {
        let (tmp, dirs) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        let mut session = Session::with_store(dirs, "default", store).expect("session");
        session.restore().expect("restore");
        assert_eq!(session.state(), &SessionState::SignedOut);
        assert!(session.tokens().is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn restore_with_tokens_enters_loading() {
        let (tmp, dirs) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        store.save("work", &sample_set()).unwrap();
        let mut session = Session::with_store(dirs, "work", store).expect("session");
        session.restore().expect("restore");
        assert_eq!(session.state(), &SessionState::Loading);
        assert_eq!(session.tokens().unwrap().tenant(), "common");
        session.mark_ready();
        assert!(session.state().is_ready());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn sign_out_deletes_and_resets() {
        let (tmp, dirs) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        store.save("default", &sample_set()).unwrap();
        let mut session = Session::with_store(dirs, "default", store.clone()).expect("session");
        session.restore().expect("restore");
        assert!(store.contains("default"));

        session.sign_out().expect("sign out");
        assert_eq!(session.state(), &SessionState::SignedOut);
        assert!(session.tokens().is_none());
        assert!(!store.contains("default"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn begin_sign_in_without_runtime_errors() {
        let (tmp, dirs) = dirs();
        let mut session = Session::new(dirs).expect("session");
        // No tokio runtime in this plain test: must fail cleanly, no network.
        let err = session.begin_sign_in(false).unwrap_err();
        assert!(format!("{err}").contains("async runtime"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Minimal offline transport for the session test.
    struct StubTransport;

    impl AuthTransport for StubTransport {
        fn request_device_code(
            &self,
            _p: &OAuthParams,
        ) -> impl Future<Output = Result<DeviceCodeStart>> + Send {
            async {
                Ok(DeviceCodeStart {
                    device_code: "dc".into(),
                    user_code: "STUB-CODE".into(),
                    verification_uri: "https://v".into(),
                    expires_in: 900,
                    interval: 0,
                    message: String::new(),
                })
            }
        }
        fn poll_device_code(
            &self,
            _p: &OAuthParams,
            _d: &str,
        ) -> impl Future<Output = Result<PollResult>> + Send {
            async {
                Ok(PollResult::Token(TokenResponse {
                    access_token: "at".into(),
                    refresh_token: Some("rt".into()),
                    expires_in: Some(3600),
                }))
            }
        }
        fn exchange_skype(
            &self,
            _a: &str,
            _p: bool,
        ) -> impl Future<Output = Result<SkypeExchange>> + Send {
            async {
                Ok(SkypeExchange {
                    token: "skype".into(),
                    expires_in: Some(3600),
                    region_gtms: Some(serde_json::json!({"region": "emea"})),
                })
            }
        }
        fn refresh_grant(
            &self,
            _p: &OAuthParams,
            _r: &str,
            kind: TokenKind,
        ) -> impl Future<Output = Result<GrantedToken>> + Send {
            async move {
                Ok(GrantedToken {
                    token: format!("t-{}", kind.as_str()),
                    refresh_token: Some("rt".into()),
                    expires_in: Some(3600),
                })
            }
        }
    }

    #[tokio::test]
    async fn full_sign_in_via_session_reaches_ready() {
        let (tmp, dirs) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        let mut session = Session::with_store(dirs, "default", store.clone()).expect("session");

        let driver = AuthDriver::new(StubTransport);
        session
            .begin_sign_in_with_driver(false, driver)
            .expect("begin");
        assert!(matches!(session.state(), SessionState::SigningIn { .. }));

        // Let the background task run to completion.
        for _ in 0..10_000 {
            session.poll_sign_in();
            if session.state().is_ready() {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert!(session.state().is_ready(), "state = {:?}", session.state());
        let tokens = session.tokens().expect("tokens");
        assert_eq!(tokens.tenant(), "common");
        assert_eq!(tokens.skype_token(), "skype");
        assert_eq!(tokens.graph.token(), "t-graph");
        assert_eq!(tokens.ic3.token(), "t-ic3");
        assert_eq!(tokens.recorder.token(), "t-recorder");
        assert!(store.contains("default"));

        std::fs::remove_dir_all(&tmp).ok();
    }
}
