//! The app's tokio runtime and the core→UI event bridge (ADR-004, ADR-026).
//!
//! `squads-core` is fully async, but egui has no executor: the update loop is
//! synchronous. [`AppRuntime`] owns a multi-threaded [`tokio::runtime::Runtime`]
//! ("the backend") and bridges it to the UI with a channel of [`AppEvent`]s.
//!
//! * `spawn(future)` runs a future that resolves to one [`AppEvent`]; the result
//!   is sent back over the channel.
//! * `spawn_task(future)` runs a fire-and-forget future (e.g. the realtime
//!   forwarding loop) that uses [`EventSender`] to publish as many events as it
//!   likes.
//! * `drain()` is called from [`eframe::App::logic`] and returns everything the
//!   backend has produced since the last frame, using a non-blocking `try_recv`.
//!
//! ## Repaint discipline (ADR-006 / ADR-027)
//!
//! The UI must not poll: an idle window costs ~0% CPU. When a background task
//! publishes an event, [`EventSender::send`] calls [`egui::Context::request_repaint`]
//! **once** (only if a context has been installed), so the next frame drains the
//! event. Nothing here ever requests a repaint unconditionally.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;

use tokio::runtime::{Handle, Runtime};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use squads_core::api::JoinTarget;
use squads_core::model::{CallLogEntry, Chat, FileEntry, Meeting, Message, SearchHit, Team};
use squads_core::{CallState, Channel, ContactPresence, Identity, RealtimeEvent, SignInProgress};

/// Everything the backend can tell the UI about.
///
/// The UI applies these at the top of a frame and requests a repaint only when
/// at least one arrived (see [`crate::app::SquadsApp::logic`]).
pub enum AppEvent {
    /// A step of the device-code sign-in flow.
    SignInProgress(SignInProgress),
    /// The newest chat list, fetched from the service.
    ChatsLoaded(Vec<Chat>),
    /// A page of messages for `chat_id` (newest page or a merged older page).
    MessagesLoaded {
        /// Owning chat / channel conversation.
        chat_id: String,
        /// The messages to merge into the store (any order).
        messages: Vec<Message>,
    },
    /// An older page was fetched; `has_more` is whether a further page exists.
    OlderLoaded {
        /// Owning chat / channel conversation.
        chat_id: String,
        /// The older messages to merge.
        messages: Vec<Message>,
        /// Whether a further (older) page is available.
        has_more: bool,
    },
    /// The joined teams, fetched from the service.
    TeamsLoaded(Vec<Team>),
    /// The channels of one team.
    ChannelsLoaded {
        /// Owning team id.
        team_id: String,
        /// The team's channels.
        channels: Vec<Channel>,
    },
    /// The files of one chat / channel.
    FilesLoaded {
        /// Owning chat / channel.
        chat_id: String,
        /// The files to display (and cache).
        files: Vec<FileEntry>,
    },
    /// The upcoming meetings, fetched from the calendar service (M12).
    MeetingsLoaded(Vec<Meeting>),
    /// A join string resolved to a target (M12). The app hands a `thread`
    /// target off to the call path; other kinds are shown to the user.
    JoinResolved(JoinTarget),
    /// A meeting was scheduled (M12); the created event is already cached.
    MeetingScheduled(Meeting),
    /// A meeting was cancelled (M12); the event id is already dropped.
    MeetingCancelled(String),
    /// The signed-in identity resolved from whoami (M13).
    IdentityLoaded(Identity),
    /// One inline media URL was fetched and cached (M13): the local file to
    /// render.
    MediaLoaded {
        /// The source URL (the cache key).
        url: String,
        /// The local cached file.
        path: PathBuf,
    },
    /// One inline media URL could not be fetched (M13); the view shows a
    /// placeholder instead.
    MediaFailed {
        /// The source URL (the cache key).
        url: String,
        /// A short, token-free reason.
        error: String,
    },
    /// The **optional** server message search returned (B1). The app only uses
    /// this when the local store had no hits; `query` guards against a stale
    /// response landing after the user changed the query.
    SearchResults {
        /// The query these hits answer.
        query: String,
        /// The mapped hits (possibly empty).
        hits: Vec<SearchHit>,
    },
    /// Batch presence for a set of contacts. No producer is wired yet (the M11
    /// presence panel will emit it); the variant is part of the event vocabulary.
    #[allow(dead_code)]
    PresenceUpdated(Vec<ContactPresence>),
    /// A live realtime event.
    Realtime(RealtimeEvent),
    /// The call state machine moved.
    CallState(CallState),
    /// The call history was refreshed. The recents list is read synchronously
    /// from the store today; the variant is part of the event vocabulary.
    #[allow(dead_code)]
    CallLogLoaded(Vec<CallLogEntry>),
    /// A human-readable, non-fatal error to surface as a toast.
    Error(String),
}

impl AppEvent {
    /// A short, stable name for logs (never prints payloads).
    pub fn kind(&self) -> &'static str {
        match self {
            AppEvent::SignInProgress(_) => "sign_in_progress",
            AppEvent::ChatsLoaded(_) => "chats_loaded",
            AppEvent::MessagesLoaded { .. } => "messages_loaded",
            AppEvent::OlderLoaded { .. } => "older_loaded",
            AppEvent::TeamsLoaded(_) => "teams_loaded",
            AppEvent::ChannelsLoaded { .. } => "channels_loaded",
            AppEvent::FilesLoaded { .. } => "files_loaded",
            AppEvent::MeetingsLoaded(_) => "meetings_loaded",
            AppEvent::JoinResolved(_) => "join_resolved",
            AppEvent::MeetingScheduled(_) => "meeting_scheduled",
            AppEvent::MeetingCancelled(_) => "meeting_cancelled",
            AppEvent::IdentityLoaded(_) => "identity_loaded",
            AppEvent::MediaLoaded { .. } => "media_loaded",
            AppEvent::MediaFailed { .. } => "media_failed",
            AppEvent::SearchResults { .. } => "search_results",
            AppEvent::PresenceUpdated(_) => "presence_updated",
            AppEvent::Realtime(_) => "realtime",
            AppEvent::CallState(_) => "call_state",
            AppEvent::CallLogLoaded(_) => "call_log_loaded",
            AppEvent::Error(_) => "error",
        }
    }
}

/// A cloneable handle the backend uses to publish [`AppEvent`]s and wake the UI.
#[derive(Clone)]
pub struct EventSender {
    tx: UnboundedSender<AppEvent>,
    ctx: Option<egui::Context>,
}

impl EventSender {
    /// Publish an event and (if a context is installed) request one repaint so
    /// the UI drains it on the next frame. Never blocks.
    pub fn send(&self, event: AppEvent) {
        if self.tx.send(event).is_err() {
            // The UI is gone; nothing to do.
            return;
        }
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint();
        }
    }
}

/// Owns the tokio runtime and the core→UI event channel.
pub struct AppRuntime {
    runtime: Runtime,
    tx: UnboundedSender<AppEvent>,
    /// `Mutex` because `Receiver::try_recv` needs `&mut` while `drain(&self)` is
    /// called through `&mut SquadsApp` from `logic`.
    rx: Mutex<UnboundedReceiver<AppEvent>>,
    ctx: Mutex<Option<egui::Context>>,
}

impl AppRuntime {
    /// Build the runtime and channel. Fails only if the OS refuses to spawn the
    /// runtime's worker threads.
    pub fn new() -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("squads-backend")
            .build()?;
        let (tx, rx) = unbounded_channel();
        Ok(Self {
            runtime,
            tx,
            rx: Mutex::new(rx),
            ctx: Mutex::new(None),
        })
    }

    /// Install the egui context so background events can wake the UI. Called
    /// once, after the app is created.
    pub fn set_context(&self, ctx: egui::Context) {
        *self.ctx.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctx);
    }

    /// A [`Handle`] to the backend runtime (e.g. to enter it before calling a
    /// core method that needs an *ambient* runtime, like
    /// [`squads_core::Session::start_realtime`]).
    pub fn handle(&self) -> Handle {
        self.runtime.handle().clone()
    }

    /// A cloneable publisher for the backend tasks.
    pub fn sender(&self) -> EventSender {
        EventSender {
            tx: self.tx.clone(),
            ctx: self.ctx.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        }
    }

    /// Spawn a future that resolves to a single [`AppEvent`]; the result is sent
    /// back to the UI.
    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = AppEvent> + Send + 'static,
    {
        let sender = self.sender();
        self.runtime.spawn(async move {
            sender.send(future.await);
        });
    }

    /// Spawn a fire-and-forget task (it publishes through [`EventSender`]).
    pub fn spawn_task<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.runtime.spawn(future);
    }

    /// Take every pending event, non-blocking. Called once per `logic()`.
    pub fn drain(&self) -> Vec<AppEvent> {
        let mut rx = self.rx.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }
}

impl std::fmt::Debug for AppRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppRuntime").finish_non_exhaustive()
    }
}
