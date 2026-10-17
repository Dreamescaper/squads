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

use squads_core::api::JoinTarget;
use squads_core::model::{Channel, Meeting, Message, SendState};
use squads_core::{
    Accent, AppDirs, Calendar, CallIdentity, CallLogEntry, CallService, CallState, Chat,
    ContactPresence, FileEntry, Files, IncomingCall, Messaging, NotificationContext, RealtimeEvent,
    Session, Settings, SignInProgress, Store, Team, Teams, Theme,
};

use crate::runtime::{AppEvent, AppRuntime};
use crate::shortcuts::{self, Shortcut};
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
    /// Calendar: upcoming meetings, week view and scheduling.
    Calendar,
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
            Page::Calendar => "Calendar",
            Page::Teams => "Teams",
            Page::Files => "Files",
            Page::Calls => "Calls",
            Page::Settings => "Settings",
        }
    }

    /// Every page, in nav-rail order (also the `Ctrl/Cmd+1`…`5` slot order;
    /// Settings has its own `Ctrl/Cmd+,`).
    pub fn all() -> [Page; 6] {
        [
            Page::Chats,
            Page::Calendar,
            Page::Teams,
            Page::Files,
            Page::Calls,
            Page::Settings,
        ]
    }

    /// The page a numbered shortcut slot selects (`Ctrl/Cmd+1`…`5`), or `None`
    /// for an unbound slot.
    pub fn from_slot(slot: u8) -> Option<Page> {
        match slot {
            1 => Some(Page::Chats),
            2 => Some(Page::Calendar),
            3 => Some(Page::Teams),
            4 => Some(Page::Files),
            5 => Some(Page::Calls),
            _ => None,
        }
    }
}

/// A user intent produced by a view and applied after the frame is drawn.
///
/// This is the "Action" half of UI → Action → apply: a widget records what the
/// user asked for; [`SquadsApp::apply_actions`] turns it into a backend call or
/// a state change. Keeping them separate means a view never mutates shared
/// state mid-layout and a test can inspect the queue.
#[derive(Debug, Clone, PartialEq)]
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
    /// Refresh the upcoming meetings from the calendar service (M12).
    RefreshCalendar,
    /// Refresh the calendar **week** view (M12): the next 7 days.
    RefreshCalendarWeek,
    /// Join a meeting from a pasted URL or a typed numeric meeting id (M12).
    JoinMeeting {
        /// The join URL or bare meeting id.
        target: String,
        /// Optional passcode.
        passcode: String,
    },
    /// Schedule a new meeting (M12).
    ScheduleMeeting {
        /// Meeting subject.
        subject: String,
        /// Graph start datetime (`YYYY-MM-DDTHH:MM:SS`).
        start: String,
        /// Graph end datetime.
        end: String,
        /// IANA/Windows timezone name.
        time_zone: String,
    },
    /// Cancel one meeting by its Graph event id (M12).
    CancelMeeting {
        /// Graph event id.
        event_id: String,
    },
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
    /// Switch the accent colour.
    SetAccent(Accent),
    /// Set the UI font scale.
    SetFontScale(f32),
    /// Toggle compact density.
    SetCompactMode(bool),
    /// Toggle notification delivery.
    SetNotificationsEnabled(bool),
    /// Toggle "only mentions notify".
    SetNotifyOnlyMentions(bool),
    /// Set the download directory (empty string clears it to the default).
    SetDownloadDir(String),
    /// Open/close the quick chat switcher (`Ctrl/Cmd+K`).
    ToggleQuickSwitcher,
    /// Show the in-conversation message search (`Ctrl/Cmd+F`).
    FocusTimelineSearch,
    /// Close overlays and clear the active search (`Esc`).
    CloseOverlays,
    /// Jump to the settings page (`Ctrl/Cmd+,`).
    OpenSettings,
    /// Select the next chat (`j` / `Down`).
    NextChat,
    /// Select the previous chat (`k` / `Up`).
    PrevChat,
    /// Send the current draft of the selected conversation (`Ctrl/Cmd+Enter`).
    SendDraft,
    /// Mute/unmute — a placeholder with no media device (`Ctrl/Cmd+Shift+M`).
    ToggleMute,
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
    calendar_svc: Option<Arc<Calendar>>,

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
    /// Quick switcher overlay: whether it is open, the typed query, and a
    /// one-shot focus request for its input.
    quick_switcher_open: bool,
    quick_switcher_query: String,
    focus_quick_switcher: bool,
    /// In-conversation search: whether the search bar is shown, the filter, and
    /// a one-shot focus request for its input.
    timeline_search_open: bool,
    timeline_filter: String,
    focus_timeline_search: bool,
    /// App-level mute placeholder (there is no media device in this build).
    muted: bool,
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

    /// Calendar (M12): the cached upcoming meetings, whether a fetch is in
    /// flight, the last resolved join target, the agenda/week toggle, and the
    /// "new meeting" form fields.
    meetings: Vec<Meeting>,
    calendar_loading: bool,
    join_target: Option<JoinTarget>,
    calendar_week: bool,
    new_meeting_subject: String,
    new_meeting_start: String,
    new_meeting_end: String,
    new_meeting_timezone: String,
    /// Free-form "join by link or meeting id" inputs.
    join_input: String,
    join_passcode: String,

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
        app.reload_meetings_from_store();

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
        app.reload_meetings_from_store();
        app.signed_in = app.session.state().is_ready();
        Ok(app)
    }

    fn build(
        ctx: egui::Context,
        dirs: Arc<AppDirs>,
        session: Session,
        settings: Settings,
    ) -> anyhow::Result<Self> {
        let palette = theme::install(&ctx, &settings);
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
            calendar_svc: None,
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
            quick_switcher_open: false,
            quick_switcher_query: String::new(),
            focus_quick_switcher: false,
            timeline_search_open: false,
            timeline_filter: String::new(),
            focus_timeline_search: false,
            muted: false,
            unread: HashMap::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
            teams: Vec::new(),
            expanded_teams: HashSet::new(),
            channels: HashMap::new(),
            files: Vec::new(),
            meetings: Vec::new(),
            calendar_loading: false,
            join_target: None,
            calendar_week: false,
            new_meeting_subject: String::new(),
            new_meeting_start: String::new(),
            new_meeting_end: String::new(),
            new_meeting_timezone: "UTC".to_string(),
            join_input: String::new(),
            join_passcode: String::new(),
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

    /// The upcoming meetings (cached + fetched), soonest first.
    pub fn meetings(&self) -> &[Meeting] {
        &self.meetings
    }

    /// Whether a calendar fetch is in flight (drives the loading state).
    pub fn calendar_loading(&self) -> bool {
        self.calendar_loading
    }

    /// The last resolved join target, if any (shown when it cannot be dialled).
    pub fn join_target(&self) -> Option<&JoinTarget> {
        self.join_target.as_ref()
    }

    /// Whether the calendar is showing the week view (else the agenda).
    pub fn calendar_week(&self) -> bool {
        self.calendar_week
    }

    /// The "new meeting" form's subject field.
    pub fn new_meeting_subject(&self) -> &str {
        &self.new_meeting_subject
    }

    /// The "new meeting" form's start field.
    pub fn new_meeting_start(&self) -> &str {
        &self.new_meeting_start
    }

    /// The "new meeting" form's end field.
    pub fn new_meeting_end(&self) -> &str {
        &self.new_meeting_end
    }

    /// The "new meeting" form's timezone field.
    pub fn new_meeting_timezone(&self) -> &str {
        &self.new_meeting_timezone
    }

    /// Mutable subject field (the form edits it directly).
    pub fn new_meeting_subject_mut(&mut self) -> &mut String {
        &mut self.new_meeting_subject
    }

    /// Mutable start field.
    pub fn new_meeting_start_mut(&mut self) -> &mut String {
        &mut self.new_meeting_start
    }

    /// Mutable end field.
    pub fn new_meeting_end_mut(&mut self) -> &mut String {
        &mut self.new_meeting_end
    }

    /// Mutable timezone field.
    pub fn new_meeting_timezone_mut(&mut self) -> &mut String {
        &mut self.new_meeting_timezone
    }

    /// The free-form "join by link or meeting id" input.
    pub fn join_input(&self) -> &str {
        &self.join_input
    }

    /// The free-form join passcode input.
    pub fn join_passcode(&self) -> &str {
        &self.join_passcode
    }

    /// Mutable join input.
    pub fn join_input_mut(&mut self) -> &mut String {
        &mut self.join_input
    }

    /// Mutable join passcode input.
    pub fn join_passcode_mut(&mut self) -> &mut String {
        &mut self.join_passcode
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

    // ----- M11: shortcuts, overlays and preferences --------------------------

    /// Whether the quick switcher overlay is open.
    pub fn quick_switcher_open(&self) -> bool {
        self.quick_switcher_open
    }

    /// The quick switcher's typed query.
    pub fn quick_switcher_query(&self) -> &str {
        &self.quick_switcher_query
    }

    /// Set the quick switcher's query.
    pub fn set_quick_switcher_query(&mut self, text: String) {
        self.quick_switcher_query = text;
    }

    /// Take the one-shot quick-switcher focus request.
    pub fn take_quick_switcher_focus(&mut self) -> bool {
        std::mem::take(&mut self.focus_quick_switcher)
    }

    /// Whether the in-conversation search bar is shown.
    pub fn timeline_search_open(&self) -> bool {
        self.timeline_search_open
    }

    /// The in-conversation message filter.
    pub fn timeline_filter(&self) -> &str {
        &self.timeline_filter
    }

    /// Set the in-conversation message filter.
    pub fn set_timeline_filter(&mut self, text: String) {
        self.timeline_filter = text;
    }

    /// Take the one-shot timeline-search focus request.
    pub fn take_timeline_search_focus(&mut self) -> bool {
        std::mem::take(&mut self.focus_timeline_search)
    }

    /// Whether the app-level mute placeholder is engaged.
    #[allow(dead_code)] // asserted by the headless tests
    pub fn muted(&self) -> bool {
        self.muted
    }

    /// Whether compact density is enabled (drives the timeline row height).
    pub fn compact_mode(&self) -> bool {
        self.settings.compact_mode
    }

    /// The timeline row height for the current density.
    pub fn row_height(&self) -> f32 {
        if self.settings.compact_mode {
            56.0
        } else {
            78.0
        }
    }

    /// The accent colour choice.
    pub fn accent(&self) -> Accent {
        self.settings.accent
    }

    /// The UI font scale.
    pub fn font_scale(&self) -> f32 {
        self.settings.effective_font_scale()
    }

    /// Whether notifications are enabled.
    pub fn notifications_enabled(&self) -> bool {
        self.settings.notifications_enabled
    }

    /// Whether only mentions notify.
    pub fn notify_only_mentions(&self) -> bool {
        self.settings.notify_only_mentions
    }

    /// The configured download directory (as a string; empty means the default).
    pub fn download_dir(&self) -> String {
        self.settings
            .download_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Where a download should be written: the configured directory when set,
    /// else the app's media directory.
    pub fn download_target(&self) -> std::path::PathBuf {
        self.settings
            .download_dir
            .clone()
            .unwrap_or_else(|| self.dirs.media_dir())
    }

    /// The [`NotificationContext`] the app routes realtime messages with,
    /// derived from the persisted notification settings, our own MRI (decoded
    /// from the skype token when available) and the store's muted chats.
    pub(crate) fn notification_context(&self) -> NotificationContext {
        let mut ctx = NotificationContext::new()
            .with_notifications_enabled(self.settings.notifications_enabled)
            .with_notify_only_mentions(self.settings.notify_only_mentions);
        if let Some(tokens) = self.session.tokens()
            && let Some(identity) = CallIdentity::from_skype_token(tokens.skype_token())
        {
            ctx = ctx.with_self_mri(identity.mri);
        }
        if let Ok(muted) = self.store.muted_chats(&self.profile) {
            ctx = ctx.mute_all(muted);
        }
        ctx
    }

    /// The mutable draft for a conversation (creates an empty one on demand).
    pub fn draft_mut(&mut self, chat_id: &str) -> &mut String {
        self.drafts.entry(chat_id.to_string()).or_default()
    }

    /// Queue an action (called by the views).
    pub fn push_action(&mut self, action: Action) {
        self.actions.push(action);
    }

    // ----- M11: shortcut input + dispatch -----------------------------------

    /// Read egui's raw key events and dispatch every bound shortcut (M11).
    ///
    /// The mapping is the pure [`shortcuts::resolve`]; this method owns the
    /// dispatch and the "is a text field focused?" guard so a plain `j`/`k`
    /// never hijacks typing in the composer or a search box.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if !self.signed_in {
            return;
        }
        let events: Vec<(egui::Key, egui::Modifiers)> = ctx.input(|input| {
            input
                .events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect()
        });
        let text_focused = ctx.egui_wants_keyboard_input();
        for (key, modifiers) in events {
            let Some(shortcut) = shortcuts::resolve(key, modifiers) else {
                continue;
            };
            if text_focused
                && matches!(
                    shortcut,
                    Shortcut::NextChat | Shortcut::PrevChat | Shortcut::Send
                )
            {
                continue;
            }
            self.apply_shortcut(shortcut);
        }
    }

    /// Turn a resolved [`Shortcut`] into a queued [`Action`] — the dispatch half
    /// of the M11 shortcut design (the mapping lives in [`crate::shortcuts`]).
    pub(crate) fn apply_shortcut(&mut self, shortcut: Shortcut) {
        tracing::trace!(
            shortcut = shortcut.label(),
            placeholder = shortcut.is_placeholder(),
            "keyboard shortcut"
        );
        match shortcut {
            Shortcut::QuickSwitcher => self.push_action(Action::ToggleQuickSwitcher),
            Shortcut::SearchInChat => self.push_action(Action::FocusTimelineSearch),
            Shortcut::CloseOverlay => self.push_action(Action::CloseOverlays),
            Shortcut::OpenSettings => self.push_action(Action::OpenSettings),
            Shortcut::ToggleMute => self.push_action(Action::ToggleMute),
            Shortcut::NextChat => self.push_action(Action::NextChat),
            Shortcut::PrevChat => self.push_action(Action::PrevChat),
            Shortcut::Send => self.push_action(Action::SendDraft),
            Shortcut::PageSlot(slot) => {
                if let Some(page) = Page::from_slot(slot) {
                    self.push_action(Action::SetPage(page));
                }
            }
        }
    }

    /// Select the chat `delta` rows from the current selection (clamped to the
    /// list; an unselected list starts at the first/last row).
    fn select_relative_chat(&mut self, delta: i32) {
        if self.chats.is_empty() {
            return;
        }
        let current = self
            .selected_chat
            .as_ref()
            .and_then(|id| self.chats.iter().position(|chat| &chat.id == id));
        let len = self.chats.len() as i32;
        let next = match current {
            Some(index) => (index as i32 + delta).clamp(0, len - 1) as usize,
            None if delta >= 0 => 0,
            None => self.chats.len() - 1,
        };
        let id = self.chats[next].id.clone();
        self.push_action(Action::SelectChat(id));
    }

    /// Re-install the theme from the current settings (after a preference
    /// change) and persist them.
    fn reinstall_theme(&mut self) {
        self.palette = theme::install(&self.ctx, &self.settings);
        self.persist_settings();
    }

    /// Persist the current settings, surfacing a write error as a toast.
    fn persist_settings(&mut self) {
        if let Err(e) = self.settings.save(&self.dirs) {
            self.push_error(e);
        }
    }

    /// Route a realtime event to a notification using the persisted settings
    /// (M11 wiring). Delivery goes through the session's notifier — a
    /// [`LogNotifier`](squads_core::LogNotifier) by default, a desktop notifier
    /// under the default-off feature.
    fn notify_realtime(&self, event: &RealtimeEvent) {
        if !self.settings.notifications_enabled {
            return;
        }
        let ctx = self.notification_context();
        let _ = self.session.notifications().notify_event(event, &ctx);
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
        if self.calendar_svc.is_none() {
            match self.session.calendar() {
                Ok(service) => self.calendar_svc = Some(Arc::new(service)),
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
        self.reload_meetings_from_store();
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

    /// Paint the calendar from the store's cached meetings (offline).
    fn reload_meetings_from_store(&mut self) {
        match self.store.upcoming_meetings(&self.profile, 0, 100) {
            Ok(meetings) => self.meetings = meetings,
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
        let dest = self.download_target().join(&entry.name);
        let dest_path = dest.to_string_lossy().into_owned();
        self.runtime.spawn(async move {
            match files.download(&entry, &dest_path, |_, _| {}).await {
                Ok(_) => AppEvent::Error(format!("downloaded {}", dest.display())),
                Err(e) => AppEvent::Error(format!("download: {e}")),
            }
        });
    }

    /// Refresh the upcoming meetings: paint the cache, then fetch (M12).
    fn refresh_calendar(&mut self) {
        self.calendar_week = false;
        self.reload_meetings_from_store();
        let Some(calendar) = self.calendar_svc.clone() else {
            return;
        };
        self.calendar_loading = true;
        self.runtime.spawn(async move {
            match calendar.upcoming(50).await {
                Ok(meetings) => AppEvent::MeetingsLoaded(meetings),
                Err(e) => AppEvent::Error(format!("calendar: {e}")),
            }
        });
    }

    /// Refresh the calendar week view (the next 7 days from now) (M12).
    fn refresh_calendar_week(&mut self) {
        self.calendar_week = true;
        let Some(calendar) = self.calendar_svc.clone() else {
            return;
        };
        self.calendar_loading = true;
        let start = squads_core::auth::tokens::now_unix();
        self.runtime.spawn(async move {
            match calendar.week(start, 7, 100).await {
                Ok(meetings) => AppEvent::MeetingsLoaded(meetings),
                Err(e) => AppEvent::Error(format!("calendar week: {e}")),
            }
        });
    }

    /// Resolve a pasted join string or typed meeting id (M12). The result is
    /// applied as [`AppEvent::JoinResolved`], which hands a `thread` target to
    /// the call path and shows every other target.
    fn join_meeting(&mut self, target: String, passcode: String) {
        let Some(calendar) = self.calendar_svc.clone() else {
            self.push_error("joining a meeting requires sign-in");
            return;
        };
        self.runtime.spawn(async move {
            match calendar.resolve_join(&target, &passcode).await {
                Ok(resolved) => AppEvent::JoinResolved(resolved),
                Err(e) => AppEvent::Error(format!("join: {e}")),
            }
        });
    }

    /// Schedule a new meeting (M12).
    fn schedule_meeting(&mut self, subject: String, start: String, end: String, time_zone: String) {
        let Some(calendar) = self.calendar_svc.clone() else {
            self.push_error("scheduling a meeting requires sign-in");
            return;
        };
        self.runtime.spawn(async move {
            match calendar.schedule(&subject, &start, &end, &time_zone).await {
                Ok(meeting) => AppEvent::MeetingScheduled(meeting),
                Err(e) => AppEvent::Error(format!("schedule: {e}")),
            }
        });
    }

    /// Cancel one meeting (M12).
    fn cancel_meeting(&mut self, event_id: String) {
        let Some(calendar) = self.calendar_svc.clone() else {
            self.push_error("cancelling a meeting requires sign-in");
            return;
        };
        self.runtime.spawn(async move {
            match calendar.cancel(&event_id).await {
                Ok(()) => AppEvent::MeetingCancelled(event_id),
                Err(e) => AppEvent::Error(format!("cancel: {e}")),
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
            AppEvent::MeetingsLoaded(meetings) => {
                if let Err(e) = self.store.upsert_meetings(&self.profile, &meetings) {
                    self.push_error(e);
                }
                self.calendar_loading = false;
                self.reload_meetings_from_store();
            }
            AppEvent::JoinResolved(target) => {
                let thread_id = target.thread_id.clone();
                let is_thread = target.kind == "thread";
                let url = target.url.clone();
                self.join_target = Some(target);
                if is_thread {
                    if let Some(thread_id) = thread_id {
                        // A native thread target dials through the existing call
                        // path (ADR-033).
                        self.place_call(thread_id, None, false);
                    }
                } else {
                    // Anything else cannot start a call: surface the resolved
                    // target for the user to open.
                    self.toasts.push(Toast {
                        text: format!("Join target: {url}"),
                        error: false,
                    });
                }
            }
            AppEvent::MeetingScheduled(meeting) => {
                self.reload_meetings_from_store();
                self.toasts.push(Toast {
                    text: format!("Scheduled \"{}\"", meeting.subject),
                    error: false,
                });
            }
            AppEvent::MeetingCancelled(event_id) => {
                self.reload_meetings_from_store();
                self.toasts.push(Toast {
                    text: format!("Cancelled meeting {event_id}"),
                    error: false,
                });
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
                self.notify_realtime(&event);
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
                    Page::Calendar => self.refresh_calendar(),
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
            Action::RefreshCalendar => self.refresh_calendar(),
            Action::RefreshCalendarWeek => self.refresh_calendar_week(),
            Action::JoinMeeting { target, passcode } => self.join_meeting(target, passcode),
            Action::ScheduleMeeting {
                subject,
                start,
                end,
                time_zone,
            } => self.schedule_meeting(subject, start, end, time_zone),
            Action::CancelMeeting { event_id } => self.cancel_meeting(event_id),
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
                self.reinstall_theme();
            }
            Action::SetAccent(accent) => {
                self.settings.accent = accent;
                self.reinstall_theme();
            }
            Action::SetFontScale(scale) => {
                self.settings.font_scale = scale.clamp(
                    squads_core::settings::FONT_SCALE_MIN,
                    squads_core::settings::FONT_SCALE_MAX,
                );
                self.reinstall_theme();
            }
            Action::SetCompactMode(compact) => {
                self.settings.compact_mode = compact;
                self.reinstall_theme();
            }
            Action::SetNotificationsEnabled(enabled) => {
                self.settings.notifications_enabled = enabled;
                self.persist_settings();
            }
            Action::SetNotifyOnlyMentions(only) => {
                self.settings.notify_only_mentions = only;
                self.persist_settings();
            }
            Action::SetDownloadDir(path) => {
                let trimmed = path.trim();
                self.settings.download_dir = if trimmed.is_empty() {
                    None
                } else {
                    Some(std::path::PathBuf::from(trimmed))
                };
                self.persist_settings();
            }
            Action::ToggleQuickSwitcher => {
                self.quick_switcher_open = !self.quick_switcher_open;
                if self.quick_switcher_open {
                    self.focus_quick_switcher = true;
                } else {
                    self.quick_switcher_query.clear();
                }
            }
            Action::FocusTimelineSearch => {
                self.timeline_search_open = true;
                self.focus_timeline_search = true;
                self.page = Page::Chats;
            }
            Action::CloseOverlays => {
                self.quick_switcher_open = false;
                self.quick_switcher_query.clear();
                self.timeline_search_open = false;
                self.timeline_filter.clear();
                self.search.clear();
                self.show_info = false;
            }
            Action::OpenSettings => self.dispatch(Action::SetPage(Page::Settings)),
            Action::NextChat => self.select_relative_chat(1),
            Action::PrevChat => self.select_relative_chat(-1),
            Action::SendDraft => {
                if let Some(chat_id) = self.selected_chat.clone() {
                    let text = self.draft(&chat_id).trim().to_string();
                    if !text.is_empty() {
                        self.drafts.insert(chat_id.clone(), String::new());
                        // Reuse the normal send path (and its applied log).
                        self.push_action(Action::SendMessage { chat_id, text });
                    }
                }
            }
            Action::ToggleMute => {
                self.muted = !self.muted;
                self.toasts.push(Toast {
                    text: if self.muted {
                        "Muted (placeholder — no media device in this build).".into()
                    } else {
                        "Unmuted (placeholder).".into()
                    },
                    error: false,
                });
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
        self.calendar_svc = None;
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
        self.meetings.clear();
        self.calendar_loading = false;
        self.join_target = None;
        self.calendar_week = false;
        self.join_input.clear();
        self.join_passcode.clear();
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
        // Shortcuts are read from the raw input first, so a shortcut queued this
        // frame is applied at the end of it.
        self.handle_shortcuts(ui.ctx());
        // Panels must be added before the central content, so the status bar
        // comes first.
        self.show_status_bar(ui);
        if self.signed_in {
            views::shell::show(self, ui);
        } else {
            views::login::show(self, ui);
        }
        // The quick switcher is a floating overlay, drawn after the shell.
        views::quick_switcher::show(self, ui.ctx());
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
