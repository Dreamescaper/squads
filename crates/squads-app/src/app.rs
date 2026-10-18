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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use squads_core::api::JoinTarget;
use squads_core::model::{
    Channel, Identity, Meeting, Message, Person, PublicTeam, Recording, SearchHit, SendState,
    TeamMember, Transcript,
};
use squads_core::{
    Accent, AppDirs, Calendar, CallIdentity, CallLogEntry, CallService, CallState, Chat,
    ChatFolder, Chats, ContactPresence, FileEntry, Files, Folders, IncomingCall, MediaCache,
    Messaging, NotificationContext, People, PinnedMessage, Pins, RealtimeEvent, Recordings, Search,
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
/// How many global drive file-search hits to request (Graph caps at 25).
pub const FILE_SEARCH_LIMIT: usize = 25;
/// How many directory people to request for the picker (Graph caps at 25).
pub const PEOPLE_LIMIT: usize = 25;
/// How many recordings/transcripts to request per conversation (B5).
pub const RECORDINGS_LIMIT: usize = 50;

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
    /// Search: local-first message search (backlog item B1).
    Search,
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
            Page::Search => "Search",
            Page::Settings => "Settings",
        }
    }

    /// Every page, in nav-rail order (also the `Ctrl/Cmd+1`…`5` slot order;
    /// Search and Settings have their own bindings).
    pub fn all() -> [Page; 7] {
        [
            Page::Chats,
            Page::Calendar,
            Page::Teams,
            Page::Files,
            Page::Calls,
            Page::Search,
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
    /// Run a global drive file search for `query` (B3).
    SearchFiles(String),
    /// Run a directory people search for `query` (B3) — the people picker.
    SearchPeople(String),
    /// Add or remove a [`Person`] from the people-picker selection (B3).
    TogglePersonSelected(Person),
    /// Clear the people-picker selection (B3).
    ClearPeopleSelection,
    /// Open/close the reusable people-picker overlay (B3).
    TogglePeoplePicker,
    /// Open the people picker in **"new chat"** mode (B2a): picking one person
    /// creates a 1:1, several create a group.
    NewChat,
    /// Create the chat from the current people-picker selection (B2a).
    CreateChat,
    /// Open/close a chat's context menu (B2a).
    ToggleChatMenu(String),
    /// Mute/unmute one chat (B2a): the local routing flag plus the server write.
    ToggleChatMuted(String),
    /// Hide/unhide one chat (B2a): the local flag plus the server write.
    ToggleChatHidden(String),
    /// Ask to leave a chat (opens the confirmation) (B2a).
    RequestLeaveChat(String),
    /// Confirm leaving a chat (B2a).
    ConfirmLeaveChat(String),
    /// Cancel a pending leave (B2a).
    CancelLeaveChat,
    /// Show/hide the collapsed hidden-chats section (B2a).
    ToggleShowHidden,
    /// Refresh the chat folders from the service (B2b).
    RefreshFolders,
    /// Filter the chat list to one folder (B2b). An empty id selects "All".
    SelectFolder(String),
    /// Move the selected chat into a folder (B2b); a blank folder clears it.
    MoveChatToFolder {
        /// The chat to move.
        chat_id: String,
        /// The target folder id (blank = out of every folder).
        folder_id: String,
    },
    /// Refresh the pinned messages of the selected chat (B2b).
    RefreshPins,
    /// Unpin one message from the selected chat (B2b).
    UnpinMessage {
        /// Owning conversation.
        chat_id: String,
        /// The pinned message id.
        message_id: String,
    },
    /// Search the directory for public (joinable) teams (B4).
    SearchPublicTeams(String),
    /// Join a public team (B4).
    JoinTeam(String),
    /// Refresh one team's roster (B4).
    RefreshTeamMembers(String),
    /// Open the people picker in **"add team member"** mode (B4).
    OpenAddTeamMember(String),
    /// Add one person to a team (B4). `user_id` is the Graph user id.
    AddTeamMember {
        /// Owning team.
        team_id: String,
        /// The Graph/AAD user id to add.
        user_id: String,
        /// Grant the owner role.
        owner: bool,
    },
    /// Ask to remove a team membership (opens the confirmation) (B4).
    RequestRemoveTeamMember {
        /// Owning team.
        team_id: String,
        /// The roster membership id.
        membership_id: String,
    },
    /// Confirm removing a team membership (B4).
    ConfirmRemoveTeamMember {
        /// Owning team.
        team_id: String,
        /// The roster membership id.
        membership_id: String,
    },
    /// Cancel a pending member removal (B4).
    CancelRemoveTeamMember,
    /// Add or remove a reaction on a **channel** message (B4).
    ReactInChannel {
        /// The conversation the message is stored under (channel or thread).
        conversation_id: String,
        /// The message (or reply) id.
        message_id: String,
        /// The picker emoji.
        emoji: String,
        /// Remove instead of add.
        remove: bool,
        /// The message is a thread reply.
        is_reply: bool,
    },
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
    /// Refresh the selected conversation's recordings/transcripts (B5).
    RefreshRecordings,
    /// Download one recording to the local download directory (B5).
    DownloadRecording {
        /// The recording's driveItem id.
        id: String,
    },
    /// Download one transcript to the local download directory (B5).
    DownloadTranscript {
        /// The transcript's driveItem id.
        id: String,
    },
    /// Fetch and cache one inline media URL (M13).
    LoadMedia {
        /// The image URL extracted from a message body.
        url: String,
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
    /// Run a local-first message search for `query` (B1).
    SearchMessages(String),
    /// Move the search-result selection down (B1).
    NextSearchResult,
    /// Move the search-result selection up (B1).
    PrevSearchResult,
    /// Open a search result's chat and highlight the matched message (B1).
    OpenSearchHit {
        /// Owning conversation.
        chat_id: String,
        /// The matched message id.
        message_id: String,
    },
    /// Clear the transient toasts.
    DismissToasts,
}

/// A transient message shown at the bottom of the window.
#[derive(Debug, Clone)]
struct Toast {
    text: String,
    error: bool,
}

/// The load state of one inline media URL (M13).
///
/// The conversation view renders the cached image once [`MediaState::Loaded`],
/// a placeholder while `Loading`, and a small "unavailable" note on `Failed`.
/// An absent entry means the URL has not been requested yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaState {
    /// The fetch is in flight (or no cache is available, e.g. headless tests).
    Loading,
    /// The bytes are cached locally.
    Loaded(PathBuf),
    /// The fetch failed; the view shows a placeholder.
    Failed,
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
    /// The directory people-search service (B3), built once signed in.
    people_svc: Option<Arc<People>>,
    calls_svc: Option<Arc<CallService>>,
    calendar_svc: Option<Arc<Calendar>>,
    /// The inline-media cache (M13), built once signed in. `None` in headless
    /// mode, so the view shows placeholders instead of fetching.
    media_cache: Option<Arc<MediaCache>>,
    /// Per-URL inline-media load state (M13), keyed by URL.
    media: HashMap<String, MediaState>,

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

    /// Message search (B1): the local-first service handle (always built), the
    /// query, the results, the selected row, whether the optional server
    /// fallback is in flight, and a one-shot focus request for the query box.
    search_svc: Arc<Search>,
    search_query: String,
    search_results: Vec<SearchHit>,
    search_selected: usize,
    search_loading: bool,
    focus_search: bool,
    /// The message id to highlight after jumping from a search result (B1).
    highlight_message: Option<String>,

    teams: Vec<Team>,
    expanded_teams: HashSet<String>,
    channels: HashMap<String, Vec<Channel>>,

    files: Vec<FileEntry>,
    /// Global drive file search (B3): the query, the results, and whether a
    /// search is in flight. A non-empty query switches the Files page from the
    /// chat's files to the global results.
    file_search_query: String,
    file_search_results: Vec<FileEntry>,
    file_search_loading: bool,

    /// Reusable people picker (B3): the query, the async directory results, the
    /// multi-select set (what a caller reads to create a chat — B2a), whether a
    /// search is in flight, whether the overlay is open, and a one-shot focus
    /// request for its query box.
    people_query: String,
    people_results: Vec<Person>,
    people_selected: Vec<Person>,
    people_loading: bool,
    people_picker_open: bool,
    focus_people_search: bool,

    /// Chat management (B2a): the create/leave/mute/hide service handle, the
    /// local flag mirrors (recomputed with the chat list), the collapsed
    /// hidden-section toggle, the open context menu, the new-chat flow state,
    /// and the pending leave confirmation.
    chats_svc: Option<Arc<Chats>>,
    muted_ids: HashSet<String>,
    hidden_ids: HashSet<String>,
    show_hidden: bool,
    chat_menu: Option<String>,
    new_chat_open: bool,
    new_chat_topic: String,
    leave_confirm: Option<String>,

    /// Folders & pins (B2b): the folder/pin service handles, the cached
    /// folders, the selected folder tab (`None` = All), and the selected chat's
    /// cached pins.
    folders_svc: Option<Arc<Folders>>,
    pins_svc: Option<Arc<Pins>>,
    folders: Vec<ChatFolder>,
    selected_folder: Option<String>,
    pins: Vec<PinnedMessage>,

    /// Team administration (B4): the public-team search query + results, the
    /// per-team rosters, the open "add member" flow (which team + owner flag),
    /// and the pending member-removal confirmation.
    public_query: String,
    public_results: Vec<PublicTeam>,
    public_loading: bool,
    members: HashMap<String, Vec<TeamMember>>,
    add_member_team: Option<String>,
    add_member_owner: bool,
    remove_member_confirm: Option<(String, String)>,

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

    /// Recordings & transcripts (B5): the service handle, the selected
    /// conversation's cached rows, and whether a refresh is in flight.
    recordings_svc: Option<Arc<Recordings>>,
    recordings: Vec<Recording>,
    transcripts: Vec<Transcript>,
    recordings_loading: bool,

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
        // The local-first search handle is always available (even signed out);
        // `ensure_services` attaches the client for the optional server
        // fallback once tokens exist (B1).
        let search_svc = Arc::new(Search::new(profile.clone(), Arc::clone(&store)));
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
            people_svc: None,
            calls_svc: None,
            calendar_svc: None,
            media_cache: None,
            media: HashMap::new(),
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
            search_svc,
            search_query: String::new(),
            search_results: Vec::new(),
            search_selected: 0,
            search_loading: false,
            focus_search: false,
            highlight_message: None,
            teams: Vec::new(),
            expanded_teams: HashSet::new(),
            channels: HashMap::new(),
            files: Vec::new(),
            file_search_query: String::new(),
            file_search_results: Vec::new(),
            file_search_loading: false,
            people_query: String::new(),
            people_results: Vec::new(),
            people_selected: Vec::new(),
            people_loading: false,
            people_picker_open: false,
            focus_people_search: false,
            chats_svc: None,
            muted_ids: HashSet::new(),
            hidden_ids: HashSet::new(),
            show_hidden: false,
            chat_menu: None,
            new_chat_open: false,
            new_chat_topic: String::new(),
            leave_confirm: None,
            folders_svc: None,
            pins_svc: None,
            folders: Vec::new(),
            selected_folder: None,
            pins: Vec::new(),
            public_query: String::new(),
            public_results: Vec::new(),
            public_loading: false,
            members: HashMap::new(),
            add_member_team: None,
            add_member_owner: false,
            remove_member_confirm: None,
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
            recordings_svc: None,
            recordings: Vec::new(),
            transcripts: Vec::new(),
            recordings_loading: false,
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

    // ----- B3: file search & the reusable people picker ----------------------

    /// The global drive file-search query.
    pub fn file_search_query(&self) -> &str {
        &self.file_search_query
    }

    /// Mutable global drive file-search query (the Files view edits it).
    pub fn file_search_query_mut(&mut self) -> &mut String {
        &mut self.file_search_query
    }

    /// The global drive file-search results (wire order).
    pub fn file_search_results(&self) -> &[FileEntry] {
        &self.file_search_results
    }

    /// Whether a global drive file search is in flight.
    pub fn file_search_loading(&self) -> bool {
        self.file_search_loading
    }

    /// The people-picker query.
    pub fn people_query(&self) -> &str {
        &self.people_query
    }

    /// Mutable people-picker query (the picker edits it).
    pub fn people_query_mut(&mut self) -> &mut String {
        &mut self.people_query
    }

    /// The directory results currently shown by the people picker.
    pub fn people_results(&self) -> &[Person] {
        &self.people_results
    }

    /// The people picked so far — the value **B2a**'s "new chat" flow reads to
    /// create a 1:1 (one) or group (several) chat.
    pub fn selected_people(&self) -> &[Person] {
        &self.people_selected
    }

    /// Whether `id` is currently in the people-picker selection.
    pub fn is_person_selected(&self, id: &str) -> bool {
        self.people_selected.iter().any(|person| person.id == id)
    }

    /// Whether a directory people search is in flight.
    pub fn people_loading(&self) -> bool {
        self.people_loading
    }

    /// Whether the people-picker overlay is open.
    pub fn people_picker_open(&self) -> bool {
        self.people_picker_open
    }

    /// Take the one-shot people-picker focus request.
    pub fn take_people_focus(&mut self) -> bool {
        std::mem::take(&mut self.focus_people_search)
    }

    // ----- B2a: new chat & chat management ----------------------------------

    /// Whether the people picker is in "new chat" mode.
    pub fn new_chat_open(&self) -> bool {
        self.new_chat_open
    }

    /// The new-chat group topic.
    ///
    /// The picker edits it through [`new_chat_topic_mut`](SquadsApp::new_chat_topic_mut);
    /// the getter is used by the headless tests.
    #[cfg(test)]
    pub(crate) fn new_chat_topic(&self) -> &str {
        &self.new_chat_topic
    }

    /// Mutable new-chat group topic (the picker edits it).
    pub fn new_chat_topic_mut(&mut self) -> &mut String {
        &mut self.new_chat_topic
    }

    /// Whether one chat is muted (the local routing flag, M8/B2a).
    pub fn is_chat_muted(&self, chat_id: &str) -> bool {
        self.muted_ids.contains(chat_id)
    }

    /// Whether one chat is hidden (B2a).
    pub fn is_chat_hidden(&self, chat_id: &str) -> bool {
        self.hidden_ids.contains(chat_id)
    }

    /// Whether the collapsed hidden-chats section is shown.
    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    /// How many chats are hidden.
    pub fn hidden_count(&self) -> usize {
        self.hidden_ids.len()
    }

    /// The chat whose context menu is open, if any.
    pub fn chat_menu(&self) -> Option<&str> {
        self.chat_menu.as_deref()
    }

    /// The chat a leave is pending confirmation for, if any.
    pub fn leave_confirm(&self) -> Option<&str> {
        self.leave_confirm.as_deref()
    }

    /// The cached chat folders (B2b), in server order.
    pub fn folders(&self) -> &[ChatFolder] {
        &self.folders
    }

    /// The selected folder tab id, if any (`None` = "All") (B2b).
    pub fn selected_folder(&self) -> Option<&str> {
        self.selected_folder.as_deref()
    }

    /// How many chats are filed in `folder_id` (B2b).
    pub fn folder_count(&self, folder_id: &str) -> usize {
        self.folders
            .iter()
            .find(|folder| folder.id == folder_id)
            .map(|folder| folder.item_ids.len())
            .unwrap_or(0)
    }

    /// Whether `chat_id` is in the selected folder (or the selection is "All")
    /// (B2b).
    pub fn chat_in_selected_folder(&self, chat_id: &str) -> bool {
        match self.selected_folder.as_deref() {
            None => true,
            Some(folder_id) => self
                .folders
                .iter()
                .find(|folder| folder.id == folder_id)
                .map(|folder| folder.item_ids.iter().any(|id| id == chat_id))
                .unwrap_or(false),
        }
    }

    /// The selected chat's cached pinned messages (B2b).
    pub fn pins(&self) -> &[PinnedMessage] {
        &self.pins
    }

    // ----- B5: recordings & transcripts -------------------------------------

    /// The selected conversation's cached recordings, newest first (B5).
    pub fn recordings(&self) -> &[Recording] {
        &self.recordings
    }

    /// The selected conversation's cached transcripts, newest first (B5).
    pub fn transcripts(&self) -> &[Transcript] {
        &self.transcripts
    }

    /// Whether a recordings/transcripts refresh is in flight (B5).
    pub fn recordings_loading(&self) -> bool {
        self.recordings_loading
    }

    // ----- B4: team administration ------------------------------------------

    /// The public-team search query.
    pub fn public_query(&self) -> &str {
        &self.public_query
    }

    /// Mutable public-team search query (the Teams view edits it).
    pub fn public_query_mut(&mut self) -> &mut String {
        &mut self.public_query
    }

    /// The public-team search results.
    pub fn public_results(&self) -> &[PublicTeam] {
        &self.public_results
    }

    /// Whether a public-team search is in flight.
    pub fn public_loading(&self) -> bool {
        self.public_loading
    }

    /// The cached roster of one team (B4).
    pub fn members_for(&self, team_id: &str) -> &[TeamMember] {
        self.members.get(team_id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Whether the signed-in user holds the `owner` role on `team_id` (B4).
    ///
    /// Derived from the fetched roster: a member is "us" when its Graph
    /// `user_id` matches our identity's oid. A cached roster (which does not
    /// persist the user id) therefore reports `false` until a live fetch.
    pub fn is_owner_of(&self, team_id: &str) -> bool {
        let Some(oid) = self.session.identity().map(|identity| identity.oid.clone()) else {
            return false;
        };
        self.members_for(team_id)
            .iter()
            .any(|member| member.is_owner() && member.user_id.as_deref() == Some(oid.as_str()))
    }

    /// The team whose "add member" picker is open, if any (B4).
    pub fn add_member_team(&self) -> Option<&str> {
        self.add_member_team.as_deref()
    }

    /// Whether the pending member add grants the owner role (B4).
    pub fn add_member_owner(&self) -> bool {
        self.add_member_owner
    }

    /// Mutable owner-role flag for the add-member picker (B4).
    pub fn add_member_owner_mut(&mut self) -> &mut bool {
        &mut self.add_member_owner
    }

    /// The `(team_id, membership_id)` a removal is pending confirmation for
    /// (B4).
    pub fn remove_member_confirm(&self) -> Option<(&str, &str)> {
        self.remove_member_confirm
            .as_ref()
            .map(|(team, member)| (team.as_str(), member.as_str()))
    }

    /// Test-only: seed a team's roster (the headless harness has no service to
    /// fetch one).
    #[cfg(test)]
    pub(crate) fn set_team_members(&mut self, team_id: &str, members: Vec<TeamMember>) {
        self.members.insert(team_id.to_string(), members);
    }

    /// Test-only: seed the public-team query and results.
    #[cfg(test)]
    pub(crate) fn set_public_results(&mut self, query: &str, teams: Vec<PublicTeam>) {
        self.public_query = query.to_string();
        self.public_results = teams;
        self.public_loading = false;
    }

    /// Test-only: seed the global file-search query and results (the headless
    /// harness has no service to run a real search).
    #[cfg(test)]
    pub(crate) fn set_file_search(&mut self, query: &str, files: Vec<FileEntry>) {
        self.file_search_query = query.to_string();
        self.file_search_results = files;
        self.file_search_loading = false;
    }

    /// Test-only: seed the people-picker query and results.
    #[cfg(test)]
    pub(crate) fn set_people_results(&mut self, query: &str, people: Vec<Person>) {
        self.people_query = query.to_string();
        self.people_results = people;
        self.people_loading = false;
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

    // ----- B1: global message search ----------------------------------------

    /// The global message-search query.
    pub fn search_query(&self) -> &str {
        &self.search_query
    }

    /// Mutable global message-search query (the search view edits it directly).
    pub fn search_query_mut(&mut self) -> &mut String {
        &mut self.search_query
    }

    /// The current search results (newest first).
    pub fn search_results(&self) -> &[SearchHit] {
        &self.search_results
    }

    /// The selected search-result row index.
    pub fn search_selected(&self) -> usize {
        self.search_selected
    }

    /// Whether the optional server search fallback is in flight.
    pub fn search_loading(&self) -> bool {
        self.search_loading
    }

    /// Take the one-shot search-box focus request.
    pub fn take_search_focus(&mut self) -> bool {
        std::mem::take(&mut self.focus_search)
    }

    /// The message id to highlight after jumping from a search result, if any.
    pub fn highlight_message(&self) -> Option<&str> {
        self.highlight_message.as_deref()
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

    /// The load state of one inline media URL, if it has been requested (M13).
    pub fn media_state(&self, url: &str) -> Option<MediaState> {
        self.media.get(url).cloned()
    }

    /// Request that an inline media URL be fetched and cached (M13).
    ///
    /// Idempotent: a URL already known (loading/loaded/failed) is left alone.
    /// The view only records the intent — it inserts a `Loading` placeholder and
    /// queues [`Action::LoadMedia`], which `apply_actions` turns into the
    /// backend fetch (ADR-026).
    pub fn request_media(&mut self, url: &str) {
        if self.media.contains_key(url) {
            return;
        }
        self.media.insert(url.to_string(), MediaState::Loading);
        self.push_action(Action::LoadMedia {
            url: url.to_string(),
        });
    }

    /// The [`NotificationContext`] the app routes realtime messages with,
    /// derived from the persisted notification settings, our own MRI (decoded
    /// from the skype token when available) and the store's muted chats.
    pub(crate) fn notification_context(&self) -> NotificationContext {
        let mut ctx = NotificationContext::new()
            .with_notifications_enabled(self.settings.notifications_enabled)
            .with_notify_only_mentions(self.settings.notify_only_mentions);
        // Prefer the whoami identity (M13); fall back to the skype JWT before
        // whoami resolves.
        if let Some(mri) = self.session.self_mri() {
            ctx = ctx.with_self_mri(mri);
        } else if let Some(tokens) = self.session.tokens()
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
        let on_search = self.page == Page::Search;
        for (key, modifiers) in events {
            let Some(shortcut) = shortcuts::resolve(key, modifiers) else {
                continue;
            };
            // On the Search page the plain navigation keys move the *result*
            // selection, not the chat list — even while the query box is
            // focused (a single-line field has no use for Up/Down) (B1).
            if on_search {
                match shortcut {
                    Shortcut::NextChat => {
                        self.push_action(Action::NextSearchResult);
                        continue;
                    }
                    Shortcut::PrevChat => {
                        self.push_action(Action::PrevSearchResult);
                        continue;
                    }
                    _ => {}
                }
            }
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
            Shortcut::GlobalSearch => self.push_action(Action::SetPage(Page::Search)),
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

    // ----- B1: global message search ----------------------------------------

    /// Run the local-first search for `query`, then (only when the local store
    /// had no hits and a client is attached) kick off the optional server
    /// fallback. The store query is synchronous and cheap, so the local results
    /// are ready in this frame; the server answer arrives as
    /// [`AppEvent::SearchResults`] and repaints only then (ADR-027).
    fn run_search(&mut self, query: String) {
        self.search_query = query;
        self.search_selected = 0;
        self.search_loading = false;
        let trimmed = self.search_query.trim().to_string();
        match self
            .search_svc
            .local(&trimmed, None, squads_core::search::SEARCH_LIMIT)
        {
            Ok(hits) => self.search_results = hits,
            Err(e) => {
                self.push_error(e);
                self.search_results.clear();
            }
        }
        if self.search_results.is_empty() && !trimmed.is_empty() && self.search_svc.has_client() {
            let search = Arc::clone(&self.search_svc);
            self.search_loading = true;
            self.runtime.spawn(async move {
                let hits = search
                    .server(&trimmed, squads_core::search::SEARCH_LIMIT)
                    .await
                    .unwrap_or_default();
                AppEvent::SearchResults {
                    query: trimmed,
                    hits,
                }
            });
        }
    }

    /// Move the search-result selection by `delta`, clamped to the list.
    fn select_relative_result(&mut self, delta: i32) {
        if self.search_results.is_empty() {
            return;
        }
        let len = self.search_results.len() as i32;
        let next = (self.search_selected as i32 + delta).clamp(0, len - 1) as usize;
        self.search_selected = next;
    }

    /// After a jump from a search result, make sure the matched message is in
    /// the loaded timeline: if it is not, pull older pages until it appears or
    /// history is exhausted — and, when exhausted, say so rather than silently
    /// doing nothing (B1).
    fn resolve_highlight(&mut self) {
        let Some(message_id) = self.highlight_message.clone() else {
            return;
        };
        if self.timeline.iter().any(|message| message.id == message_id) {
            return;
        }
        if self.has_older {
            if let Some(chat_id) = self.selected_chat.clone() {
                self.load_older(chat_id);
            }
            return;
        }
        // Not cached and no older page to fetch: document the limitation.
        self.highlight_message = None;
        self.toasts.push(Toast {
            text: "The matched message is outside the cached history window.".into(),
            error: false,
        });
    }

    // ----- B3: global file search & the reusable people picker --------------

    /// Run the global drive file search for `query`. The results arrive as
    /// [`AppEvent::FileSearchResults`] (a repaint only then — ADR-027). An
    /// empty query clears the results and never hits the network.
    fn run_file_search(&mut self, query: String) {
        self.file_search_query = query;
        let trimmed = self.file_search_query.trim().to_string();
        if trimmed.is_empty() {
            self.file_search_results.clear();
            self.file_search_loading = false;
            return;
        }
        let Some(files) = self.files_svc.clone() else {
            return;
        };
        self.file_search_loading = true;
        self.runtime.spawn(async move {
            match files.search_files(&trimmed, FILE_SEARCH_LIMIT).await {
                Ok(list) => AppEvent::FileSearchResults {
                    query: trimmed,
                    files: list,
                },
                Err(e) => AppEvent::Error(format!("search files: {e}")),
            }
        });
    }

    /// Run the directory people search for `query` (the people picker). The
    /// results arrive as [`AppEvent::PeopleResults`]. An empty query clears the
    /// results and never hits the network.
    fn run_people_search(&mut self, query: String) {
        self.people_query = query;
        let trimmed = self.people_query.trim().to_string();
        if trimmed.is_empty() {
            self.people_results.clear();
            self.people_loading = false;
            return;
        }
        let Some(people) = self.people_svc.clone() else {
            return;
        };
        self.people_loading = true;
        self.runtime.spawn(async move {
            match people.search(&trimmed, PEOPLE_LIMIT).await {
                Ok(list) => AppEvent::PeopleResults {
                    query: trimmed,
                    people: list,
                },
                Err(e) => AppEvent::Error(format!("search people: {e}")),
            }
        });
    }

    /// Add `person` to the picker selection, or remove it if already picked.
    fn toggle_person_selected(&mut self, person: Person) {
        if let Some(index) = self
            .people_selected
            .iter()
            .position(|picked| picked.id == person.id)
        {
            self.people_selected.remove(index);
        } else {
            self.people_selected.push(person);
        }
    }

    // ----- B2a: new chat & chat management ----------------------------------

    /// Open the people picker in **"new chat"** mode with a clean selection.
    fn open_new_chat(&mut self) {
        self.people_selected.clear();
        self.people_query.clear();
        self.people_results.clear();
        self.people_loading = false;
        self.new_chat_topic.clear();
        self.new_chat_open = true;
        self.people_picker_open = true;
        self.focus_people_search = true;
    }

    /// Create a chat from the picker selection: one person → a 1:1, several →
    /// a group with the topic. The picker closes either way.
    fn create_chat(&mut self) {
        let people = self.people_selected.clone();
        if people.is_empty() {
            self.toasts.push(Toast {
                text: "Pick at least one person to start a chat.".into(),
                error: false,
            });
            return;
        }
        let topic = self.new_chat_topic.trim().to_string();
        self.people_picker_open = false;
        self.new_chat_open = false;
        if people.len() == 1 {
            self.create_one_to_one(people[0].clone());
        } else {
            self.create_group(topic, people);
        }
    }

    /// Create a 1:1 chat with `person` (their Graph id is the create call's
    /// user ref) and open it on success.
    fn create_one_to_one(&mut self, person: Person) {
        let Some(chats) = self.chats_svc.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match chats.new_one_to_one(&person.id).await {
                Ok(chat) => AppEvent::ChatCreated(chat),
                Err(e) => AppEvent::Error(format!("new chat: {e}")),
            }
        });
    }

    /// Create a group chat with `people` and an optional `topic`.
    fn create_group(&mut self, topic: String, people: Vec<Person>) {
        let Some(chats) = self.chats_svc.clone() else {
            return;
        };
        let ids: Vec<String> = people.iter().map(|p| p.id.clone()).collect();
        let topic = if topic.is_empty() { None } else { Some(topic) };
        self.runtime.spawn(async move {
            match chats.new_group(topic.as_deref(), &ids).await {
                Ok(chat) => AppEvent::ChatCreated(chat),
                Err(e) => AppEvent::Error(format!("new group: {e}")),
            }
        });
    }

    /// Toggle one chat's mute: the **local** routing flag (M8) is written
    /// synchronously — immediate and offline — and the **server** write is
    /// best-effort on the backend (B2a/ADR-038). The service re-affirms the
    /// local flag and records the server state.
    fn toggle_chat_muted(&mut self, chat_id: String) {
        let muted = !self.is_chat_muted(&chat_id);
        let _ = self.store.set_chat_muted(&self.profile, &chat_id, muted);
        self.reload_chat_flags();
        if let Some(chats) = self.chats_svc.clone() {
            self.runtime.spawn(async move {
                match chats.set_muted(&chat_id, muted).await {
                    Ok(()) => AppEvent::ChatFlagsChanged,
                    Err(e) => AppEvent::Error(format!("mute: {e}")),
                }
            });
        }
    }

    /// Toggle one chat's hidden flag (local immediate, server best-effort).
    fn toggle_chat_hidden(&mut self, chat_id: String) {
        let hidden = !self.is_chat_hidden(&chat_id);
        let _ = self.store.set_chat_hidden(&self.profile, &chat_id, hidden);
        self.reload_chat_flags();
        if let Some(chats) = self.chats_svc.clone() {
            self.runtime.spawn(async move {
                match chats.set_hidden(&chat_id, hidden).await {
                    Ok(()) => AppEvent::ChatFlagsChanged,
                    Err(e) => AppEvent::Error(format!("hide: {e}")),
                }
            });
        }
    }

    /// Confirm leaving `chat_id`: remove it from the server roster and drop it
    /// from the local list.
    fn confirm_leave_chat(&mut self, chat_id: String) {
        self.leave_confirm = None;
        self.chat_menu = None;
        let Some(chats) = self.chats_svc.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match chats.leave(&chat_id).await {
                Ok(()) => AppEvent::ChatLeft(chat_id),
                Err(e) => AppEvent::Error(format!("leave: {e}")),
            }
        });
    }

    /// Recompute the local mute/hidden mirrors from the store (called with the
    /// chat list, never per frame).
    fn reload_chat_flags(&mut self) {
        self.muted_ids = self
            .store
            .muted_chats(&self.profile)
            .unwrap_or_default()
            .into_iter()
            .collect();
        self.hidden_ids = self
            .store
            .hidden_chats(&self.profile)
            .unwrap_or_default()
            .into_iter()
            .collect();
    }

    // ----- B2b: folders & pins ---------------------------------------------

    /// Paint the folder tabs from the store (offline), dropping a selection
    /// that no longer names a folder.
    fn reload_folders_from_store(&mut self) {
        match self.store.chat_folders(&self.profile) {
            Ok(folders) => self.folders = folders,
            Err(e) => self.push_error(e),
        }
        if let Some(selected) = self.selected_folder.clone() {
            if !self.folders.iter().any(|folder| folder.id == selected) {
                self.selected_folder = None;
            }
        }
    }

    /// Fetch the folders from the service and repaint the tabs.
    fn refresh_folders(&mut self) {
        let Some(folders) = self.folders_svc.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match folders.folders().await {
                Ok(list) => AppEvent::FoldersLoaded(list),
                Err(e) => AppEvent::Error(format!("folders: {e}")),
            }
        });
    }

    /// Select a folder tab (empty id = "All") and repaint the list.
    fn select_folder(&mut self, folder_id: String) {
        self.selected_folder = if folder_id.trim().is_empty() {
            None
        } else {
            Some(folder_id)
        };
    }

    /// Move a chat into a folder (B2b): write the local membership immediately
    /// so the tabs update offline, then run the server move on the backend
    /// (best-effort — see `Folders`).
    fn move_chat_to_folder(&mut self, chat_id: String, folder_id: String) {
        let target = folder_id.trim().to_string();
        let folder = if target.is_empty() {
            None
        } else {
            Some(target.as_str())
        };
        let _ = self.store.set_chat_folder(&self.profile, &chat_id, folder);
        self.reload_folders_from_store();
        self.chat_menu = None;
        if let Some(folders) = self.folders_svc.clone() {
            self.runtime.spawn(async move {
                match folders.move_to_folder(&chat_id, &target).await {
                    Ok(list) => AppEvent::FoldersLoaded(list),
                    Err(e) => AppEvent::Error(format!("move to folder: {e}")),
                }
            });
        }
    }

    /// Paint the selected chat's pins from the store (offline).
    fn reload_pins_from_store(&mut self) {
        let Some(chat_id) = self.selected_chat.clone() else {
            self.pins.clear();
            return;
        };
        match self.store.pinned_messages(&self.profile, &chat_id) {
            Ok(mut pins) => {
                for pin in &mut pins {
                    if let Ok(Some(message)) = self.store.message(&chat_id, &pin.message_id) {
                        pin.message = Some(message);
                    }
                }
                self.pins = pins;
            }
            Err(e) => self.push_error(e),
        }
    }

    /// Fetch the selected chat's pins from the service.
    fn refresh_pins(&mut self) {
        let Some(chat_id) = self.selected_chat.clone() else {
            return;
        };
        let Some(pins) = self.pins_svc.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match pins.pinned(&chat_id).await {
                Ok(list) => AppEvent::PinsLoaded {
                    chat_id,
                    pins: list,
                },
                Err(e) => AppEvent::Error(format!("pins: {e}")),
            }
        });
    }

    /// Unpin one message: drop it from the local list immediately and run the
    /// (best-effort) server unpin on the backend.
    fn unpin_message(&mut self, chat_id: String, message_id: String) {
        if let Err(e) = self.store.remove_pin(&self.profile, &chat_id, &message_id) {
            self.push_error(e);
        }
        self.pins.retain(|pin| pin.message_id != message_id);
        if let Some(pins) = self.pins_svc.clone() {
            self.runtime.spawn(async move {
                match pins.unpin(&chat_id, &message_id).await {
                    Ok(()) => {
                        // Re-read the local cache (the server unpin is
                        // best-effort; the local removal is the source of
                        // truth today — see `Pins`).
                        let list = pins.cached_pinned(&chat_id).unwrap_or_default();
                        AppEvent::PinsLoaded {
                            chat_id,
                            pins: list,
                        }
                    }
                    Err(e) => AppEvent::Error(format!("unpin: {e}")),
                }
            });
        }
    }

    // ----- B4: team administration ----------------------------------------

    /// Search the directory for public teams (B4). A blank query clears the
    /// results without a request.
    fn search_public_teams(&mut self, query: String) {
        if query.trim().is_empty() {
            self.public_results.clear();
            self.public_loading = false;
            return;
        }
        let Some(teams) = self.teams_svc.clone() else {
            return;
        };
        self.public_loading = true;
        self.runtime.spawn(async move {
            match teams.search_public(&query).await {
                Ok(found) => AppEvent::PublicTeamsLoaded(found),
                Err(e) => AppEvent::Error(format!("search public teams: {e}")),
            }
        });
    }

    /// Join a public team, then refresh the joined-teams cache (B4).
    fn join_team(&mut self, team_id: String) {
        let Some(teams) = self.teams_svc.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match teams.join(&team_id).await {
                Ok(team) => AppEvent::TeamJoined(team),
                Err(e) => AppEvent::Error(format!("join team: {e}")),
            }
        });
    }

    /// Fetch one team's roster (B4).
    fn refresh_team_members(&mut self, team_id: String) {
        let Some(teams) = self.teams_svc.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match teams.members(&team_id).await {
                Ok(members) => AppEvent::TeamMembersLoaded { team_id, members },
                Err(e) => AppEvent::Error(format!("team members: {e}")),
            }
        });
    }

    /// Open the people picker in "add team member" mode (B4).
    fn open_add_team_member(&mut self, team_id: String) {
        self.people_selected.clear();
        self.add_member_owner = false;
        self.add_member_team = Some(team_id);
        self.new_chat_open = false;
        self.people_picker_open = true;
        self.focus_people_search = true;
    }

    /// Add one person to a team (B4). Clears the add-member flow once queued.
    fn add_team_member(&mut self, team_id: String, user_id: String, owner: bool) {
        self.add_member_team = None;
        self.people_picker_open = false;
        self.people_selected.clear();
        let Some(teams) = self.teams_svc.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match teams.add_member(&team_id, &user_id, owner).await {
                Ok(_) => AppEvent::TeamMemberChanged { team_id },
                Err(e) => AppEvent::Error(format!("add member: {e}")),
            }
        });
    }

    /// Confirm and run a membership removal (B4).
    fn confirm_remove_team_member(&mut self, team_id: String, membership_id: String) {
        self.remove_member_confirm = None;
        let Some(teams) = self.teams_svc.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match teams.remove_member(&team_id, &membership_id).await {
                Ok(()) => AppEvent::TeamMemberChanged { team_id },
                Err(e) => AppEvent::Error(format!("remove member: {e}")),
            }
        });
    }

    /// Add or remove a reaction on a channel message (B4), routing through the
    /// `Teams` channel-reaction lane and reloading the timeline on success.
    fn react_in_channel(
        &mut self,
        conversation_id: String,
        message_id: String,
        emoji: String,
        remove: bool,
        is_reply: bool,
    ) {
        let Some(teams) = self.teams_svc.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match teams
                .react_in_channel(&conversation_id, &message_id, &emoji, remove, is_reply)
                .await
            {
                Ok(()) => AppEvent::ReactionsChanged {
                    chat_id: conversation_id,
                },
                Err(e) => AppEvent::Error(format!("react: {e}")),
            }
        });
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
        if self.people_svc.is_none() {
            match self.session.people() {
                Ok(service) => self.people_svc = Some(Arc::new(service)),
                Err(e) => self.push_error(e),
            }
        }
        if self.chats_svc.is_none() {
            match self.session.chats() {
                Ok(service) => self.chats_svc = Some(Arc::new(service)),
                Err(e) => self.push_error(e),
            }
        }
        if self.folders_svc.is_none() {
            match self.session.folders() {
                Ok(service) => self.folders_svc = Some(Arc::new(service)),
                Err(e) => self.push_error(e),
            }
        }
        if self.pins_svc.is_none() {
            match self.session.pins() {
                Ok(service) => self.pins_svc = Some(Arc::new(service)),
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
        if self.recordings_svc.is_none() {
            match self.session.recordings() {
                Ok(service) => self.recordings_svc = Some(Arc::new(service)),
                Err(e) => self.push_error(e),
            }
        }
        if self.media_cache.is_none() {
            // The inline-media cache authenticates Microsoft hosts with the
            // skype token (M13); build it once signed in.
            let token = self
                .session
                .tokens()
                .map(|tokens| tokens.skype_token().to_string())
                .unwrap_or_default();
            self.media_cache = Some(Arc::new(MediaCache::new(Arc::clone(&self.dirs), token)));
        }
        // Attach the protocol client to the local-first search handle so its
        // optional server fallback is available (B1). The store-only handle is
        // fine when signed out.
        if let Ok(client) = self.session.client() {
            self.search_svc = Arc::new(
                Search::new(self.profile.clone(), Arc::clone(&self.store)).with_client(client),
            );
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
        self.refresh_identity();
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
        // B2a: re-derive the local mute/hidden mirrors alongside the list.
        self.reload_chat_flags();
        // B2b: the folder tabs' counts derive from the chat membership, so
        // refresh them with the list (a cheap store read).
        self.reload_folders_from_store();
    }

    fn reload_timeline(&mut self) {
        let Some(chat_id) = self.selected_chat.clone() else {
            self.timeline.clear();
            return;
        };
        match self.store.messages(&chat_id, TIMELINE_LIMIT) {
            Ok(mut messages) => {
                // M13: mark own messages against the current identity, so
                // history cached before whoami resolved still renders on the
                // correct side.
                if let Some(mri) = self.session.self_mri() {
                    for message in &mut messages {
                        message.mark_own(mri);
                    }
                }
                self.timeline = messages;
            }
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

    /// Paint the selected conversation's cached recordings/transcripts from the
    /// store (offline; B5). No-op when no conversation is selected.
    fn reload_recordings_from_store(&mut self) {
        let Some(chat_id) = self.selected_chat.clone() else {
            self.recordings.clear();
            self.transcripts.clear();
            return;
        };
        match self.store.recordings_for_scope(&self.profile, &chat_id) {
            Ok(rows) => self.recordings = rows,
            Err(e) => self.push_error(e),
        }
        match self.store.transcripts_for_scope(&self.profile, &chat_id) {
            Ok(rows) => self.transcripts = rows,
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

    /// Resolve the signed-in identity from Graph `/me` (M13) and deliver it as
    /// [`AppEvent::IdentityLoaded`]. The request runs on the backend; a session
    /// without tokens does nothing.
    fn refresh_identity(&mut self) {
        let Ok(client) = self.session.client() else {
            return;
        };
        let tenant = self
            .session
            .tokens()
            .map(|tokens| tokens.tenant().to_string())
            .unwrap_or_default();
        self.runtime.spawn(async move {
            match client.whoami().await {
                Ok(info) => AppEvent::IdentityLoaded(Identity::from_whoami(&info, tenant)),
                Err(e) => AppEvent::Error(format!("whoami: {e}")),
            }
        });
    }

    /// Fetch one inline media URL into the cache (M13), publishing the local
    /// path (or a failure) back as an [`AppEvent`]. A session with no cache
    /// (headless/offline) leaves the placeholder in place.
    fn load_media(&mut self, url: String) {
        let Some(cache) = self.media_cache.clone() else {
            return;
        };
        self.runtime.spawn(async move {
            match cache.fetch(&url).await {
                Ok(media) => AppEvent::MediaLoaded {
                    url,
                    path: media.path,
                },
                Err(e) => AppEvent::MediaFailed {
                    url,
                    error: e.to_string(),
                },
            }
        });
    }

    /// Restart the realtime feed so its collector picks up a newly-resolved self
    /// MRI (M13). No-op when realtime is not running.
    fn restart_realtime(&mut self) {
        if !self.realtime_started {
            return;
        }
        self.session.stop_realtime();
        self.realtime_started = false;
        self.start_realtime();
    }

    fn select_chat(&mut self, chat_id: String) {
        self.selected_chat = Some(chat_id.clone());
        self.reload_timeline();
        self.has_older = false;
        self.files = self
            .store
            .files_for_chat(&self.profile, &chat_id)
            .unwrap_or_default();
        // B2b: paint the selected chat's cached pins immediately (offline).
        self.reload_pins_from_store();
        // B5: paint the selected conversation's cached recordings (offline).
        self.reload_recordings_from_store();

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
        // B4: paint the cached roster immediately (keeping any already-fetched
        // roster), then refresh it live.
        let cached_members = self.store.team_members(&team_id).unwrap_or_default();
        self.members
            .entry(team_id.clone())
            .or_insert(cached_members);

        let Some(teams) = self.teams_svc.clone() else {
            return;
        };
        let members_team = team_id.clone();
        self.runtime.spawn(async move {
            match teams.channels(&team_id).await {
                Ok(channels) => AppEvent::ChannelsLoaded { team_id, channels },
                Err(e) => AppEvent::Error(format!("channels: {e}")),
            }
        });
        self.refresh_team_members(members_team);
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
        // Resolve the row from the in-memory views first (the selected chat's
        // files, then the global search results), falling back to the store.
        let entry = self
            .files
            .iter()
            .chain(self.file_search_results.iter())
            .find(|file| file.id == file_id)
            .cloned()
            .or_else(|| {
                self.store
                    .files_for_chat(&self.profile, &chat_id)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|file| file.id == file_id)
            });
        let Some(entry) = entry else {
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

    // ----- B5: recordings & transcripts ------------------------------------

    /// Refresh the selected conversation's recordings/transcripts: paint the
    /// cache, then fetch the relevant slice (the OneDrive set for a chat, the
    /// channel's SharePoint set for a channel).
    fn refresh_recordings(&mut self) {
        self.recordings_loading = true;
        self.reload_recordings_from_store();
        let Some(svc) = self.recordings_svc.clone() else {
            self.recordings_loading = false;
            return;
        };
        let Some(chat_id) = self.selected_chat.clone() else {
            self.recordings_loading = false;
            return;
        };
        // A channel conversation resolves its team from the cached channel so
        // the channel slice can be matched by name (B5); an unknown team still
        // takes the channel path (the service then keeps every channel row).
        let is_channel = squads_core::is_channel_conversation_id(&chat_id);
        let team_id = if is_channel {
            self.store
                .team_for_channel(&self.profile, &chat_id)
                .ok()
                .flatten()
        } else {
            None
        };
        self.runtime.spawn(async move {
            let outcome = async {
                if is_channel {
                    let team = team_id.as_deref().unwrap_or("");
                    let recordings = svc
                        .recordings_for_channel(team, &chat_id, RECORDINGS_LIMIT)
                        .await?;
                    let transcripts = svc
                        .transcripts_for_channel(team, &chat_id, RECORDINGS_LIMIT)
                        .await?;
                    Ok::<_, squads_core::CoreError>((recordings, transcripts))
                } else {
                    let recordings = svc.recordings_for_chat(&chat_id, RECORDINGS_LIMIT).await?;
                    let transcripts = svc.transcripts_for_chat(&chat_id, RECORDINGS_LIMIT).await?;
                    Ok::<_, squads_core::CoreError>((recordings, transcripts))
                }
            }
            .await;
            match outcome {
                Ok((recordings, transcripts)) => AppEvent::RecordingsLoaded {
                    chat_id,
                    recordings,
                    transcripts,
                },
                Err(e) => AppEvent::Error(format!("recordings: {e}")),
            }
        });
    }

    /// Download one recording (resolved from the in-memory list, then the
    /// store) to the configured download directory (B5). Reuses the M7 path.
    fn download_recording(&mut self, id: String) {
        let Some(svc) = self.recordings_svc.clone() else {
            return;
        };
        let entry = self
            .recordings
            .iter()
            .find(|recording| recording.id == id)
            .cloned()
            .or_else(|| self.store.recording(&self.profile, &id).ok().flatten());
        let Some(entry) = entry else {
            return;
        };
        let dest = self.download_target().join(&entry.name);
        let dest_path = dest.to_string_lossy().into_owned();
        self.runtime.spawn(async move {
            match svc.download(&entry, &dest_path, |_, _| {}).await {
                Ok(_) => AppEvent::Error(format!("downloaded {}", dest.display())),
                Err(e) => AppEvent::Error(format!("download: {e}")),
            }
        });
    }

    /// Download one transcript to the configured download directory (B5).
    fn download_transcript(&mut self, id: String) {
        let Some(svc) = self.recordings_svc.clone() else {
            return;
        };
        let entry = self
            .transcripts
            .iter()
            .find(|transcript| transcript.id == id)
            .cloned()
            .or_else(|| self.store.transcript(&self.profile, &id).ok().flatten());
        let Some(entry) = entry else {
            return;
        };
        let dest = self.download_target().join(&entry.name);
        let dest_path = dest.to_string_lossy().into_owned();
        self.runtime.spawn(async move {
            match svc.download_transcript(&entry, &dest_path, |_, _| {}).await {
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
                    // A search jump may need older pages (B1).
                    self.resolve_highlight();
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
                    // Keep pulling until the search jump target is found (B1).
                    self.resolve_highlight();
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
            AppEvent::IdentityLoaded(identity) => {
                let mri = identity.mri.clone();
                self.session.set_identity(identity);
                // Push the MRI into the live services so newly-loaded history is
                // marked own, and restart realtime so its collector does too.
                if let Some(messaging) = &self.messaging {
                    messaging.set_self_mri(mri);
                }
                self.restart_realtime();
                // Re-open the selected chat so its history is re-marked.
                if let Some(chat_id) = self.selected_chat.clone() {
                    self.select_chat(chat_id);
                }
            }
            AppEvent::MediaLoaded { url, path } => {
                self.media.insert(url, MediaState::Loaded(path));
            }
            AppEvent::MediaFailed { url, error } => {
                tracing::debug!(%url, %error, "inline media fetch failed");
                self.media.insert(url, MediaState::Failed);
            }
            AppEvent::SearchResults { query, hits } => {
                // Ignore a stale response for a query the user has since
                // changed; either way the loading flag is cleared (B1).
                if self.search_query.trim() == query.trim() {
                    self.search_results = hits;
                    self.search_selected = self
                        .search_selected
                        .min(self.search_results.len().saturating_sub(1));
                }
                self.search_loading = false;
            }
            AppEvent::FileSearchResults { query, files } => {
                // Ignore a stale response for a query the user has changed;
                // either way the loading flag is cleared (B3).
                if self.file_search_query.trim() == query.trim() {
                    self.file_search_results = files;
                }
                self.file_search_loading = false;
            }
            AppEvent::PeopleResults { query, people } => {
                // Ignore a stale response for a query the user has changed;
                // either way the loading flag is cleared (B3).
                if self.people_query.trim() == query.trim() {
                    self.people_results = people;
                }
                self.people_loading = false;
            }
            AppEvent::ChatCreated(chat) => {
                if let Err(e) = self
                    .store
                    .upsert_chats(&self.profile, std::slice::from_ref(&chat))
                {
                    self.push_error(e);
                }
                self.people_selected.clear();
                self.new_chat_open = false;
                self.reload_chats_from_store();
                // Open the new conversation (B2a).
                self.select_chat(chat.id);
                self.page = Page::Chats;
            }
            AppEvent::ChatLeft(chat_id) => {
                self.reload_chats_from_store();
                if self.selected_chat.as_deref() == Some(chat_id.as_str()) {
                    self.selected_chat = None;
                    self.timeline.clear();
                    self.has_older = false;
                    self.files.clear();
                }
                self.toasts.push(Toast {
                    text: "Left the chat.".into(),
                    error: false,
                });
            }
            AppEvent::ChatFlagsChanged => self.reload_chats_from_store(),
            AppEvent::FoldersLoaded(folders) => {
                if let Err(e) = self.store.replace_chat_folders(&self.profile, &folders) {
                    self.push_error(e);
                }
                self.reload_folders_from_store();
            }
            AppEvent::PinsLoaded { chat_id, pins } => {
                // Only apply a response for the chat that is still selected.
                if self.selected_chat.as_deref() == Some(chat_id.as_str()) {
                    self.pins = pins;
                }
            }
            AppEvent::PublicTeamsLoaded(teams) => {
                self.public_results = teams;
                self.public_loading = false;
            }
            AppEvent::TeamJoined(team) => {
                if let Err(e) = self
                    .store
                    .upsert_teams(&self.profile, std::slice::from_ref(&team))
                {
                    self.push_error(e);
                }
                self.teams = self.store.list_teams(&self.profile).unwrap_or_default();
                // Mark the join in the search results so the button flips.
                for result in &mut self.public_results {
                    if result.id == team.id {
                        result.is_joined = true;
                    }
                }
                self.expanded_teams.insert(team.id.clone());
                self.refresh_team_members(team.id);
                self.toasts.push(Toast {
                    text: format!("Joined {}", team.name),
                    error: false,
                });
            }
            AppEvent::TeamMembersLoaded { team_id, members } => {
                self.members.insert(team_id, members);
            }
            AppEvent::TeamMemberChanged { team_id } => {
                // Re-fetch the roster live (the cached read loses the user ids
                // the owner check needs — B4).
                self.refresh_team_members(team_id);
            }
            AppEvent::ReactionsChanged { chat_id } => {
                if self.selected_chat.as_deref() == Some(chat_id.as_str()) {
                    self.reload_timeline();
                }
            }
            AppEvent::RecordingsLoaded {
                chat_id,
                recordings,
                transcripts,
            } => {
                // Only apply a response for the conversation still selected (B5).
                if self.selected_chat.as_deref() == Some(chat_id.as_str()) {
                    self.recordings = recordings;
                    self.transcripts = transcripts;
                }
                self.recordings_loading = false;
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
                    Page::Search => {
                        // Focus the query box so the user can type immediately.
                        self.focus_search = true;
                    }
                    _ => {}
                }
            }
            Action::SelectChat(chat_id) => {
                // A manual selection clears any search jump highlight.
                self.highlight_message = None;
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
            Action::SearchFiles(query) => self.run_file_search(query),
            Action::SearchPeople(query) => self.run_people_search(query),
            Action::TogglePersonSelected(person) => self.toggle_person_selected(person),
            Action::ClearPeopleSelection => self.people_selected.clear(),
            Action::TogglePeoplePicker => {
                // The generic toggle is browse-only (B3): clear new-chat mode so
                // it cannot leak in from a previous New chat (B2a).
                self.new_chat_open = false;
                self.people_picker_open = !self.people_picker_open;
                if self.people_picker_open {
                    self.focus_people_search = true;
                }
            }
            Action::NewChat => self.open_new_chat(),
            Action::CreateChat => self.create_chat(),
            Action::ToggleChatMenu(chat_id) => {
                self.chat_menu = if self.chat_menu.as_deref() == Some(chat_id.as_str()) {
                    None
                } else {
                    Some(chat_id)
                };
            }
            Action::ToggleChatMuted(chat_id) => self.toggle_chat_muted(chat_id),
            Action::ToggleChatHidden(chat_id) => self.toggle_chat_hidden(chat_id),
            Action::RequestLeaveChat(chat_id) => self.leave_confirm = Some(chat_id),
            Action::ConfirmLeaveChat(chat_id) => self.confirm_leave_chat(chat_id),
            Action::CancelLeaveChat => self.leave_confirm = None,
            Action::ToggleShowHidden => self.show_hidden = !self.show_hidden,
            Action::RefreshFolders => self.refresh_folders(),
            Action::SelectFolder(folder_id) => self.select_folder(folder_id),
            Action::MoveChatToFolder { chat_id, folder_id } => {
                self.move_chat_to_folder(chat_id, folder_id)
            }
            Action::RefreshPins => self.refresh_pins(),
            Action::UnpinMessage {
                chat_id,
                message_id,
            } => self.unpin_message(chat_id, message_id),
            Action::SearchPublicTeams(query) => self.search_public_teams(query),
            Action::JoinTeam(team_id) => self.join_team(team_id),
            Action::RefreshTeamMembers(team_id) => self.refresh_team_members(team_id),
            Action::OpenAddTeamMember(team_id) => self.open_add_team_member(team_id),
            Action::AddTeamMember {
                team_id,
                user_id,
                owner,
            } => self.add_team_member(team_id, user_id, owner),
            Action::RequestRemoveTeamMember {
                team_id,
                membership_id,
            } => self.remove_member_confirm = Some((team_id, membership_id)),
            Action::ConfirmRemoveTeamMember {
                team_id,
                membership_id,
            } => self.confirm_remove_team_member(team_id, membership_id),
            Action::CancelRemoveTeamMember => self.remove_member_confirm = None,
            Action::ReactInChannel {
                conversation_id,
                message_id,
                emoji,
                remove,
                is_reply,
            } => self.react_in_channel(conversation_id, message_id, emoji, remove, is_reply),
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
            Action::RefreshRecordings => self.refresh_recordings(),
            Action::DownloadRecording { id } => self.download_recording(id),
            Action::DownloadTranscript { id } => self.download_transcript(id),
            Action::LoadMedia { url } => self.load_media(url),
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
                // Also clear the global message search (B1).
                self.search_query.clear();
                self.search_results.clear();
                self.search_selected = 0;
                self.search_loading = false;
                self.highlight_message = None;
                // The people picker closes but keeps its selection, so a caller
                // can still read the picked people (B3).
                self.people_picker_open = false;
                // The chat context menu / new-chat / leave-confirm overlays too
                // (B2a).
                self.chat_menu = None;
                self.new_chat_open = false;
                self.leave_confirm = None;
                // The add-member flow and its removal confirmation (B4).
                self.add_member_team = None;
                self.remove_member_confirm = None;
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
            Action::SearchMessages(query) => self.run_search(query),
            Action::NextSearchResult => self.select_relative_result(1),
            Action::PrevSearchResult => self.select_relative_result(-1),
            Action::OpenSearchHit {
                chat_id,
                message_id,
            } => {
                self.select_chat(chat_id);
                self.page = Page::Chats;
                self.highlight_message = Some(message_id);
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
        self.people_svc = None;
        self.calls_svc = None;
        self.calendar_svc = None;
        self.media_cache = None;
        self.media.clear();
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
        self.file_search_query.clear();
        self.file_search_results.clear();
        self.file_search_loading = false;
        self.people_query.clear();
        self.people_results.clear();
        self.people_selected.clear();
        self.people_loading = false;
        self.people_picker_open = false;
        self.chats_svc = None;
        self.muted_ids.clear();
        self.hidden_ids.clear();
        self.show_hidden = false;
        self.chat_menu = None;
        self.new_chat_open = false;
        self.new_chat_topic.clear();
        self.leave_confirm = None;
        self.folders_svc = None;
        self.pins_svc = None;
        self.folders.clear();
        self.selected_folder = None;
        self.pins.clear();
        self.public_query.clear();
        self.public_results.clear();
        self.public_loading = false;
        self.members.clear();
        self.add_member_team = None;
        self.add_member_owner = false;
        self.remove_member_confirm = None;
        self.meetings.clear();
        self.calendar_loading = false;
        self.join_target = None;
        self.calendar_week = false;
        self.join_input.clear();
        self.join_passcode.clear();
        self.call_state = CallState::Idle;
        self.incoming = None;
        // Drop the search client (the handle is local-only until the next
        // sign-in) and any results (B1).
        self.search_svc = Arc::new(Search::new(self.profile.clone(), Arc::clone(&self.store)));
        self.search_query.clear();
        self.search_results.clear();
        self.search_selected = 0;
        self.search_loading = false;
        self.highlight_message = None;
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
        // The reusable people picker is a floating overlay too (B3).
        views::people_picker::show(self, ui.ctx());
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
