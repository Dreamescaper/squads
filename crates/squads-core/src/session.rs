//! Session lifecycle: the state machine the UI observes.
//!
//! M0 laid down the *shape* of a session; **M1** adds authentication:
//! [`Session::restore`], [`Session::begin_sign_in`], [`Session::poll_sign_in`]
//! and [`Session::sign_out`]. **M2** attaches the protocol client
//! ([`Session::api`], built lazily from the token set) and token refresh
//! ([`Session::ensure_fresh_tokens`]). **M3** attaches the local SQLite store
//! ([`Session::store`] / [`Session::attach_store`]), opened from
//! [`AppDirs::store_file`] by [`Session::new`]. **M5/M6/M7/M8/M9** expose the
//! conversation, teams/channels, files, presence/notifications and call services
//! ([`Session::messaging`], [`Session::teams`], [`Session::files`],
//! [`Session::presence`], [`Session::notifications`], [`Session::calls`]). The
//! realtime feed attaches in M4.
//!
//! Network access only ever happens through the sign-in driver, a
//! [`Session::ensure_fresh_tokens`] refresh, or a call on the [`ApiClient`]
//! returned by [`Session::api`]; with a [`MemoryTokenStore`] and no sign-in, a
//! `Session` — and building its `ApiClient` — is fully offline (as the tests
//! demonstrate). The store is a local file and is opened offline too.
//!
//! The SQLite [`Store`] is shared as `Arc<Store>` because M4's realtime task and
//! the UI thread will both need it; see the module docs in [`crate::store`] for
//! why a `Mutex<Connection>` is acceptable at our scale.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};

use tokio::sync::broadcast;

use crate::api::ApiClient;
use crate::auth::flow::{AuthDriver, AuthTransport, HttpTransport, SignInProgress};
use crate::auth::store::{DEFAULT_PROFILE, FileTokenStore, TokenStore};
use crate::auth::tokens::{TokenSet, now_unix};
use crate::calendar::Calendar;
use crate::calls::{CallCredentials, CallEndpoints, CallIdentity, CallService, CallSignaller};
use crate::error::{CoreError, Result};
use crate::files::Files;
use crate::messaging::Messaging;
use crate::model::Identity;
use crate::notifications::{LogNotifier, Notifications, Notifier};
use crate::paths::AppDirs;
use crate::people::People;
use crate::presence::{Presence, presence_base_from_region};
use crate::realtime::{RealtimeEvent, RealtimeService};
use crate::search::Search;
use crate::store::{Account, Store};
use crate::teams::Teams;

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
    token_store: Arc<dyn TokenStore>,
    /// The local offline cache (M3). Opened at [`AppDirs::store_file`] and
    /// shared via `Arc` so the M4 realtime task can write through it.
    store: Arc<Store>,
    tokens: Option<TokenSet>,
    /// The signed-in user's identity (M13), resolved from whoami and persisted
    /// in the `accounts` row. Makes `mark_own` work for server-loaded history
    /// and realtime messages.
    identity: Option<Identity>,
    sign_in: Option<SignInTask>,
    /// Lazily-built protocol client (M2), shared behind an `Arc` so the M5
    /// [`Messaging`] service can hold it too. Dropped whenever the token set
    /// changes so the next [`Session::api`] rebuilds it with fresh credentials.
    api: Option<Arc<ApiClient>>,
    /// The running realtime (Trouter) feed (M4), if [`Session::start_realtime`]
    /// has been called.
    realtime: Option<RealtimeService>,
    /// Where M8 notifications are delivered. Defaults to a [`LogNotifier`];
    /// replace it with [`Session::set_notifier`].
    notifier: Arc<dyn Notifier>,
}

impl Session {
    /// Create a session rooted at `dirs`, using the default profile, the default
    /// [`FileTokenStore`] and the SQLite store at [`AppDirs::store_file`].
    /// Does not touch the network; the initial state is
    /// [`SessionState::SignedOut`] until [`Session::restore`] or sign-in.
    pub fn new(dirs: Arc<AppDirs>) -> Result<Self> {
        let store = Arc::new(FileTokenStore::new(&dirs));
        Self::with_store(dirs, DEFAULT_PROFILE, store)
    }

    /// Create a session with an explicit profile and token store (tests,
    /// multi-account). The SQLite store is still opened from
    /// [`AppDirs::store_file`]; use [`Session::attach_store`] to substitute one
    /// (e.g. an in-memory database).
    pub fn with_store(
        dirs: Arc<AppDirs>,
        profile: impl Into<String>,
        token_store: Arc<dyn TokenStore>,
    ) -> Result<Self> {
        dirs.ensure()?;
        let store = Arc::new(Store::open(dirs.store_file())?);
        Self::with_token_store_and_db(dirs, profile, token_store, store)
    }

    /// Create a session with an explicit profile, token store **and** SQLite
    /// store. Useful for tests that want an in-memory database via
    /// [`Store::open_in_memory`].
    pub fn with_token_store_and_db(
        dirs: Arc<AppDirs>,
        profile: impl Into<String>,
        token_store: Arc<dyn TokenStore>,
        store: Arc<Store>,
    ) -> Result<Self> {
        dirs.ensure()?;
        Ok(Self {
            dirs,
            state: SessionState::SignedOut,
            profile: profile.into(),
            token_store,
            store,
            tokens: None,
            identity: None,
            sign_in: None,
            api: None,
            realtime: None,
            notifier: Arc::new(LogNotifier),
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
    pub fn token_store(&self) -> &Arc<dyn TokenStore> {
        &self.token_store
    }

    /// The local SQLite store backing this session (M3).
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// Replace the local SQLite store.
    ///
    /// Used by tests (in-memory DB) and by a future multi-database setup; the
    /// session takes ownership of the `Arc`, so callers keep sharing it.
    pub fn attach_store(&mut self, store: Arc<Store>) {
        self.store = store;
    }

    /// The current token set, if any has been loaded or minted.
    pub fn tokens(&self) -> Option<&TokenSet> {
        self.tokens.as_ref()
    }

    /// The signed-in user's identity, if whoami has resolved (or a previous
    /// run's identity was restored from the store) (M13).
    pub fn identity(&self) -> Option<&Identity> {
        self.identity.as_ref()
    }

    /// The signed-in user's MRI (`8:orgid:<guid>`), if known (M13).
    pub fn self_mri(&self) -> Option<&str> {
        self.identity
            .as_ref()
            .map(|identity| identity.mri.as_str())
            .filter(|mri| !mri.trim().is_empty())
    }

    /// Cache the signed-in identity and persist it to the `accounts` row
    /// (M13), so `mark_own` survives a restart without a fresh whoami.
    ///
    /// The existing row's `created_at` is preserved; the tenant falls back to
    /// the token set's when whoami did not name one.
    pub fn set_identity(&mut self, identity: Identity) {
        let existing = self.store.get_account(&self.profile).ok().flatten();
        let created_at = existing
            .as_ref()
            .map(|account| account.created_at)
            .unwrap_or_else(|| now_unix() as i64);
        let personal = self
            .tokens
            .as_ref()
            .map(|tokens| tokens.personal)
            .unwrap_or(false);
        let tenant = non_empty(&identity.tenant)
            .or_else(|| {
                self.tokens
                    .as_ref()
                    .map(|tokens| tokens.tenant().to_string())
            })
            .or_else(|| existing.as_ref().and_then(|account| account.tenant.clone()));
        let account = Account {
            profile: self.profile.clone(),
            tenant,
            personal,
            display_name: non_empty(&identity.display_name),
            self_mri: non_empty(&identity.mri),
            oid: non_empty(&identity.oid),
            created_at,
        };
        if let Err(e) = self.store.put_account(&account) {
            tracing::warn!(error = %e, "could not persist the signed-in identity");
        }
        self.identity = Some(identity);
    }

    /// The shared protocol client for this session, building it lazily (M13:
    /// used to spawn the whoami fetch without holding a borrow across the
    /// frame). Returns [`CoreError::NotSignedIn`] when no token set is loaded.
    pub fn client(&mut self) -> Result<Arc<ApiClient>> {
        self.api_arc()
    }

    /// The protocol client for this session, building it lazily on first use
    /// from the current [`TokenSet`].
    ///
    /// Returns [`CoreError::NotSignedIn`] when no token set is loaded.
    /// Construction is offline (it only moves tokens into an in-memory config),
    /// so calling this never hits the network; the first `list_chats` /
    /// `read_messages` call does.
    pub fn api(&mut self) -> Result<&ApiClient> {
        self.api_arc()?;
        Ok(self.api.as_ref().expect("api built above").as_ref())
    }

    /// Build (or reuse) the shared protocol client, returning the `Arc`.
    fn api_arc(&mut self) -> Result<Arc<ApiClient>> {
        if let Some(api) = &self.api {
            return Ok(Arc::clone(api));
        }
        let tokens = self.tokens.as_ref().ok_or(CoreError::NotSignedIn)?;
        let api = Arc::new(ApiClient::from_tokens(tokens));
        self.api = Some(Arc::clone(&api));
        Ok(api)
    }

    /// Whether a protocol client has already been built for this session.
    pub fn has_api(&self) -> bool {
        self.api.is_some()
    }

    /// The M5 messaging service for this session.
    ///
    /// Shares the session's [`Store`] and protocol client. Requires a token set
    /// ([`CoreError::NotSignedIn`] otherwise); building is offline. The returned
    /// [`Messaging`] owns its own in-memory paging cursors (ADR-016), so callers
    /// that page should keep the handle they get back.
    pub fn messaging(&mut self) -> Result<Messaging> {
        let client = self.api_arc()?;
        let self_mri = self.self_mri().map(str::to_string);
        let mut service = Messaging::new(self.profile.clone(), Arc::clone(&self.store), client);
        if let Some(mri) = self_mri {
            service = service.with_self_mri(mri);
        }
        Ok(service)
    }

    /// The M6 teams & channels service for this session.
    ///
    /// Shares the session's [`Store`] and protocol client (the same
    /// `Arc<ApiClient>` [`Session::messaging`] hands out). Requires a token set
    /// ([`CoreError::NotSignedIn`] otherwise); building is offline. Like
    /// [`Session::messaging`], the returned [`Teams`] owns its own in-memory
    /// channel-history cursors, so callers that page a channel should keep the
    /// handle they get back.
    pub fn teams(&mut self) -> Result<Teams> {
        let client = self.api_arc()?;
        Ok(Teams::new(
            self.profile.clone(),
            Arc::clone(&self.store),
            client,
        ))
    }

    /// The M7 files service for this session.
    ///
    /// Shares the session's [`Store`] and protocol client (the same
    /// `Arc<ApiClient>` [`Session::messaging`] hands out). Requires a token set
    /// ([`CoreError::NotSignedIn`] otherwise); building is offline. The returned
    /// [`Files`] is stateless beyond the shared store, so callers may build a
    /// fresh handle whenever they like.
    pub fn files(&mut self) -> Result<Files> {
        let client = self.api_arc()?;
        Ok(Files::new(
            self.profile.clone(),
            Arc::clone(&self.store),
            client,
        ))
    }

    /// The M12 calendar service for this session.
    ///
    /// Shares the session's [`Store`] and protocol client (the same
    /// `Arc<ApiClient>` [`Session::messaging`] hands out). Requires a token set
    /// ([`CoreError::NotSignedIn`] otherwise); building is offline. The returned
    /// [`Calendar`] is stateless beyond the shared store, so callers may build a
    /// fresh handle whenever they like.
    pub fn calendar(&mut self) -> Result<Calendar> {
        let client = self.api_arc()?;
        Ok(Calendar::new(
            self.profile.clone(),
            Arc::clone(&self.store),
            client,
        ))
    }

    /// The B3 people-search service for this session.
    ///
    /// Shares the session's protocol client (Graph `/users` `$search`). Requires
    /// a token set ([`CoreError::NotSignedIn`] otherwise); building is offline
    /// and makes no request. The returned [`People`] is stateless beyond the
    /// shared client, so callers may build a fresh handle whenever they like.
    pub fn people(&mut self) -> Result<People> {
        let client = self.api_arc()?;
        Ok(People::new(self.profile.clone(), client))
    }

    /// The B1 message-search service for this session.
    ///
    /// Shares the session's [`Store`] (the **local-first** path — the store
    /// already holds the cached history) and, when a token set is loaded, the
    /// shared [`ApiClient`] for the **optional** server fallback (Graph
    /// `/search/query`, which is expected to 403 with the Teams sign-in — see
    /// ADR-036).
    ///
    /// Unlike the other services this **never fails**: a signed-out session gets
    /// a local-only handle, because search does not depend on the server path.
    /// Building is offline.
    pub fn search(&mut self) -> Search {
        let client = self.api_arc().ok();
        let mut service = Search::new(self.profile.clone(), Arc::clone(&self.store));
        if let Some(client) = client {
            service = service.with_client(client);
        }
        service
    }

    /// The M9 call service for this session.
    ///
    /// Shares the session's [`Store`] and notifier. The signalling endpoints are
    /// derived from `region_gtms` (see [`CallEndpoints::from_region_gtms`]); the
    /// credentials are the session's IC3/skype/recorder tokens, and the identity
    /// is decoded from the skype JWT when it names one (so placing a call works
    /// out of the box). Requires a token set ([`CoreError::NotSignedIn`]
    /// otherwise); building is offline and makes no request.
    ///
    /// The real `trouter_surl` is not in `region_gtms` (it comes from the
    /// negotiated Trouter session), so a caller that needs it should
    /// [`CallSignaller::set_endpoints`] before placing a call (B-011).
    pub fn calls(&mut self) -> Result<CallService> {
        let client = self.api_arc()?;
        let tokens = self.tokens.as_ref().ok_or(CoreError::NotSignedIn)?;
        let endpoints = CallEndpoints::from_region_gtms(client.region_gtms());
        let credentials = CallCredentials {
            ic3_token: tokens.ic3.token.clone(),
            skype_token: tokens.skype.token.clone(),
            recorder_token: tokens.recorder.token.clone(),
        };
        let mut service = CallService::new(
            self.profile.clone(),
            Arc::clone(&self.store),
            CallSignaller::new(endpoints),
        )
        .with_credentials(credentials)
        .with_notifier(Arc::clone(&self.notifier));
        if let Some(identity) = CallIdentity::from_skype_token(&tokens.skype.token) {
            service = service.with_identity(identity);
        }
        Ok(service)
    }

    /// The M8 presence service for this session.
    ///
    /// Uses the **unified** backend (the default: the Graph presence endpoints
    /// 403 with the Teams first-party token — see [`crate::presence`]). The base
    /// URL comes from `region_gtms.presenceBase` when present, else the unified
    /// default; the token sent is the **skype** token, a documented, unverified
    /// choice (the unified audience/header is not confirmed — `docs/BLOCKERS.md`).
    ///
    /// Requires a token set ([`CoreError::NotSignedIn`] otherwise); building is
    /// offline.
    pub fn presence(&mut self) -> Result<Presence> {
        let client = self.api_arc()?;
        let base = presence_base_from_region(client.region_gtms());
        let token = self
            .tokens
            .as_ref()
            .map(|tokens| tokens.skype_token().to_string())
            .unwrap_or_default();
        Ok(Presence::unified(self.profile.clone(), base, token))
    }

    /// The M8 presence service using the **Graph** backend.
    ///
    /// Live this 403s (the token lacks `Presence.Read*`); it is exposed for a
    /// suitably-scoped token and for symmetry with [`Session::presence`].
    pub fn graph_presence(&mut self) -> Result<Presence> {
        let client = self.api_arc()?;
        Ok(Presence::graph(self.profile.clone(), client))
    }

    /// The M8 notifications service (unread derivation + notification
    /// delivery), sharing the session's [`Store`] and notifier. Needs no
    /// tokens; building is offline.
    pub fn notifications(&self) -> Notifications {
        Notifications::new(
            self.profile.clone(),
            Arc::clone(&self.store),
            Arc::clone(&self.notifier),
        )
    }

    /// The notifier notifications are delivered through (default
    /// [`LogNotifier`]).
    pub fn notifier(&self) -> &Arc<dyn Notifier> {
        &self.notifier
    }

    /// Replace the notifier (e.g. a desktop notifier or a recording one in
    /// tests).
    pub fn set_notifier(&mut self, notifier: Arc<dyn Notifier>) {
        self.notifier = notifier;
    }

    /// Start the realtime (Trouter) feed for the current token set (M4).
    ///
    /// Spawns the vendored connection loop and our collector task on the
    /// **ambient tokio runtime** ([`tokio::runtime::Handle::try_current`]) and
    /// stores the handle on the session. Requires a token set
    /// ([`CoreError::NotSignedIn`] otherwise) and a runtime
    /// ([`CoreError::NotReady`] otherwise). Calling it again replaces any
    /// previous feed (the old tasks are aborted on drop).
    ///
    /// The connection is the vendored [`ost::trouter::connect_and_run_with_config`],
    /// which reconnects with backoff forever; consumers read events from
    /// [`Session::subscribe`]. The feed runs with the token set captured at
    /// start time — see B-008 on reconnect token staleness.
    pub fn start_realtime(&mut self) -> Result<()> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| CoreError::NotReady("start_realtime requires an async runtime".into()))?;
        let tokens = self.tokens.as_ref().ok_or(CoreError::NotSignedIn)?.clone();
        // Dropping the previous service aborts its tasks.
        self.realtime = None;
        let self_mri = self.self_mri().map(str::to_string);
        let service = RealtimeService::start(
            &handle,
            self.profile.clone(),
            &tokens,
            Arc::clone(&self.store),
            self_mri,
        );
        self.realtime = Some(service);
        Ok(())
    }

    /// Stop the realtime feed, aborting its tasks. Idempotent.
    pub fn stop_realtime(&mut self) {
        self.realtime = None;
    }

    /// The running realtime service, if any.
    pub fn realtime(&self) -> Option<&RealtimeService> {
        self.realtime.as_ref()
    }

    /// Subscribe to the realtime event bus.
    ///
    /// Returns `None` when realtime is not running (call
    /// [`Session::start_realtime`] first). Each call returns an independent
    /// cursor; a slow consumer that falls behind receives
    /// [`RealtimeEvent::Resync`] via [`crate::realtime::recv`].
    pub fn subscribe(&self) -> Option<broadcast::Receiver<RealtimeEvent>> {
        self.realtime.as_ref().map(|service| service.subscribe())
    }

    /// Alias for [`Session::subscribe`].
    pub fn events(&self) -> Option<broadcast::Receiver<RealtimeEvent>> {
        self.subscribe()
    }

    /// Refresh the token set if any token has expired, using the real HTTP
    /// transport. Persists the refreshed set and drops the cached protocol
    /// client so the next [`Session::api`] picks up the new credentials.
    ///
    /// Returns `true` when a refresh happened, `false` when the tokens were
    /// still fresh (a no-op, no network).
    ///
    /// **Requires a tokio runtime**: the underlying `AuthDriver::refresh` is
    /// async and may make network calls. When there is no token set this is a
    /// no-op returning `Ok(false)`.
    pub async fn ensure_fresh_tokens(&mut self) -> Result<bool> {
        let driver = AuthDriver::new(HttpTransport::new());
        self.ensure_fresh_tokens_with_driver(driver).await
    }

    /// [`Session::ensure_fresh_tokens`] with an injected driver (tests supply a
    /// deterministic offline transport, mirroring
    /// [`Session::begin_sign_in_with_driver`]).
    pub async fn ensure_fresh_tokens_with_driver<T>(
        &mut self,
        driver: AuthDriver<T>,
    ) -> Result<bool>
    where
        T: AuthTransport,
    {
        let Some(current) = self.tokens.clone() else {
            return Ok(false);
        };
        if !current.needs_refresh() {
            return Ok(false);
        }
        let store = Arc::clone(&self.token_store);
        let refreshed = driver
            .refresh(&current, &self.profile, store.as_ref(), |_| {})
            .await?;
        self.tokens = Some(refreshed);
        // The cached client holds the old tokens in its config.
        self.api = None;
        // The realtime feed also captured the old tokens; stop it and let the
        // caller restart it (B-008).
        self.realtime = None;
        Ok(true)
    }

    /// Load any stored token set for this profile. On success the state becomes
    /// [`SessionState::Loading`] when credentials were found, otherwise
    /// [`SessionState::SignedOut`].
    pub fn restore(&mut self) -> Result<()> {
        // Any previously cached client was built from a different token set.
        self.api = None;
        // The realtime feed was started with the previous token set, if any.
        self.realtime = None;
        match self.token_store.load(&self.profile)? {
            Some(tokens) => {
                tracing::info!(profile = %self.profile, "restored stored credentials");
                self.tokens = Some(tokens);
                // Restore the cached identity (M13) so `mark_own` works before
                // the whoami refresh completes.
                self.identity = self
                    .store
                    .get_account(&self.profile)
                    .ok()
                    .flatten()
                    .and_then(account_identity);
                self.state = SessionState::Loading;
            }
            None => {
                self.tokens = None;
                self.identity = None;
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
        let store = Arc::clone(&self.token_store);
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
                self.api = None;
                self.realtime = None;
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
        // Stop the realtime feed before dropping the credentials it captured.
        self.realtime = None;
        self.tokens = None;
        self.identity = None;
        self.api = None;
        self.token_store.delete(&self.profile)?;
        self.state = SessionState::SignedOut;
        Ok(())
    }
}

/// `Some(trimmed)` when `value` is non-empty, else `None`.
fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Rebuild an [`Identity`] from a persisted [`Account`] row, when it carries a
/// usable MRI.
fn account_identity(account: Account) -> Option<Identity> {
    let mri = account.self_mri?;
    if mri.trim().is_empty() {
        return None;
    }
    Some(Identity {
        mri,
        oid: account.oid.unwrap_or_default(),
        display_name: account.display_name.unwrap_or_default(),
        tenant: account.tenant.unwrap_or_default(),
    })
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
    fn session_owns_a_store_and_attach_store_overrides_it() {
        let (tmp, dirs) = dirs();
        let mut session = Session::new(dirs).expect("session");
        assert_eq!(
            session.store().schema_version().unwrap(),
            crate::store::SCHEMA_VERSION
        );

        // An explicit in-memory store can be attached and is shared, not copied.
        let mem = Arc::new(crate::store::Store::open_in_memory().expect("mem store"));
        session.attach_store(Arc::clone(&mem));
        session
            .store()
            .put_account(&crate::store::Account {
                profile: "default".into(),
                tenant: None,
                personal: false,
                display_name: None,
                self_mri: None,
                oid: None,
                created_at: 1,
            })
            .unwrap();
        assert_eq!(mem.get_account("default").unwrap().unwrap().created_at, 1);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn with_token_store_and_db_uses_the_given_database() {
        let (tmp, dirs) = dirs();
        let tokens = Arc::new(MemoryTokenStore::new());
        let db = Arc::new(crate::store::Store::open_in_memory().unwrap());
        let session = Session::with_token_store_and_db(dirs, "default", tokens, Arc::clone(&db))
            .expect("session");
        assert_eq!(
            session.store().schema_version().unwrap(),
            crate::store::SCHEMA_VERSION
        );
        assert!(Arc::ptr_eq(session.store(), &db));
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

    // ----- M2: protocol client + refresh wiring -------------------------------

    #[test]
    fn api_without_tokens_is_not_signed_in() {
        let (tmp, dirs) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        let mut session = Session::with_store(dirs, "default", store).expect("session");
        assert!(!session.has_api());
        let err = session.api().unwrap_err();
        assert!(matches!(err, CoreError::NotSignedIn), "got {err:?}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn api_builds_lazily_and_offline_from_restored_tokens() {
        let (tmp, dirs) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        store.save("work", &sample_set()).unwrap();
        let mut session = Session::with_store(dirs, "work", store).expect("session");
        session.restore().expect("restore");

        // Nothing built until `api()` is called; building makes no request.
        assert!(!session.has_api());
        let url = {
            let client = session.api().expect("api");
            assert_eq!(client.request_count(), 0);
            client.chat_service_url()
        };
        assert!(session.has_api());
        assert!(url.contains("ng.msg.teams.microsoft.com"), "url = {url}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// The M2 exit criterion at the `Session` level: `Session::api()` then
    /// `list_chats` against a local fake.
    #[tokio::test]
    async fn session_lists_chats_against_local_fake() {
        let (tmp, dirs) = dirs();
        let body = serde_json::json!({
            "conversations": [{
                "id": "19:alice@unq.gbl.spaces",
                "threadProperties": { "topic": "Alice" },
                "lastMessage": { "imdisplayname": "Alice", "content": "<p>hi</p>" }
            }]
        })
        .to_string();
        let (base, seen, task) = crate::api::one_shot_json_stub(body).await;

        let mut set = sample_set();
        set.skype = StoredToken::with_expiry("skype-token", 4_000_000_000);
        set.region_gtms = serde_json::json!({
            "csaBase": base,
            "graphBase": base,
            "chatService": base
        });
        let store = Arc::new(MemoryTokenStore::new());
        store.save("default", &set).unwrap();
        let mut session = Session::with_store(dirs, "default", store).expect("session");
        session.restore().expect("restore");
        session.mark_ready();

        let chats = {
            let client = session.api().expect("api");
            client.list_chats(10).await.expect("list chats")
        };
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].name, "Alice");
        assert!(session.has_api());

        task.await.expect("stub drains");
        let req = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            req.starts_with("GET /teams/users/ME/conversations"),
            "{req}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn ensure_fresh_tokens_is_noop_when_fresh() {
        let (tmp, dirs) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        store.save("default", &sample_set()).unwrap();
        let mut session = Session::with_store(dirs, "default", store.clone()).expect("session");
        session.restore().expect("restore");

        let driver = AuthDriver::new(StubTransport);
        let refreshed = session
            .ensure_fresh_tokens_with_driver(driver)
            .await
            .expect("no-op");
        assert!(!refreshed);
        assert_eq!(session.tokens().unwrap().skype_token(), "tok");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn ensure_fresh_tokens_refreshes_expired_and_updates_store() {
        let (tmp, dirs) = dirs();
        let mut set = sample_set();
        set.graph = StoredToken::with_expiry("old-graph", 1_000);
        let store = Arc::new(MemoryTokenStore::new());
        store.save("default", &set).unwrap();
        let mut session = Session::with_store(dirs, "default", store.clone()).expect("session");
        session.restore().expect("restore");
        // Build a client first so we can prove it is invalidated on refresh.
        session.api().expect("api");

        let driver = AuthDriver::new(StubTransport);
        let refreshed = session
            .ensure_fresh_tokens_with_driver(driver)
            .await
            .expect("refresh");
        assert!(refreshed);
        assert_eq!(session.tokens().unwrap().graph.token(), "t-graph");
        assert!(
            !session.has_api(),
            "cached client must be dropped on refresh"
        );
        assert_eq!(
            store.load("default").unwrap().unwrap().graph.token(),
            "t-graph"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ----- M5: messaging service ---------------------------------------------

    #[test]
    fn messaging_requires_tokens_and_shares_the_store_and_client() {
        let (tmp, dirs_a) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        let mut session = Session::with_store(dirs_a, "default", store).expect("session");
        // No tokens yet: building the service must fail cleanly, no network.
        let err = session.messaging().unwrap_err();
        assert!(matches!(err, CoreError::NotSignedIn), "got {err:?}");

        let (tmp2, dirs2) = dirs();
        let store2 = Arc::new(MemoryTokenStore::new());
        store2.save("work", &sample_set()).unwrap();
        let mut session = Session::with_store(dirs2, "work", store2).expect("session");
        session.restore().expect("restore");

        let messaging = session.messaging().expect("messaging");
        assert_eq!(messaging.profile(), "work");
        assert!(Arc::ptr_eq(messaging.store(), session.store()));
        assert_eq!(
            messaging.client().request_count(),
            0,
            "building the service is offline"
        );
        // The session's cached client is the same one the service holds.
        assert!(session.has_api());
        std::fs::remove_dir_all(&tmp).ok();
        std::fs::remove_dir_all(&tmp2).ok();
    }

    #[test]
    fn teams_requires_tokens_and_shares_the_store_and_client() {
        let (tmp, dirs_a) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        let mut session = Session::with_store(dirs_a, "default", store).expect("session");
        // No tokens yet: building the service must fail cleanly, no network.
        let err = session.teams().unwrap_err();
        assert!(matches!(err, CoreError::NotSignedIn), "got {err:?}");

        let (tmp2, dirs2) = dirs();
        let store2 = Arc::new(MemoryTokenStore::new());
        store2.save("work", &sample_set()).unwrap();
        let mut session = Session::with_store(dirs2, "work", store2).expect("session");
        session.restore().expect("restore");

        let teams = session.teams().expect("teams");
        assert_eq!(teams.profile(), "work");
        assert!(Arc::ptr_eq(teams.store(), session.store()));
        assert_eq!(
            teams.client().request_count(),
            0,
            "building the service is offline"
        );
        assert!(session.has_api());
        std::fs::remove_dir_all(&tmp).ok();
        std::fs::remove_dir_all(&tmp2).ok();
    }

    #[test]
    fn files_requires_tokens_and_shares_the_store_and_client() {
        let (tmp, dirs_a) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        let mut session = Session::with_store(dirs_a, "default", store).expect("session");
        // No tokens yet: building the service must fail cleanly, no network.
        let err = session.files().unwrap_err();
        assert!(matches!(err, CoreError::NotSignedIn), "got {err:?}");

        let (tmp2, dirs2) = dirs();
        let store2 = Arc::new(MemoryTokenStore::new());
        store2.save("work", &sample_set()).unwrap();
        let mut session = Session::with_store(dirs2, "work", store2).expect("session");
        session.restore().expect("restore");

        let files = session.files().expect("files");
        assert_eq!(files.profile(), "work");
        assert!(Arc::ptr_eq(files.store(), session.store()));
        assert_eq!(
            files.client().request_count(),
            0,
            "building the service is offline"
        );
        assert!(session.has_api());
        std::fs::remove_dir_all(&tmp).ok();
        std::fs::remove_dir_all(&tmp2).ok();
    }

    #[test]
    fn calendar_requires_tokens_and_shares_the_store_and_client() {
        let (tmp, dirs_a) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        let mut session = Session::with_store(dirs_a, "default", store).expect("session");
        // No tokens yet: building the service must fail cleanly, no network.
        let err = session.calendar().unwrap_err();
        assert!(matches!(err, CoreError::NotSignedIn), "got {err:?}");

        let (tmp2, dirs2) = dirs();
        let store2 = Arc::new(MemoryTokenStore::new());
        store2.save("work", &sample_set()).unwrap();
        let mut session = Session::with_store(dirs2, "work", store2).expect("session");
        session.restore().expect("restore");

        let calendar = session.calendar().expect("calendar");
        assert_eq!(calendar.profile(), "work");
        assert!(Arc::ptr_eq(calendar.store(), session.store()));
        assert_eq!(
            calendar.client().request_count(),
            0,
            "building the service is offline"
        );
        assert!(session.has_api());
        std::fs::remove_dir_all(&tmp).ok();
        std::fs::remove_dir_all(&tmp2).ok();
    }

    // ----- B3: people search -------------------------------------------------

    #[test]
    fn people_requires_tokens_and_shares_the_client() {
        let (tmp, dirs_a) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        let mut session = Session::with_store(dirs_a, "default", store).expect("session");
        // No tokens yet: building the service must fail cleanly, no network.
        let err = session.people().unwrap_err();
        assert!(matches!(err, CoreError::NotSignedIn), "got {err:?}");

        let (tmp2, dirs2) = dirs();
        let store2 = Arc::new(MemoryTokenStore::new());
        store2.save("work", &sample_set()).unwrap();
        let mut session = Session::with_store(dirs2, "work", store2).expect("session");
        session.restore().expect("restore");

        let people = session.people().expect("people");
        assert_eq!(people.profile(), "work");
        assert_eq!(
            people.client().request_count(),
            0,
            "building the service is offline"
        );
        assert!(session.has_api());
        std::fs::remove_dir_all(&tmp).ok();
        std::fs::remove_dir_all(&tmp2).ok();
    }

    // ----- B1: message search ------------------------------------------------

    #[test]
    fn search_is_local_first_and_offline_without_tokens() {
        let (tmp, dirs) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        let mut session = Session::with_store(dirs, "default", store).expect("session");
        // No tokens: the handle is still built (local-only), no network.
        let search = session.search();
        assert_eq!(search.profile(), "default");
        assert!(Arc::ptr_eq(search.store(), session.store()));
        assert!(!search.has_client(), "no tokens: local-only");
        assert!(!session.has_api(), "search must not force a client build");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn search_attaches_the_client_when_signed_in() {
        let (tmp, dirs) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        store.save("work", &sample_set()).unwrap();
        let mut session = Session::with_store(dirs, "work", store).expect("session");
        session.restore().expect("restore");
        let search = session.search();
        assert_eq!(search.profile(), "work");
        assert!(
            search.has_client(),
            "signed in: the server fallback is available"
        );
        assert_eq!(search.client().unwrap().request_count(), 0);
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ----- M13: identity -----------------------------------------------------

    fn identity() -> Identity {
        Identity::from_whoami(
            &crate::api::UserInfo {
                display_name: "Alice".into(),
                mail: Some("alice@contoso.com".into()),
                id: "aaaa".into(),
            },
            "common",
        )
    }

    #[test]
    fn set_identity_persists_and_reaches_the_messaging_service() {
        let (tmp, dirs) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        store.save("default", &sample_set()).unwrap();
        let mut session = Session::with_store(dirs, "default", store).expect("session");
        session.restore().expect("restore");
        assert!(session.self_mri().is_none());

        session.set_identity(identity());
        assert_eq!(session.self_mri(), Some("8:orgid:aaaa"));
        assert_eq!(session.identity().map(|i| i.oid.as_str()), Some("aaaa"));
        let account = session.store().get_account("default").unwrap().unwrap();
        assert_eq!(account.self_mri.as_deref(), Some("8:orgid:aaaa"));
        assert_eq!(account.oid.as_deref(), Some("aaaa"));
        assert_eq!(account.display_name.as_deref(), Some("Alice"));
        assert!(account.created_at > 0, "a fresh row gets a timestamp");

        // A service built after the identity is known carries the MRI.
        let messaging = session.messaging().expect("messaging");
        assert_eq!(messaging.self_mri().as_deref(), Some("8:orgid:aaaa"));

        // Sign-out clears it.
        session.sign_out().expect("sign out");
        assert!(session.identity().is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn restore_recovers_the_persisted_identity() {
        let (tmp, dirs) = dirs();
        let token_store = Arc::new(MemoryTokenStore::new());
        token_store.save("default", &sample_set()).unwrap();
        {
            let mut session =
                Session::with_store(dirs.clone(), "default", token_store.clone()).expect("session");
            session.restore().expect("restore");
            session.set_identity(identity());
        }
        // A fresh session over the same store restores the identity offline.
        let mut session =
            Session::with_store(dirs.clone(), "default", token_store).expect("session 2");
        session.restore().expect("restore");
        assert_eq!(session.self_mri(), Some("8:orgid:aaaa"));
        assert_eq!(
            session.identity().map(|i| i.display_name.as_str()),
            Some("Alice")
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ----- M4: realtime wiring ------------------------------------------------

    #[test]
    fn subscribe_is_none_before_realtime_starts() {
        let (tmp, dirs) = dirs();
        let session = Session::new(dirs).expect("session");
        assert!(session.realtime().is_none());
        assert!(session.subscribe().is_none());
        assert!(session.events().is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn start_realtime_without_runtime_errors() {
        let (tmp, dirs) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        store.save("default", &sample_set()).unwrap();
        let mut session = Session::with_store(dirs, "default", store).expect("session");
        session.restore().expect("restore");

        // Plain test: no tokio runtime, so this must fail before spawning any
        // network task.
        let err = session.start_realtime().unwrap_err();
        assert!(format!("{err}").contains("async runtime"), "got {err:?}");
        assert!(session.realtime().is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn start_realtime_without_tokens_is_not_signed_in() {
        let (tmp, dirs) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        let mut session = Session::with_store(dirs, "default", store).expect("session");
        // No tokens: must not spawn the connection loop.
        let err = session.start_realtime().unwrap_err();
        assert!(matches!(err, CoreError::NotSignedIn), "got {err:?}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ----- M8: presence & notifications --------------------------------------

    #[test]
    fn presence_requires_tokens_and_uses_the_region_base_and_skype_token() {
        let (tmp, dirs_a) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        let mut session = Session::with_store(dirs_a, "default", store).expect("session");
        let err = session.presence().unwrap_err();
        assert!(matches!(err, CoreError::NotSignedIn), "got {err:?}");

        let (tmp2, dirs_b) = dirs();
        let mut set = sample_set();
        set.skype = StoredToken::with_expiry("skype-token", 4_000_000_000);
        set.region_gtms = serde_json::json!({ "presenceBase": "http://127.0.0.1:9" });
        let store2 = Arc::new(MemoryTokenStore::new());
        store2.save("work", &set).unwrap();
        let mut session = Session::with_store(dirs_b, "work", store2).expect("session");
        session.restore().expect("restore");

        let presence = session.presence().expect("presence");
        assert_eq!(presence.profile(), "work");
        assert_eq!(presence.backend().kind(), "unified");
        match presence.backend() {
            crate::presence::PresenceBackend::Unified(unified) => {
                assert_eq!(unified.base(), "http://127.0.0.1:9");
            }
            other => panic!("expected the unified backend, got {}", other.kind()),
        }
        // The Graph backend builds too.
        let graph = session.graph_presence().expect("graph presence");
        assert_eq!(graph.backend().kind(), "graph");

        std::fs::remove_dir_all(&tmp).ok();
        std::fs::remove_dir_all(&tmp2).ok();
    }

    #[test]
    fn notifications_use_the_configured_notifier() {
        let (tmp, dirs) = dirs();
        let mut session = Session::with_store(dirs, "default", Arc::new(MemoryTokenStore::new()))
            .expect("session");
        let recorder = Arc::new(crate::notifications::RecordingNotifier::new());
        session.set_notifier(recorder.clone());

        let notifications = session.notifications();
        assert_eq!(notifications.profile(), session.profile());
        assert!(Arc::ptr_eq(notifications.store(), session.store()));

        let ctx = crate::notifications::NotificationContext::new().with_self_mri("8:orgid:self");
        let event = RealtimeEvent::NewMessage {
            chat_id: "c1".into(),
            message: crate::model::Message {
                id: "m1".into(),
                chat_id: "c1".into(),
                sender_mri: "8:orgid:other".into(),
                sender_name: "Alice".into(),
                timestamp: "2026-01-01T00:00:00Z".into(),
                content: "hi".into(),
                raw: "<p>hi</p>".into(),
                reactions: Vec::new(),
                reply_to: None,
                client_message_id: None,
                edited: false,
                is_own: false,
                send_state: crate::model::SendState::Sent,
            },
        };
        let delivered = notifications.notify_event(&event, &ctx).expect("delivered");
        assert_eq!(
            delivered.kind,
            crate::notifications::NotificationKind::NewMessage
        );
        assert_eq!(recorder.len(), 1);
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ----- M9: calls ---------------------------------------------------------

    fn skype_jwt() -> String {
        use base64::Engine as _;
        let enc = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let header = enc(br#"{"alg":"none"}"#);
        let payload = enc(serde_json::json!({
            "skypeid": "orgid:self-guid",
            "name": "Alice",
            "tid": "tenant-1",
        })
        .to_string()
        .as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn calls_requires_tokens_and_builds_from_the_region_and_skype_token() {
        let (tmp, dirs_a) = dirs();
        let store = Arc::new(MemoryTokenStore::new());
        let mut session = Session::with_store(dirs_a, "default", store).expect("session");
        let err = session.calls().unwrap_err();
        assert!(matches!(err, CoreError::NotSignedIn), "got {err:?}");

        let (tmp2, dirs_b) = dirs();
        let mut set = sample_set();
        set.skype = StoredToken::with_expiry(skype_jwt(), 4_000_000_000);
        set.region_gtms = serde_json::json!({
            "epconvUrl": "http://127.0.0.1:9/epconv",
            "trouterSurl": "/surl/",
        });
        let store2 = Arc::new(MemoryTokenStore::new());
        store2.save("work", &set).unwrap();
        let mut session = Session::with_store(dirs_b, "work", store2).expect("session");
        session.restore().expect("restore");

        let service = session.calls().expect("calls");
        assert_eq!(service.profile(), "work");
        assert!(Arc::ptr_eq(service.store(), session.store()));
        assert_eq!(service.state(), crate::calls::CallState::Idle);
        assert_eq!(
            service.signaller().endpoints().epconv_url,
            "http://127.0.0.1:9/epconv"
        );
        assert_eq!(service.signaller().endpoints().trouter_surl, "/surl/");
        // Building the service is offline.
        assert!(session.has_api());
        assert_eq!(session.api().unwrap().request_count(), 0);

        std::fs::remove_dir_all(&tmp).ok();
        std::fs::remove_dir_all(&tmp2).ok();
    }
}
