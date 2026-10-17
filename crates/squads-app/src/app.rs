//! The application state and the `eframe::App` implementation (milestone **M10**).
//!
//! This module is the glue between the UI-free [`squads_core`] services and the
//! egui views:
//!
//! * [`SquadsApp`] owns the [`Session`], the shared [`Store`], the service
//!   handles (built as `Arc`s so an async call can outlive the frame) and all
//!   the view state (page, selection, drafts, caches, toasts).
//! * [`eframe::App::logic`] drains [`AppEvent`]s from the backend and applies
//!   them, requesting a repaint **only when something actually changed** (see
//!   ADR-006/ADR-027). Idle CPU stays at ~0%.
//! * [`eframe::App::ui`] composes the panels (via [`crate::views`]), then calls
//!   [`SquadsApp::apply_actions`] — the zapfast "UI → Action → apply" pattern
//!   (ADR-026). Views never touch the network; they only push [`Action`]s.
//!
//! ## Testability
//!
//! [`SquadsApp::from_session`] builds the app from an injected [`Session`]; the
//! headless tests construct one over an in-memory [`Store`] and a
//! [`MemoryTokenStore`](squads_core::auth::store::MemoryTokenStore) and seed the
//! store directly, so no network is touched. The action log
//! ([`SquadsApp::applied_actions`]) lets a test assert what a widget queued
//! without running a backend task.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use squads_core::model::{Channel, Message, SendState};
use squads_core::{
    AppDirs, CallLogEntry, CallService, CallState, Chat, ContactPresence, FileEntry, Files,
    IncomingCall, Messaging, RealtimeEvent, Session, Settings, SignInProgress, Store, Team, Teams,
    Theme,
};

use crate::runtime::{AppEvent, AppRuntime};
use crate::theme::{self, Palette};
use crate::views;

/// How many chats to keep in the list.
pub const CHAT_LIMIT: usize = 200;
/// How many messages to keep in the timeline.
pub const TIMELINE_LIMIT: usize = 300;

/// Which top-level section the nav rail selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    /// Chats: chat list + conversation.
    #[default]
    Chats,
    /// Teams & channels browser.
    Teams,
    /// Files for the selected chat/channel.
    Files,
    /// Calls: place/accept/end + recents.
    Calls,
    /// Settings: theme, account, sign-out.
    Settings,
}

impl Page {
    /// The nav-rail label (also the accessible name of the nav button).
    pub fn label(self) -> &'static str {
        match self {
            Page::Chats => "Chats",
            Page::Teams => "Teams",
            Page::Files => "Files",
            Page::Calls => "Calls",
            Page::Settings => "Settings",
        }
    }

    /// Every page, in nav-rail order.
    pub fn all() -> [Page; 5] {
        [
            Page::Chats,
            Page::Teams,
            Page::Files,
            Page::Calls,
            Page::Settings,
        ]
    }
}

/// A user intent produced by a view and applied after the frame is drawn.
///
/// This is the "Action" half of UI → Action → apply: a widget records what the
/// user asked for; [`SquadsApp::apply_actions`] turns it into a backend call or
/// a state change. Keeping them separate means a view never mutates shared
/// state mid-layout and a test can inspect the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Start the device-code sign-in flow.
    BeginSignIn {
        /// Sign in to a personal (consumer) account.
        use_personal: bool,
    },
    /// Sign out and clear the in-memory session.
    SignOut,
    /// Switch the nav rail to a page.
    SetPage(Page),
    /// Select a chat / channel conversation and open it.
    SelectChat(String),
    /// Fetch an older page of the selected conversation.
    LoadOlder(String),
    /// Send `text` to `chat_id`.
    SendMessage {
        /// Destination conversation.
        chat_id: String,
        /// The body the user typed.
        text: String,
    },
    /// Retry a failed/pending send.
    RetryMessage {
        /// Destination conversation.
        chat_id: String,
        /// The stable idempotency key.
        client_message_id: String,
    },
    /// Request the attachment picker (a stub: the path exists, the dialog does
    /// not yet).
    Attach {
        /// Conversation the upload would target.
        chat_id: String,
    },
    /// Refresh the chat list from the service.
    RefreshChats,
    /// Refresh teams & channels from the service.
    RefreshTeams,
    /// Expand/collapse a team's channel list.
    ToggleTeam(String),
    /// Refresh the files for a conversation.
    RefreshFiles(String),
    /// Download a cached file to the local media directory.
    DownloadFile {
        /// Owning conversation.
        chat_id: String,
        /// DriveItem id.
        file_id: String,
    },
    /// Place an outgoing call.
    PlaceCall {
        /// Conversation/thread to call in.
        thread_id: String,
        /// Callee MRI for a 1:1 (optional).
        callee: Option<String>,
        /// Offer video.
        video: bool,
    },
    /// Accept the ringing incoming call.
    AcceptCall,
    /// Decline the ringing incoming call.
    DeclineCall,
    /// End the current call.
    EndCall,
    /// Mark a connecting call as connected (media up; B-011 keeps this manual).
    MarkCallConnected,
    /// Dismiss a finished call.
    ResetCall,
    /// Switch the colour theme.
    SetTheme(Theme),
    /// Clear the transient toasts.
    DismissToasts,
}

/// A transient message shown at the bottom of the window.
#[derive(Debug, Clone)]
struct Toast {
    text: String,
    error: bool,
}

/// Sign-in-screen state that is not derived from the session each frame.
#[derive(Debug, Default)]
pub(crate) struct LoginView {
    pub(crate) use_personal: bool,
    pub(crate) user_code: Option<String>,
    pub(crate) verification_uri: Option<String>,
    pub(crate) message: String,
    pub(crate) hint: String,
    pub(crate) error: Option<String>,
}

/// Root application state.
pub struct SquadsApp {
    // Core
    session: Session,
    runtime: AppRuntime,
    ctx: egui::Context,
    dirs: Arc<AppDirs>,
    store: Arc<Store>,
    profile: String,
    settings: Settings,
    palette: Palette,

    // Services (built lazily once signed in; `Arc` so a spawned call can hold
    // them past the frame).
    messaging: Option<Arc<Messaging>>,
    teams_svc: Option<Arc<Teams>>,
    files_svc: Option<Arc<Files>>,
    calls_svc: Option<Arc<CallService>>,

    // Session / login
    signed_in: bool,
    signing_in: bool,
    login: LoginView,

    // View state
    page: Page,
    chats: Vec<Chat>,
    selected_chat: Option<String>,
    timeline: Vec<Message>,
    has_older: bool,
    drafts: HashMap<String, String>,
    search: String,
    show_info: bool,
    /// Unread counts per chat, recomputed when the chat list reloads (never per
    /// frame — the store is synchronous and this keeps idle frames cheap).
    unread: HashMap<String, usize>,
    /// Chats whose newest cached messages include a pending send.
    pending: HashSet<String>,
    /// Chats whose newest cached messages include a failed send.
    failed: HashSet<String>,

    teams: Vec<Team>,
    expanded_teams: HashSet<String>,
    channels: HashMap<String, Vec<Channel>>,

    files: Vec<FileEntry>,

    call_state: CallState,
    call_log: Vec<CallLogEntry>,
    incoming: Option<IncomingCall>,
    presence: HashMap<String, ContactPresence>,

    realtime_started: bool,

    // Action pipeline
    actions: Vec<Action>,
    applied: Vec<Action>,
    toasts: Vec<Toast>,
}

impl SquadsApp {
    /// Boot the app: resolve the session, restore stored credentials and build
    /// the state. Called by [`crate::main`].
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        dirs: Arc<AppDirs>,
        settings: Settings,
    ) -> anyhow::Result<Self> {
        let mut session = Session::new(dirs.clone())?;
        session.restore()?;
        Self::from_session(cc.egui_ctx.clone(), dirs, session, settings)
    }

    /// Build the app from an already-constructed session. The session is **not**
    /// restored here, so tests can inject a ready session over an in-memory
    /// store without touching the network.
    pub fn from_session(
        ctx: egui::Context,
        dirs: Arc<AppDirs>,
        session: Session,
        settings: Settings,
    ) -> anyhow::Result<Self> {
        let mut app = Self::build(ctx, dirs, session, settings)?;

        // Paint the cached (offline) state immediately.
        app.reload_chats_from_store();
        app.reload_call_log_from_store();

        if app.session.tokens().is_some() {
            app.session.mark_ready();
            app.on_signed_in();
        }
        Ok(app)
    }

    /// Build the app in **offline mode** for the headless UI tests: the shell is
    /// shown whenever the injected session reports `Ready`, but no services are
    /// built, no network task is spawned and realtime is not started. Everything
    /// the views render comes from the seeded store.
    #[cfg(test)]
    pub(crate) fn headless(
        ctx: egui::Context,
        dirs: Arc<AppDirs>,
        session: Session,
        settings: Settings,
    ) -> anyhow::Result<Self> {
        let mut app = Self::build(ctx, dirs, session, settings)?;
        app.reload_chats_from_store();
        app.reload_call_log_from_store();
        app.signed_in = app.session.state().is_ready();
        Ok(app)
    }

    fn build(
        ctx: egui::Context,
        dirs: Arc<AppDirs>,
        session: Session,
        settings: Settings,
    ) -> anyhow::Result<Self> {
        let palette = theme::install(&ctx, settings.theme);
        let store = Arc::clone(session.store());
        let profile = session.profile().to_string();
        let runtime = AppRuntime::new()?;
        runtime.set_context(ctx.clone());

        Ok(Self {
            session,
            runtime,
            ctx,
            dirs,
            store,
            profile,
            settings,
            palette,
            messaging: None,
            teams_svc: None,
            files_svc: None,
            calls_svc: None,
            signed_in: false,
            signing_in: false,
            login: LoginView::default(),
            page: Page::default(),
            chats: Vec::new(),
            selected_chat: None,
            timeline: Vec::new(),
            has_older: false,
            drafts: HashMap::new(),
            search: String::new(),
            show_info: false,
            unread: HashMap::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
            teams: Vec::new(),
            expanded_teams: HashSet::new(),
            channels: HashMap::new(),
            files: Vec::new(),
            call_state: CallState::Idle,
            call_log: Vec::new(),
            incoming: None,
            presence: HashMap::new(),
            realtime_started: false,
            actions: Vec::new(),
            applied: Vec::new(),
            toasts: Vec::new(),
        })
    }

    // ----- accessors used by the views and the headless tests ---------------

    /// The account profile.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The current page.
    pub fn page(&self) -> Page {
        self.page
    }

    /// The palette the views draw with.
    pub fn palette(&self) -> Palette {
        self.palette
    }

    /// The chat list (cached + fetched).
    pub fn chats(&self) -> &[Chat] {
        &self.chats
    }

    /// The currently selected conversation id.
    pub fn selected_chat(&self) -> Option<&str> {
        self.selected_chat.as_deref()
    }

    /// The timeline of the selected conversation (oldest-first).
    pub fn timeline(&self) -> &[Message] {
        &self.timeline
    }

    /// Whether an older page is known to be available.
    pub fn has_older(&self) -> bool {
        self.has_older
    }

    /// The joined teams (cached + fetched).
    pub fn teams(&self) -> &[Team] {
        &self.teams
    }

    /// Whether a team's channel list is expanded.
    pub fn team_expanded(&self, team_id: &str) -> bool {
        self.expanded_teams.contains(team_id)
    }

    /// A team's cached channels.
    pub fn channels_for(&self, team_id: &str) -> &[Channel] {
        self.channels.get(team_id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The files for the selected conversation.
    pub fn files(&self) -> &[FileEntry] {
        &self.files
    }

    /// The call state machine's current state.
    pub fn call_state(&self) -> &CallState {
        &self.call_state
    }

    /// The local call history (newest first).
    pub fn call_log(&self) -> &[CallLogEntry] {
        &self.call_log
    }

    /// The ringing incoming call, if any.
    pub fn incoming_call(&self) -> Option<&IncomingCall> {
        self.incoming.as_ref()
    }

    /// Whether the shell or the login screen is shown.
    pub fn is_signed_in(&self) -> bool {
        self.signed_in
    }

    /// The theme currently in effect.
    pub fn theme(&self) -> Theme {
        self.settings.theme
    }

    /// The sign-in view state (for the login screen).
    pub fn login(&self) -> &LoginView {
        &self.login
    }

    /// The message draft for a conversation.
    pub fn draft(&self, chat_id: &str) -> &str {
        self.drafts.get(chat_id).map(String::as_str).unwrap_or("")
    }

    /// The actions applied so far (a test-visible log; also useful for a debug
    /// overlay). Bounded so it cannot grow without limit.
    #[allow(dead_code)]
    pub fn applied_actions(&self) -> &[Action] {
        &self.applied
    }

    /// Whether the user selected the personal (consumer) account toggle.
    pub fn use_personal(&self) -> bool {
        self.login.use_personal
    }

    /// Set the personal/work toggle on the login screen.
    pub fn set_use_personal(&mut self, personal: bool) {
        self.login.use_personal = personal;
    }

    /// Whether the info side panel is open.
    pub fn show_info(&self) -> bool {
        self.show_info
    }

    /// The chat-list search filter.
    pub fn search(&self) -> &str {
        &self.search
    }

    /// Whether a sign-in flow is currently running.
    pub fn signing_in(&self) -> bool {
        self.signing_in
    }

    // ----- mutable view state the views write directly ----------------------

    /// Set the info side panel open/closed.
    pub fn set_show_info(&mut self, open: bool) {
        self.show_info = open;
    }

    /// Set the chat-list search filter.
    pub fn set_search(&mut self, text: String) {
        self.search = text;
    }

    /// The mutable draft for a conversation (creates an empty one on demand).
    pub fn draft_mut(&mut self, chat_id: &str) -> &mut String {
        self.drafts.entry(chat_id.to_string()).or_default()
    }

    /// Queue an action (called by the views).
    pub fn push_action(&mut self, action: Action) {
        self.actions.push(action);
    }

    /// A preview line for a chat (falls back to the last sender).
    pub fn chat_preview(&self, chat: &Chat) -> String {
        chat.last_message_preview
            .clone()
            .or_else(|| chat.last_message_sender.clone())
            .unwrap_or_default()
    }

    /// The unread count for a chat, derived from the store.
    pub fn unread_count(&self, chat_id: &str) -> usize {
        self.unread.get(chat_id).copied().unwrap_or(0)
    }

    /// Whether the chat has a pending outgoing message.
    pub fn chat_pending(&self, chat_id: &str) -> bool {
        self.pending.contains(chat_id)
    }

    /// Whether the chat has a failed outgoing message.
    pub fn chat_failed(&self, chat_id: &str) -> bool {
        self.failed.contains(chat_id)
    }

    // ----- core wiring ------------------------------------------------------

    fn ensure_services(&mut self) {
        if self.session.tokens().is_none() {
            return;
        }
        if self.messaging.is_none() {
            match self.session.messaging() {
                Ok(service) => self.messaging = Some(Arc::new(service)),
                Err(e) => self.push_error(e),
            }
        }
        if self.teams_svc.is_none() {
            match self.session.teams() {
                Ok(service) => self.teams_svc = Some(Arc::new(service)),
                Err(e) => self.push_error(e),
            }
        }
        if self.files_svc.is_none() {
            match self.session.files() {
                Ok(service) => self.files_svc = Some(Arc::new(service)),
                Err(e) => self.push_error(e),
            }
        }
        if self.calls_svc.is_none() {
            match self.session.calls() {
                Ok(service) => self.calls_svc = Some(Arc::new(service)),
                Err(e) => self.push_error(e),
            }
        }
    }

    fn on_signed_in(&mut self) {
        self.signed_in = true;
        self.signing_in = false;
        self.login = LoginView::default();
        self.ensure_services();
        self.reload_chats_from_store();
        self.reload_call_log_from_store();
        self.refresh_chats();
        self.start_realtime();
    }

    fn start_realtime(&mut self) {
        if self.realtime_started {
            return;
        }
        let handle = self.runtime.handle();
        // `start_realtime` needs an ambient runtime; enter the backend's.
        let started = {
            let _guard = handle.enter();
            self.session.start_realtime().is_ok()
        };
        if !started {
            return;
        }
        self.realtime_started = true;
        if let Some(mut rx) = self.session.subscribe() {
            let sender = self.runtime.sender();
            self.runtime.spawn_task(async move {
                loop {
                    match rx.recv().await {
                        Ok(event) => sender.send(AppEvent::Realtime(event)),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            sender.send(AppEvent::Realtime(RealtimeEvent::Resync {
                                reason: "bus lagged".into(),
                            }));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }
    }

    fn reload_chats_from_store(&mut self) {
        match self.store.list_chats(&self.profile, CHAT_LIMIT) {
            Ok(chats) => self.chats = chats,
            Err(e) => self.push_error(e),
        }
        // Derive the per-chat badges once, here, rather than per frame.
        self.unread.clear();
        self.pending.clear();
        self.failed.clear();
        for chat in &self.chats {
            let unread = self
                .store
                .unread_count(&self.profile, &chat.id)
                .unwrap_or(0);
            if unread > 0 {
                self.unread.insert(chat.id.clone(), unread);
            }
            if let Ok(recent) = self.store.messages(&chat.id, 16) {
                if recent.iter().any(|m| m.send_state == SendState::Pending) {
                    self.pending.insert(chat.id.clone());
                }
                if recent.iter().any(|m| m.send_state == SendState::Failed) {
                    self.failed.insert(chat.id.clone());
                }
            }
        }
    }

    fn reload_timeline(&mut self) {
        let Some(chat_id) = self.selected_chat.clone() else {
            self.timeline.clear();
            return;
        };
        match self.store.messages(&chat_id, TIMELINE_LIMIT) {
            Ok(messages) => self.timeline = messages,
            Err(e) => self.push_error(e),
        }
    }

    fn reload_call_log_from_store(&mut self) {
        match self.store.call_log(&self.profile, 50) {
            Ok(entries) => self.call_log = entries,
            Err(e) => self.push_error(e),
        }
    }

    fn push_error(&mut self, error: impl std::fmt::Display) {
        let text = error.to_string();
        tracing::warn!(error = %text, "app error");
        self.toasts.push(Toast { text, error: true });
    }

    fn refresh_chats(&mut self) {
        let Some(messaging) = self.messaging.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match messaging.client().list_chats(CHAT_LIMIT).await {
                Ok(infos) => {
                    let chats: Vec<Chat> = infos.iter().map(Chat::from).collect();
                    AppEvent::ChatsLoaded(chats)
                }
                Err(e) => AppEvent::Error(format!("list chats: {e}")),
            }
        });
    }

    fn select_chat(&mut self, chat_id: String) {
        self.selected_chat = Some(chat_id.clone());
        self.reload_timeline();
        self.has_older = false;
        self.files = self
            .store
            .files_for_chat(&self.profile, &chat_id)
            .unwrap_or_default();

        let Some(messaging) = self.messaging.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match messaging.open(&chat_id).await {
                Ok(page) => AppEvent::MessagesLoaded {
                    chat_id,
                    messages: page.messages,
                },
                Err(e) => AppEvent::Error(format!("open chat: {e}")),
            }
        });
    }

    fn load_older(&mut self, chat_id: String) {
        let Some(messaging) = self.messaging.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match messaging.load_older(&chat_id).await {
                Ok(page) => AppEvent::OlderLoaded {
                    chat_id,
                    messages: page.messages,
                    has_more: page.backward_link.is_some(),
                },
                Err(e) => AppEvent::Error(format!("load older: {e}")),
            }
        });
    }

    fn send_message(&mut self, chat_id: String, text: String) {
        let Some(messaging) = self.messaging.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            let _ = messaging.send_text(&chat_id, &text).await;
            match messaging.cached(&chat_id, TIMELINE_LIMIT) {
                Ok(messages) => AppEvent::MessagesLoaded { chat_id, messages },
                Err(e) => AppEvent::Error(format!("send: {e}")),
            }
        });
    }

    fn retry_message(&mut self, chat_id: String, client_message_id: String) {
        let Some(messaging) = self.messaging.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            let _ = messaging.retry(&chat_id, &client_message_id).await;
            match messaging.cached(&chat_id, TIMELINE_LIMIT) {
                Ok(messages) => AppEvent::MessagesLoaded { chat_id, messages },
                Err(e) => AppEvent::Error(format!("retry: {e}")),
            }
        });
    }

    fn refresh_teams(&mut self) {
        self.teams = self.store.list_teams(&self.profile).unwrap_or_default();
        let Some(teams) = self.teams_svc.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match teams.list_teams().await {
                Ok(list) => AppEvent::TeamsLoaded(list),
                Err(e) => AppEvent::Error(format!("list teams: {e}")),
            }
        });
    }

    fn toggle_team(&mut self, team_id: String) {
        if self.expanded_teams.contains(&team_id) {
            self.expanded_teams.remove(&team_id);
            return;
        }
        self.expanded_teams.insert(team_id.clone());
        let cached = self.store.channels_for_team(&team_id).unwrap_or_default();
        self.channels.insert(team_id.clone(), cached);

        let Some(teams) = self.teams_svc.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match teams.channels(&team_id).await {
                Ok(channels) => AppEvent::ChannelsLoaded { team_id, channels },
                Err(e) => AppEvent::Error(format!("channels: {e}")),
            }
        });
    }

    fn refresh_files(&mut self, chat_id: String) {
        self.files = self
            .store
            .files_for_chat(&self.profile, &chat_id)
            .unwrap_or_default();
        let Some(files) = self.files_svc.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match files.list_chat_files(&chat_id).await {
                Ok(list) => AppEvent::FilesLoaded {
                    chat_id,
                    files: list,
                },
                Err(e) => AppEvent::Error(format!("list files: {e}")),
            }
        });
    }

    fn download_file(&mut self, chat_id: String, file_id: String) {
        let Some(files) = self.files_svc.clone() else {
            return;
        };
        let Some(entry) = self
            .store
            .files_for_chat(&self.profile, &chat_id)
            .unwrap_or_default()
            .into_iter()
            .find(|f| f.id == file_id)
        else {
            return;
        };
        let dest = self.dirs.media_dir().join(&entry.name);
        let dest_path = dest.to_string_lossy().into_owned();
        self.runtime.spawn(async move {
            match files.download(&entry, &dest_path, |_, _| {}).await {
                Ok(_) => AppEvent::Error(format!("downloaded {}", dest.display())),
                Err(e) => AppEvent::Error(format!("download: {e}")),
            }
        });
    }

    fn place_call(&mut self, thread_id: String, callee: Option<String>, video: bool) {
        let Some(calls) = self.calls_svc.clone() else {
            self.push_error("calling requires sign-in");
            return;
        };
        let sender = self.runtime.sender();
        self.runtime.spawn_task(async move {
            match calls.place(&thread_id, callee.as_deref(), video).await {
                Ok(_) => sender.send(AppEvent::CallState(calls.state())),
                Err(e) => {
                    sender.send(AppEvent::CallState(calls.state()));
                    sender.send(AppEvent::Error(format!("call: {e}")));
                }
            }
        });
    }

    fn accept_call(&mut self) {
        let (Some(calls), Some(incoming)) = (self.calls_svc.clone(), self.incoming.clone()) else {
            return;
        };
        let sender = self.runtime.sender();
        self.runtime.spawn_task(async move {
            match calls.accept(&incoming, false).await {
                Ok(_) => sender.send(AppEvent::CallState(calls.state())),
                Err(e) => {
                    sender.send(AppEvent::CallState(calls.state()));
                    sender.send(AppEvent::Error(format!("accept: {e}")));
                }
            }
        });
    }

    fn decline_call(&mut self) {
        let (Some(calls), Some(incoming)) = (self.calls_svc.clone(), self.incoming.clone()) else {
            return;
        };
        let sender = self.runtime.sender();
        self.runtime.spawn_task(async move {
            let _ = calls.decline(&incoming).await;
            sender.send(AppEvent::CallState(calls.state()));
        });
    }

    fn end_call(&mut self) {
        let (Some(calls), Some(incoming)) = (self.calls_svc.clone(), self.incoming.clone()) else {
            if let Some(calls) = self.calls_svc.clone() {
                calls.reset();
                self.call_state = calls.state();
            }
            return;
        };
        let sender = self.runtime.sender();
        self.runtime.spawn_task(async move {
            let _ = calls.end(&incoming).await;
            sender.send(AppEvent::CallState(calls.state()));
        });
    }

    // ----- event application ------------------------------------------------

    fn apply_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::SignInProgress(progress) => self.on_sign_in_progress(progress),
            AppEvent::ChatsLoaded(chats) => {
                if let Err(e) = self.store.upsert_chats(&self.profile, &chats) {
                    self.push_error(e);
                }
                self.chats = chats;
            }
            AppEvent::MessagesLoaded { chat_id, messages } => {
                if let Err(e) = self.store.upsert_messages(&self.profile, &messages) {
                    self.push_error(e);
                }
                if self.selected_chat.as_deref() == Some(chat_id.as_str()) {
                    self.reload_timeline();
                    self.has_older = self
                        .messaging
                        .as_ref()
                        .map(|m| m.cursor(&chat_id).is_some())
                        .unwrap_or(false);
                }
                self.reload_chats_from_store();
            }
            AppEvent::OlderLoaded {
                chat_id,
                messages,
                has_more,
            } => {
                if let Err(e) = self.store.upsert_messages(&self.profile, &messages) {
                    self.push_error(e);
                }
                if self.selected_chat.as_deref() == Some(chat_id.as_str()) {
                    self.reload_timeline();
                    self.has_older = has_more;
                }
            }
            AppEvent::TeamsLoaded(teams) => {
                if let Err(e) = self.store.upsert_teams(&self.profile, &teams) {
                    self.push_error(e);
                }
                self.teams = teams;
            }
            AppEvent::ChannelsLoaded { team_id, channels } => {
                if let Err(e) = self.store.upsert_channels(&self.profile, &channels) {
                    self.push_error(e);
                }
                self.channels.insert(team_id, channels);
            }
            AppEvent::FilesLoaded { chat_id, files } => {
                if let Err(e) = self
                    .store
                    .upsert_files(&self.profile, Some(&chat_id), &files)
                {
                    self.push_error(e);
                }
                if self.selected_chat.as_deref() == Some(chat_id.as_str()) {
                    self.files = files;
                }
            }
            AppEvent::PresenceUpdated(list) => {
                for presence in list {
                    self.presence.insert(presence.mri.clone(), presence);
                }
            }
            AppEvent::Realtime(event) => self.on_realtime(event),
            AppEvent::CallState(state) => self.call_state = state,
            AppEvent::CallLogLoaded(entries) => self.call_log = entries,
            AppEvent::Error(message) => self.toasts.push(Toast {
                text: message,
                error: true,
            }),
        }
    }

    fn on_realtime(&mut self, event: RealtimeEvent) {
        match &event {
            RealtimeEvent::NewMessage { chat_id, message } => {
                if let Err(e) = self
                    .store
                    .upsert_messages(&self.profile, std::slice::from_ref(message))
                {
                    self.push_error(e);
                }
                if self.selected_chat.as_deref() == Some(chat_id.as_str()) {
                    self.reload_timeline();
                }
                self.reload_chats_from_store();
            }
            RealtimeEvent::MessageEdited {
                chat_id,
                message_id,
                content,
                ..
            } => {
                if let Err(e) =
                    self.store
                        .update_message_edit(&self.profile, chat_id, message_id, content)
                {
                    self.push_error(e);
                }
                if self.selected_chat.as_deref() == Some(chat_id.as_str()) {
                    self.reload_timeline();
                }
            }
            RealtimeEvent::MessageDeleted {
                chat_id,
                message_id,
            } => {
                let _ = self
                    .store
                    .delete_message(&self.profile, chat_id, message_id);
                if self.selected_chat.as_deref() == Some(chat_id.as_str()) {
                    self.reload_timeline();
                }
            }
            RealtimeEvent::Resync { .. } => {
                self.refresh_chats();
                if let Some(chat_id) = self.selected_chat.clone() {
                    self.select_chat(chat_id);
                }
            }
            _ => {}
        }
    }

    fn on_sign_in_progress(&mut self, progress: SignInProgress) {
        match progress {
            SignInProgress::AwaitingUser {
                user_code,
                verification_uri,
                message,
            } => {
                self.login.user_code = Some(user_code);
                self.login.verification_uri = Some(verification_uri);
                self.login.message = message;
                self.login.hint = "Waiting for sign-in…".into();
            }
            SignInProgress::Completed(_) => self.on_signed_in(),
            SignInProgress::Failed(message) => {
                self.login.error = Some(message.clone());
                self.login.hint = String::new();
                self.signing_in = false;
                self.toasts.push(Toast {
                    text: message,
                    error: true,
                });
            }
            other => {
                self.login.hint = other.hint();
            }
        }
    }

    // ----- action application ----------------------------------------------

    /// Apply every queued action (called after the frame is drawn).
    fn apply_actions(&mut self) {
        // `dispatch` may itself queue an action (e.g. selecting a chat), so loop
        // until the queue is empty.
        while !self.actions.is_empty() {
            let actions = std::mem::take(&mut self.actions);
            for action in actions {
                self.applied.push(action.clone());
                if self.applied.len() > 512 {
                    self.applied.drain(0..256);
                }
                self.dispatch(action);
            }
        }
    }

    fn dispatch(&mut self, action: Action) {
        match action {
            Action::BeginSignIn { use_personal } => {
                self.login.error = None;
                self.login.user_code = None;
                self.login.verification_uri = None;
                self.login.hint = "Requesting device code…".into();
                let handle = self.runtime.handle();
                let result = {
                    let _guard = handle.enter();
                    self.session.begin_sign_in(use_personal)
                };
                match result {
                    Ok(()) => self.signing_in = true,
                    Err(e) => {
                        self.login.error = Some(e.to_string());
                        self.push_error(e);
                    }
                }
            }
            Action::SignOut => self.sign_out(),
            Action::SetPage(page) => {
                self.page = page;
                match page {
                    Page::Teams => self.refresh_teams(),
                    Page::Files => {
                        if let Some(chat_id) = self.selected_chat.clone() {
                            self.refresh_files(chat_id);
                        }
                    }
                    Page::Calls => {
                        self.reload_call_log_from_store();
                    }
                    _ => {}
                }
            }
            Action::SelectChat(chat_id) => {
                self.select_chat(chat_id);
                self.page = Page::Chats;
            }
            Action::LoadOlder(chat_id) => self.load_older(chat_id),
            Action::SendMessage { chat_id, text } => {
                self.drafts.insert(chat_id.clone(), String::new());
                self.send_message(chat_id, text);
            }
            Action::RetryMessage {
                chat_id,
                client_message_id,
            } => self.retry_message(chat_id, client_message_id),
            Action::Attach { chat_id } => {
                // The upload path exists (`Files::upload`); the picker is a
                // documented follow-up (B-012).
                self.toasts.push(Toast {
                    text: format!("Attachment picker is not wired yet (chat {chat_id})."),
                    error: false,
                });
            }
            Action::RefreshChats => self.refresh_chats(),
            Action::RefreshTeams => self.refresh_teams(),
            Action::ToggleTeam(team_id) => self.toggle_team(team_id),
            Action::RefreshFiles(chat_id) => self.refresh_files(chat_id),
            Action::DownloadFile { chat_id, file_id } => self.download_file(chat_id, file_id),
            Action::PlaceCall {
                thread_id,
                callee,
                video,
            } => self.place_call(thread_id, callee, video),
            Action::AcceptCall => self.accept_call(),
            Action::DeclineCall => self.decline_call(),
            Action::EndCall => self.end_call(),
            Action::MarkCallConnected => {
                if let Some(calls) = &self.calls_svc {
                    let _ = calls.mark_connected();
                    self.call_state = calls.state();
                }
            }
            Action::ResetCall => {
                if let Some(calls) = &self.calls_svc {
                    calls.reset();
                    self.call_state = calls.state();
                }
                self.incoming = None;
            }
            Action::SetTheme(theme) => {
                self.settings.theme = theme;
                self.palette = theme::install(&self.ctx, theme);
                if let Err(e) = self.settings.save(&self.dirs) {
                    self.push_error(e);
                }
            }
            Action::DismissToasts => self.toasts.clear(),
        }
    }

    fn sign_out(&mut self) {
        if let Err(e) = self.session.sign_out() {
            self.push_error(e);
        }
        self.signed_in = false;
        self.signing_in = false;
        self.login = LoginView::default();
        self.messaging = None;
        self.teams_svc = None;
        self.files_svc = None;
        self.calls_svc = None;
        self.realtime_started = false;
        self.chats.clear();
        self.timeline.clear();
        self.selected_chat = None;
        self.has_older = false;
        self.drafts.clear();
        self.teams.clear();
        self.expanded_teams.clear();
        self.channels.clear();
        self.files.clear();
        self.call_state = CallState::Idle;
        self.incoming = None;
        self.page = Page::Chats;
    }
}

impl eframe::App for SquadsApp {
    /// Drain backend events and apply them. Requests a repaint only when at
    /// least one event was applied (ADR-027); while a device-code sign-in is
    /// running it polls on a short timer (the sign-in task reports over a std
    /// channel that cannot wake the UI).
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let events = self.runtime.drain();
        let mut changed = !events.is_empty();
        for event in events {
            tracing::trace!(kind = event.kind(), "app event");
            self.apply_event(event);
        }

        if self.signing_in {
            if let Some(progress) = self.session.poll_sign_in() {
                self.apply_event(AppEvent::SignInProgress(progress));
                changed = true;
            }
            if self.signing_in {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }

        if changed {
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Panels must be added before the central content, so the status bar
        // comes first.
        self.show_status_bar(ui);
        if self.signed_in {
            views::shell::show(self, ui);
        } else {
            views::login::show(self, ui);
        }
        self.apply_actions();
    }
}

impl SquadsApp {
    /// The bottom status bar: session state, the newest toast, a dismiss.
    fn show_status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let (text, color) = self.status();
                ui.colored_label(color, text);
                if let Some(toast) = self.toasts.last() {
                    let color = if toast.error {
                        self.palette.failed
                    } else {
                        ui.visuals().weak_text_color()
                    };
                    ui.separator();
                    ui.colored_label(color, &toast.text);
                }
                let has_toasts = !self.toasts.is_empty();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if has_toasts && ui.button("Dismiss").clicked() {
                        self.push_action(Action::DismissToasts);
                    }
                });
            });
        });
    }

    fn status(&self) -> (&str, egui::Color32) {
        if self.signing_in {
            ("Signing in…", self.palette.accent)
        } else if self.signed_in {
            ("Ready", self.palette.presence_online)
        } else {
            ("Signed out", self.palette.pending)
        }
    }
}

/// A short `HH:MM` from an ISO-8601 timestamp (falls back to the raw string).
pub fn short_time(timestamp: &str) -> String {
    if let Some((_, rest)) = timestamp.split_once('T')
        && rest.len() >= 5
    {
        return rest[..5].to_string();
    }
    timestamp.to_string()
}

/// A short `YYYY-MM-DD` from an ISO-8601 timestamp.
pub fn short_date(timestamp: &str) -> String {
    timestamp.split('T').next().unwrap_or(timestamp).to_string()
}
