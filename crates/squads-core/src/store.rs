//! Local, offline-first SQLite store (milestone **M3**).
//!
//! This is the durable cache the UI reads from: accounts, chats and messages.
//! It is deliberately **synchronous** — `rusqlite` is a blocking C library and
//! there is no async DB layer here (ADR-005/ADR-011). The M2/M4 async code owns
//! a [`Store`] behind an `Arc` and calls these methods directly; at our scale the
//! calls are sub-millisecond and the UI reads its own already-materialized state.
//!
//! ## Concurrency model (ADR-012)
//!
//! [`Store`] holds a `Mutex<rusqlite::Connection>`, which makes it
//! `Send + Sync` so the whole core can share one `Arc<Store>`. Access is
//! **serialized**: two threads cannot query at once. That is acceptable while
//! writes are occasional (M3 only writes on explicit fetch/import; M4 adds
//! write-through) and reads are cheap. If contention ever matters, the fix is a
//! small reader pool of `Connection`s (SQLite allows many concurrent readers in
//! WAL mode), not an async wrapper.
//!
//! ## Schema & migrations
//!
//! The schema version lives in `PRAGMA user_version`; this build knows **v9**.
//! Migrations are forward-only and re-entrant: statements are `IF NOT EXISTS`
//! (or guarded by a schema probe, for `ALTER TABLE`), so re-running is a no-op.
//! A database whose `user_version` is **newer** than this build understands is
//! rejected with [`CoreError::StoreSchemaVersion`] rather than silently mangled.
//!
//! * **v1** (M3) — `accounts`, `chats`, `messages`.
//! * **v2** (M5) — `messages.send_state` (optimistic sends; existing rows
//!   default to `sent`) and the `read_state` table (our own consumption
//!   horizon). See ADR-014/ADR-015.
//! * **v3** (M6) — `teams`, `channels`, `team_members` (the teams/channel
//!   metadata cache). See ADR-017.
//! * **v4** (M7) — `files`, the driveItem cache for the files browser (keyed
//!   `(id, profile)`, with an optional owning `chat_id`). See ADR-019.
//! * **v5** (M8) — `chats.muted`, the local per-chat notification mute (fed from
//!   the server's `alerts` property); unread/activity are **derived** from
//!   `messages` + `read_state`, so no new table is needed. See ADR-021.
//! * **v6** (M9) — `call_log`, the local call-history table (Teams exposes no
//!   call-history API through the reverse-engineered surfaces). See ADR-025.
//! * **v7** (M12) — `meetings`, the Graph `calendarView` cache for the calendar
//!   agenda/week views (keyed `(id, profile)`). See ADR-033.
//! * **v8** (M13) — `accounts.self_mri` / `accounts.oid`, the signed-in
//!   identity that lets server-loaded history and realtime messages be marked
//!   own. See ADR-034.
//! * **v9** (B1) — the `messages(profile, timestamp)` message-search index. See
//!   ADR-036.
//! * **v10** (B2a) — `chats.hidden` and `chats.server_muted`, the server-side
//!   hide/mute state (the local `chats.muted` stays the notification-routing
//!   truth). See ADR-038.
//!
//! ## Who writes what
//!
//! M3 provides the store API; **realtime write-through landed in M4** and M5's
//! optimistic sends call the same idempotent [`Store::upsert_messages`] /
//! [`Store::upsert_chats`] (keyed by server id) and read history through
//! [`Store::messages`] / [`Store::messages_before`]. M6's teams service caches
//! team/channel/member metadata through [`Store::upsert_teams`],
//! [`Store::upsert_channels`] and [`Store::upsert_team_members`]. M7's files
//! service caches driveItems through [`Store::upsert_files`]. M8 derives unread
//! counts from [`Store::unread_count`] / [`Store::unread_messages`] and persists
//! the mute flag with [`Store::set_chat_muted`]. M9 records local call history
//! through [`Store::upsert_call_log`] / [`Store::call_log`]. M12's calendar
//! service caches meetings through [`Store::upsert_meetings`] and reads them
//! with [`Store::upcoming_meetings`]. B2a's `Chats` service upserts a created
//! chat and drops a left one (`delete_chat`), and mirrors the server mute/hide
//! state with [`Store::set_chat_server_muted`] / [`Store::set_chat_hidden`]
//! (the local `chats.muted` routing flag is unchanged). Nothing in this module
//! talks to the network.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::auth::tokens::now_unix;
use crate::error::{CoreError, Result};
use crate::model::{
    CallDirection, CallLogEntry, CallOutcome, Channel, Chat, FileEntry, Meeting, Message, Reaction,
    SendState, Team, TeamMember,
};

/// Newest schema version this build knows how to create/migrate to.
///
/// * **v1** (M3) — accounts, chats, messages.
/// * **v2** (M5) — `messages.send_state` for optimistic sends, and the
///   `read_state` table for our own read horizon.
/// * **v3** (M6) — `teams`, `channels`, `team_members` for the teams service.
/// * **v4** (M7) — `files`, the driveItem cache for the files service.
/// * **v5** (M8) — `chats.muted`, the local per-chat notification mute.
/// * **v6** (M9) — `call_log`, the local call-history table.
/// * **v7** (M12) — `meetings`, the Graph `calendarView` cache.
/// * **v8** (M13) — `accounts.self_mri` / `accounts.oid`, the signed-in
///   identity [`Message::mark_own`] needs.
/// * **v9** (B1) — `messages(profile, timestamp)`, the message-search index.
/// * **v10** (B2a) — `chats.hidden` and `chats.server_muted`, the server-side
///   hide/mute state (the local `chats.muted` remains the routing flag).
pub const SCHEMA_VERSION: i32 = 10;

/// Timeout for a busy database before `SQLITE_BUSY` is surfaced.
const BUSY_TIMEOUT_MS: u64 = 5_000;

/// v1 schema. Every statement is `IF NOT EXISTS` so migration is idempotent.
const MIGRATION_V1: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    profile      TEXT PRIMARY KEY,
    tenant       TEXT,
    personal     INTEGER NOT NULL DEFAULT 0,
    display_name TEXT,
    created_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS chats (
    id                   TEXT NOT NULL,
    profile              TEXT NOT NULL,
    name                 TEXT NOT NULL,
    is_group             INTEGER NOT NULL DEFAULT 0,
    last_message_time    TEXT,
    last_message_sender  TEXT,
    last_message_preview TEXT,
    updated_at           INTEGER NOT NULL,
    PRIMARY KEY (id, profile)
);

CREATE TABLE IF NOT EXISTS messages (
    id                TEXT NOT NULL,
    chat_id           TEXT NOT NULL,
    profile           TEXT NOT NULL,
    sender_mri        TEXT NOT NULL DEFAULT '',
    sender_name       TEXT NOT NULL DEFAULT '',
    timestamp         TEXT NOT NULL DEFAULT '',
    content           TEXT NOT NULL DEFAULT '',
    raw               TEXT NOT NULL DEFAULT '',
    reactions_json    TEXT NOT NULL DEFAULT '[]',
    reply_to          TEXT,
    client_message_id TEXT,
    edited            INTEGER NOT NULL DEFAULT 0,
    is_own            INTEGER NOT NULL DEFAULT 0,
    updated_at        INTEGER NOT NULL,
    PRIMARY KEY (id, chat_id)
);

CREATE INDEX IF NOT EXISTS idx_messages_chat_time
    ON messages (chat_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_chats_profile_time
    ON chats (profile, last_message_time);
"#;

/// v2 schema additions that are plain `CREATE TABLE IF NOT EXISTS` (re-entrant).
///
/// The `messages.send_state` column is added separately in [`migrate_v2`],
/// because SQLite's `ALTER TABLE … ADD COLUMN` has no `IF NOT EXISTS` and a
/// re-entrant migration must probe the schema first.
const MIGRATION_V2_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS read_state (
    chat_id    TEXT NOT NULL,
    profile    TEXT NOT NULL,
    message_id TEXT NOT NULL,
    read_at    INTEGER NOT NULL,
    PRIMARY KEY (chat_id, profile)
);
"#;

/// v3 schema: the teams/channel metadata cache (M6). Plain
/// `CREATE TABLE IF NOT EXISTS`, so the migration is re-entrant.
const MIGRATION_V3_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS teams (
    id         TEXT NOT NULL,
    profile    TEXT NOT NULL,
    name       TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (id, profile)
);

CREATE TABLE IF NOT EXISTS channels (
    id              TEXT NOT NULL,
    team_id         TEXT NOT NULL,
    profile         TEXT NOT NULL,
    name            TEXT NOT NULL,
    description     TEXT,
    membership_type TEXT,
    web_url         TEXT,
    is_channel      INTEGER NOT NULL DEFAULT 0,
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (id, team_id)
);

CREATE TABLE IF NOT EXISTS team_members (
    id           TEXT NOT NULL,
    team_id      TEXT NOT NULL,
    profile      TEXT NOT NULL,
    display_name TEXT NOT NULL DEFAULT '',
    email        TEXT,
    roles_json   TEXT NOT NULL DEFAULT '[]',
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (id, team_id)
);

CREATE INDEX IF NOT EXISTS idx_channels_team
    ON channels (team_id);
CREATE INDEX IF NOT EXISTS idx_team_members_team
    ON team_members (team_id);
"#;

/// v4 schema: the driveItem cache for the files service (M7).
///
/// Keyed `(id, profile)` (ADR-019): the driveItem id matches the store's
/// existing `(id, profile)` convention and sidesteps keying on the nullable
/// `drive_id`. The owning `chat_id` is a non-key column so a chat listing can
/// be read back with [`Store::files_for_chat`]; recents and folder listings
/// leave it `NULL`. Plain `CREATE TABLE/INDEX IF NOT EXISTS`, so the migration
/// is re-entrant.
const MIGRATION_V4_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS files (
    id            TEXT NOT NULL,
    profile       TEXT NOT NULL,
    drive_id      TEXT,
    chat_id       TEXT,
    name          TEXT NOT NULL DEFAULT '[unnamed]',
    size          INTEGER NOT NULL DEFAULT 0,
    mime          TEXT,
    web_url       TEXT,
    download_url  TEXT,
    is_folder     INTEGER NOT NULL DEFAULT 0,
    created       TEXT,
    modified      TEXT,
    sender        TEXT,
    share_url     TEXT,
    attachment_id TEXT,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (id, profile)
);

CREATE INDEX IF NOT EXISTS idx_files_chat
    ON files (profile, chat_id);
CREATE INDEX IF NOT EXISTS idx_files_drive_item
    ON files (drive_id, id);
"#;

/// v6 schema: the local call-history table (M9). Plain
/// `CREATE TABLE/INDEX IF NOT EXISTS`, so the migration is re-entrant.
///
/// `id` is the wire `callId` when known, else a generated uuid; `thread_id` is
/// the conversation the call belongs to (may be empty). `ended_at`/
/// `duration_secs` are `NULL` while a call is in progress; `outcome` degrades
/// from an unknown value to `unknown`. `participants_json` mirrors the
/// `reactions_json`/`roles_json` convention (a corrupt blob reads as empty).
const MIGRATION_V6_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS call_log (
    id                TEXT NOT NULL,
    profile           TEXT NOT NULL,
    thread_id         TEXT NOT NULL DEFAULT '',
    direction         TEXT NOT NULL DEFAULT 'outgoing',
    started_at        INTEGER NOT NULL,
    ended_at          INTEGER,
    duration_secs     INTEGER,
    participants_json TEXT NOT NULL DEFAULT '[]',
    outcome           TEXT NOT NULL DEFAULT 'unknown',
    updated_at        INTEGER NOT NULL,
    PRIMARY KEY (id, profile)
);

CREATE INDEX IF NOT EXISTS idx_call_log_thread
    ON call_log (profile, thread_id, started_at);
"#;

/// v7 schema: the Graph `calendarView` meeting cache (M12). Plain
/// `CREATE TABLE/INDEX IF NOT EXISTS`, so the migration is re-entrant.
///
/// Keyed `(id, profile)` (the store's convention). `start`/`end` are the Graph
/// `dateTime` strings **verbatim** (`YYYY-MM-DDTHH:MM:SS.fffffff`), so ordering
/// and the `from_secs` filter are lexicographic text comparisons; `start_secs`
/// is *not* stored (the wire carries no timezone). The five columns the vendored
/// projection does not carry (`all_day`, `join_meeting_id`, `passcode`,
/// `web_link`, `location`) exist for forward-compatibility and default empty.
const MIGRATION_V7_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS meetings (
    id              TEXT NOT NULL,
    profile         TEXT NOT NULL,
    subject         TEXT NOT NULL DEFAULT '(no subject)',
    start           TEXT,
    end             TEXT,
    all_day         INTEGER NOT NULL DEFAULT 0,
    organizer       TEXT,
    organizer_email TEXT,
    is_organizer    INTEGER NOT NULL DEFAULT 0,
    join_url        TEXT,
    join_meeting_id TEXT,
    passcode        TEXT,
    web_link        TEXT,
    location        TEXT,
    is_online       INTEGER NOT NULL DEFAULT 0,
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (id, profile)
);

CREATE INDEX IF NOT EXISTS idx_meetings_start
    ON meetings (profile, start);
"#;

/// v9 schema: the message-search index (backlog item **B1**). Plain
/// `CREATE INDEX IF NOT EXISTS`, so the migration is re-entrant.
///
/// `messages(profile, timestamp)` lets a search filter by profile and walk rows
/// in recency order without a temp sort. A leading-wildcard `LIKE` cannot use an
/// index for the *match* itself, so this is a scan-support index, not a lookup
/// one — see [`Store::search_messages`] and ADR-036.
const MIGRATION_V9_INDEX: &str = r#"
CREATE INDEX IF NOT EXISTS idx_messages_profile_time
    ON messages (profile, timestamp);
"#;

/// One local account record.
///
/// `profile` is the account key the rest of the core uses (see
/// [`DEFAULT_PROFILE`](crate::auth::store::DEFAULT_PROFILE)); `tenant` and
/// `display_name` are optional because a personal account may know neither at
/// first write. `self_mri`/`oid` (schema v8, M13) cache the signed-in identity
/// resolved from Graph `/me`, so `mark_own` survives a restart without a fresh
/// whoami.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Account key (the token-store profile name).
    pub profile: String,
    /// AAD tenant id, when known.
    pub tenant: Option<String>,
    /// Whether this is a personal (consumer) account.
    pub personal: bool,
    /// Cached display name, when known.
    pub display_name: Option<String>,
    /// Cached own MRI (`8:orgid:<guid>`), when known (M13).
    pub self_mri: Option<String>,
    /// Cached AAD object id, when known (M13).
    pub oid: Option<String>,
    /// Unix seconds when the row was first written.
    pub created_at: i64,
}

/// One message-search hit as it comes out of the store (backlog item **B1**).
///
/// The store returns the raw `content` (not a snippet) and the owning
/// `chat_id`; the [`Search`](crate::search::Search) service resolves the chat
/// name and builds the snippet into a [`SearchHit`](crate::model::SearchHit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRow {
    /// Owning conversation id.
    pub chat_id: String,
    /// Server message id.
    pub message_id: String,
    /// Sender display name.
    pub sender: String,
    /// Plain-text message body.
    pub content: String,
    /// ISO-8601 arrival timestamp.
    pub timestamp: String,
    /// Whether the message was sent by the signed-in user.
    pub is_own: bool,
}

/// Row counts, for diagnostics and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Counts {
    /// Number of accounts.
    pub accounts: i64,
    /// Number of chats.
    pub chats: i64,
    /// Number of messages.
    pub messages: i64,
}

/// A synchronous, `Send + Sync` handle to the local SQLite database.
///
/// See the module docs for the concurrency model and migration discipline.
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (creating if needed) the database at `path`, applying pragmas and
    /// running migrations. Missing parent directories are created.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| CoreError::io(parent, e))?;
        }
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// Open a private in-memory database (tests, ephemeral sessions).
    ///
    /// WAL is unavailable for `:memory:`; the pragma is still issued and simply
    /// reports `memory`, which is not an error.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn)
    }

    /// Configure pragmas, migrate, then wrap the connection.
    fn from_connection(mut conn: Connection) -> Result<Self> {
        // Pragmas must be set outside a transaction.
        //
        // `journal_mode` returns a row ("wal" on disk, "memory" for :memory:),
        // so we read it rather than using `pragma_update`.
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;")?;
        conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;

        migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// The schema version recorded in `PRAGMA user_version`.
    pub fn schema_version(&self) -> Result<i32> {
        let conn = self.lock();
        Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A poisoned lock only means another caller panicked mid-query; the
        // connection itself is still usable, so recover rather than propagate.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ----- accounts -------------------------------------------------------

    /// Insert or replace an account, preserving the original `created_at`.
    pub fn put_account(&self, account: &Account) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO accounts
                (profile, tenant, personal, display_name, self_mri, oid, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(profile) DO UPDATE SET
                tenant = excluded.tenant,
                personal = excluded.personal,
                display_name = excluded.display_name,
                self_mri = excluded.self_mri,
                oid = excluded.oid",
            params![
                account.profile,
                account.tenant,
                account.personal as i64,
                account.display_name,
                account.self_mri,
                account.oid,
                account.created_at
            ],
        )?;
        Ok(())
    }

    /// Fetch one account, or `None` if it was never stored.
    pub fn get_account(&self, profile: &str) -> Result<Option<Account>> {
        let conn = self.lock();
        let account = conn
            .query_row(
                "SELECT profile, tenant, personal, display_name, self_mri, oid, created_at
                 FROM accounts WHERE profile = ?1",
                params![profile],
                |row| {
                    Ok(Account {
                        profile: row.get(0)?,
                        tenant: row.get(1)?,
                        personal: row.get::<_, i64>(2)? != 0,
                        display_name: row.get(3)?,
                        self_mri: row.get(4)?,
                        oid: row.get(5)?,
                        created_at: row.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(account)
    }

    // ----- chats ----------------------------------------------------------

    /// Insert or update chats for `profile`, keyed by `(id, profile)`.
    ///
    /// Idempotent: writing the same chat twice updates the row instead of
    /// duplicating it, and `updated_at` is advanced to the write time (never
    /// moved backwards). Returns the number of chats written.
    pub fn upsert_chats(&self, profile: &str, chats: &[Chat]) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let now = now_unix() as i64;
        let mut written = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO chats
                    (id, profile, name, is_group, last_message_time,
                     last_message_sender, last_message_preview, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id, profile) DO UPDATE SET
                    name = excluded.name,
                    is_group = excluded.is_group,
                    last_message_time = excluded.last_message_time,
                    last_message_sender = excluded.last_message_sender,
                    last_message_preview = excluded.last_message_preview,
                    updated_at = MAX(chats.updated_at, excluded.updated_at)",
            )?;
            for chat in chats {
                stmt.execute(params![
                    chat.id,
                    profile,
                    chat.name,
                    chat.is_group as i64,
                    chat.last_message_time,
                    chat.last_message_sender,
                    chat.last_message_preview,
                    now
                ])?;
                written += 1;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// List chats for `profile`, newest activity first, at most `limit`.
    ///
    /// Chats with no `last_message_time` sort last (SQLite orders `NULL` below
    /// every value, and the sort is descending).
    pub fn list_chats(&self, profile: &str, limit: usize) -> Result<Vec<Chat>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, is_group, last_message_time,
                    last_message_sender, last_message_preview
             FROM chats
             WHERE profile = ?1
             ORDER BY last_message_time DESC, id ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![profile, limit as i64], row_to_chat)?;
        let mut chats = Vec::new();
        for chat in rows {
            chats.push(chat?);
        }
        Ok(chats)
    }

    /// Fetch one chat by id within `profile`, or `None`.
    pub fn chat(&self, id: &str, profile: &str) -> Result<Option<Chat>> {
        let conn = self.lock();
        let chat = conn
            .query_row(
                "SELECT id, name, is_group, last_message_time,
                        last_message_sender, last_message_preview
                 FROM chats WHERE id = ?1 AND profile = ?2",
                params![id, profile],
                row_to_chat,
            )
            .optional()?;
        Ok(chat)
    }

    /// Delete one chat and every message belonging to it, in one transaction.
    ///
    /// Returns whether a chat row actually existed.
    pub fn delete_chat(&self, id: &str, profile: &str) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM messages WHERE chat_id = ?1 AND profile = ?2",
            params![id, profile],
        )?;
        let removed = tx.execute(
            "DELETE FROM chats WHERE id = ?1 AND profile = ?2",
            params![id, profile],
        )?;
        tx.commit()?;
        Ok(removed > 0)
    }

    // ----- messages -------------------------------------------------------

    /// Insert or update messages for `profile`, keyed by `(id, chat_id)`.
    ///
    /// Idempotent, like [`Store::upsert_messages`]'s chat sibling: the same
    /// server id updates the row (so an edit or a reaction change overwrites the
    /// cached copy) and `updated_at` advances.
    pub fn upsert_messages(&self, profile: &str, messages: &[Message]) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let now = now_unix() as i64;
        let mut written = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO messages
                    (id, chat_id, profile, sender_mri, sender_name, timestamp,
                     content, raw, reactions_json, reply_to, client_message_id,
                     edited, is_own, send_state, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT(id, chat_id) DO UPDATE SET
                    profile = excluded.profile,
                    sender_mri = excluded.sender_mri,
                    sender_name = excluded.sender_name,
                    timestamp = excluded.timestamp,
                    content = excluded.content,
                    raw = excluded.raw,
                    reactions_json = excluded.reactions_json,
                    reply_to = excluded.reply_to,
                    client_message_id = excluded.client_message_id,
                    edited = excluded.edited,
                    is_own = excluded.is_own,
                    send_state = excluded.send_state,
                    updated_at = MAX(messages.updated_at, excluded.updated_at)",
            )?;
            for msg in messages {
                let reactions = reactions_to_json(&msg.reactions)?;
                stmt.execute(params![
                    msg.id,
                    msg.chat_id,
                    profile,
                    msg.sender_mri,
                    msg.sender_name,
                    msg.timestamp,
                    msg.content,
                    msg.raw,
                    reactions,
                    msg.reply_to,
                    msg.client_message_id,
                    msg.edited as i64,
                    msg.is_own as i64,
                    msg.send_state.as_str(),
                    now
                ])?;
                written += 1;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// The newest `limit` messages of a chat, returned **oldest-first** (the
    /// order a timeline renders them).
    pub fn messages(&self, chat_id: &str, limit: usize) -> Result<Vec<Message>> {
        self.messages_query(
            "SELECT id, chat_id, sender_mri, sender_name, timestamp, content,
                    raw, reactions_json, reply_to, client_message_id, edited, is_own,
                    send_state
             FROM messages
             WHERE chat_id = ?1
             ORDER BY timestamp DESC, id DESC
             LIMIT ?2",
            params![chat_id, limit as i64],
        )
    }

    /// The newest `limit` messages **strictly older than `timestamp`**,
    /// returned oldest-first (backward paging).
    ///
    /// `timestamp` is compared as text; ISO-8601 UTC values sort correctly.
    /// Messages sharing the boundary timestamp are not returned (the wire's
    /// timestamps are millisecond-resolution in practice).
    pub fn messages_before(
        &self,
        chat_id: &str,
        timestamp: &str,
        limit: usize,
    ) -> Result<Vec<Message>> {
        self.messages_query(
            "SELECT id, chat_id, sender_mri, sender_name, timestamp, content,
                    raw, reactions_json, reply_to, client_message_id, edited, is_own,
                    send_state
             FROM messages
             WHERE chat_id = ?1 AND timestamp < ?2
             ORDER BY timestamp DESC, id DESC
             LIMIT ?3",
            params![chat_id, timestamp, limit as i64],
        )
    }

    /// Apply a realtime **edit** to a stored message in place (M4).
    ///
    /// Replaces only the plain-text `content`, sets `edited` and advances
    /// `updated_at`; every other column (sender, timestamp, raw HTML, reactions,
    /// reply link) is left untouched, so a partial realtime edit payload cannot
    /// clobber a richer cached row. Returns whether a matching
    /// `(id, chat_id, profile)` row existed.
    pub fn update_message_edit(
        &self,
        profile: &str,
        chat_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<bool> {
        let conn = self.lock();
        let now = now_unix() as i64;
        let changed = conn.execute(
            "UPDATE messages
                SET content = ?1, edited = 1, updated_at = MAX(updated_at, ?2)
              WHERE id = ?3 AND chat_id = ?4 AND profile = ?5",
            params![content, now, message_id, chat_id, profile],
        )?;
        Ok(changed > 0)
    }

    /// Delete one stored message (M4 realtime delete). Returns whether a
    /// matching `(id, chat_id, profile)` row existed.
    pub fn delete_message(&self, profile: &str, chat_id: &str, message_id: &str) -> Result<bool> {
        let conn = self.lock();
        let removed = conn.execute(
            "DELETE FROM messages WHERE id = ?1 AND chat_id = ?2 AND profile = ?3",
            params![message_id, chat_id, profile],
        )?;
        Ok(removed > 0)
    }

    /// Fetch one message by its server id within a chat, or `None`.
    ///
    /// Like [`Store::messages`], this is **not** profile-scoped (matching the
    /// M3 read API); chat ids are account-unique in practice (see U-001).
    pub fn message(&self, chat_id: &str, message_id: &str) -> Result<Option<Message>> {
        let conn = self.lock();
        let message = conn
            .query_row(
                "SELECT id, chat_id, sender_mri, sender_name, timestamp, content,
                        raw, reactions_json, reply_to, client_message_id, edited, is_own,
                        send_state
                 FROM messages WHERE chat_id = ?1 AND id = ?2",
                params![chat_id, message_id],
                row_to_message,
            )
            .optional()?;
        Ok(message)
    }

    /// Find the message carrying `client_message_id` in a chat.
    ///
    /// This is how an optimistic send reconciles its local row with the server's
    /// id (M5): the `clientmessageid` is the idempotency key, so it survives the
    /// row being re-keyed. The newest match wins when (unusually) several rows
    /// share the key.
    pub fn message_by_client_id(
        &self,
        chat_id: &str,
        client_message_id: &str,
    ) -> Result<Option<Message>> {
        let conn = self.lock();
        let message = conn
            .query_row(
                "SELECT id, chat_id, sender_mri, sender_name, timestamp, content,
                        raw, reactions_json, reply_to, client_message_id, edited, is_own,
                        send_state
                 FROM messages
                 WHERE chat_id = ?1 AND client_message_id = ?2
                 ORDER BY timestamp DESC, id DESC
                 LIMIT 1",
                params![chat_id, client_message_id],
                row_to_message,
            )
            .optional()?;
        Ok(message)
    }

    /// Update only the [`SendState`] of one stored message (M5 optimistic send).
    ///
    /// Returns whether a matching `(id, chat_id, profile)` row existed. Content
    /// and every other column are left untouched.
    pub fn update_message_send_state(
        &self,
        profile: &str,
        chat_id: &str,
        message_id: &str,
        state: SendState,
    ) -> Result<bool> {
        let conn = self.lock();
        let now = now_unix() as i64;
        let changed = conn.execute(
            "UPDATE messages
                SET send_state = ?1, updated_at = MAX(updated_at, ?2)
              WHERE id = ?3 AND chat_id = ?4 AND profile = ?5",
            params![state.as_str(), now, message_id, chat_id, profile],
        )?;
        Ok(changed > 0)
    }

    // ----- search (B1) ----------------------------------------------------

    /// Search the cached messages of `profile` for `query`, **newest first**.
    ///
    /// A bounded, local-first `LIKE` scan over `messages.content`: matching is
    /// case-insensitive for ASCII (SQLite's default `LIKE` semantics), the
    /// query's `LIKE` metacharacters (`%`, `_`, `\`) are escaped so user input
    /// matches literally, and the results are ordered by `timestamp DESC` then
    /// `rowid DESC` so ties are stable. `chat_id` optionally narrows the search
    /// to one conversation. An empty/whitespace query (or `limit == 0`) returns
    /// no rows — never "everything".
    ///
    /// The index added in schema **v9** (`messages(profile, timestamp)`) lets
    /// SQLite filter by profile and walk rows in recency order; a leading-
    /// wildcard `LIKE` still scans, which is why the cap is caller-supplied.
    /// The snippet/chat-name shaping happens in [`crate::search`].
    pub fn search_messages(
        &self,
        profile: &str,
        query: &str,
        chat_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchRow>> {
        let trimmed = query.trim();
        if trimmed.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let pattern = crate::search::like_pattern(trimmed);
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT chat_id, id, sender_name, content, timestamp, is_own
             FROM messages
             WHERE profile = ?1
               AND content LIKE ?2 ESCAPE '\\'
               AND (?3 IS NULL OR chat_id = ?3)
             ORDER BY timestamp DESC, rowid DESC
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![profile, pattern, chat_id, limit as i64],
            row_to_search_row,
        )?;
        let mut hits = Vec::new();
        for hit in rows {
            hits.push(hit?);
        }
        Ok(hits)
    }

    // ----- read state (M5) ------------------------------------------------

    /// Record that `chat_id` has been read up to `message_id`.
    ///
    /// A local-only marker: the server's consumption horizon is authoritative on
    /// the wire, and this row is what lets the UI render "read up to here"
    /// without re-deriving it. One row per `(chat_id, profile)`.
    pub fn set_read_state(&self, profile: &str, chat_id: &str, message_id: &str) -> Result<()> {
        let conn = self.lock();
        let now = now_unix() as i64;
        conn.execute(
            "INSERT INTO read_state (chat_id, profile, message_id, read_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(chat_id, profile) DO UPDATE SET
                message_id = excluded.message_id,
                read_at = excluded.read_at",
            params![chat_id, profile, message_id, now],
        )?;
        Ok(())
    }

    /// The last message id `chat_id` was marked read up to, if any.
    pub fn read_state(&self, profile: &str, chat_id: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let message_id = conn
            .query_row(
                "SELECT message_id FROM read_state WHERE chat_id = ?1 AND profile = ?2",
                params![chat_id, profile],
                |row| row.get(0),
            )
            .optional()?;
        Ok(message_id)
    }

    // ----- mute + unread (M8) ---------------------------------------------

    /// Set (or clear) the local **notification mute** for one chat (M8, schema
    /// v5).
    ///
    /// A local mirror of the server's `alerts`/mute property: the UI toggles it
    /// and reads it back, and it survives a restart. Returns whether a matching
    /// `(id, profile)` chat row existed (mute cannot be set before the chat is
    /// cached).
    pub fn set_chat_muted(&self, profile: &str, chat_id: &str, muted: bool) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE chats SET muted = ?1 WHERE id = ?2 AND profile = ?3",
            params![muted as i64, chat_id, profile],
        )?;
        Ok(changed > 0)
    }

    /// Whether one cached chat is muted for this profile (unknown chat →
    /// `false`).
    pub fn is_chat_muted(&self, profile: &str, chat_id: &str) -> Result<bool> {
        let conn = self.lock();
        let muted = conn
            .query_row(
                "SELECT muted FROM chats WHERE id = ?1 AND profile = ?2",
                params![chat_id, profile],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        Ok(muted.unwrap_or(0) != 0)
    }

    /// The ids of every muted chat for this profile (for building a
    /// [`NotificationContext`](crate::notifications::NotificationContext)).
    pub fn muted_chats(&self, profile: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT id FROM chats WHERE profile = ?1 AND muted != 0 ORDER BY id ASC")?;
        let rows = stmt.query_map(params![profile], |row| row.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for id in rows {
            ids.push(id?);
        }
        Ok(ids)
    }

    /// Set (or clear) the local **hidden** flag for one chat (B2a, schema v10).
    ///
    /// The mirror of the server's `unpinnedTime`/`historyHiddenTime` hide: a
    /// hidden chat is collapsed out of the chat list but can be shown again.
    /// Returns whether a matching `(id, profile)` chat row existed.
    pub fn set_chat_hidden(&self, profile: &str, chat_id: &str, hidden: bool) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE chats SET hidden = ?1 WHERE id = ?2 AND profile = ?3",
            params![hidden as i64, chat_id, profile],
        )?;
        Ok(changed > 0)
    }

    /// Whether one cached chat is hidden for this profile (unknown chat →
    /// `false`).
    pub fn is_chat_hidden(&self, profile: &str, chat_id: &str) -> Result<bool> {
        let conn = self.lock();
        let hidden = conn
            .query_row(
                "SELECT hidden FROM chats WHERE id = ?1 AND profile = ?2",
                params![chat_id, profile],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        Ok(hidden.unwrap_or(0) != 0)
    }

    /// The ids of every hidden chat for this profile (the collapsed section).
    pub fn hidden_chats(&self, profile: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT id FROM chats WHERE profile = ?1 AND hidden != 0 ORDER BY id ASC")?;
        let rows = stmt.query_map(params![profile], |row| row.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for id in rows {
            ids.push(id?);
        }
        Ok(ids)
    }

    /// Record the last **server-confirmed** mute state for one chat (B2a,
    /// schema v10).
    ///
    /// `Some(muted)` is what the server's `alerts` property was set to;
    /// `None` clears it back to "unknown" (the column is nullable, so a failed
    /// offline write stays `NULL` rather than lying). This is deliberately
    /// **separate** from `chats.muted`, which is the local routing flag
    /// (M8/ADR-021): the local flag is the notification-routing truth, the
    /// server state is a best-effort mirror (see ADR-038).
    pub fn set_chat_server_muted(
        &self,
        profile: &str,
        chat_id: &str,
        muted: Option<bool>,
    ) -> Result<bool> {
        let conn = self.lock();
        let value = muted.map(|m| m as i64);
        let changed = conn.execute(
            "UPDATE chats SET server_muted = ?1 WHERE id = ?2 AND profile = ?3",
            params![value, chat_id, profile],
        )?;
        Ok(changed > 0)
    }

    /// The last server-confirmed mute state for one chat, if ever recorded
    /// (`None` = unknown).
    pub fn server_muted(&self, profile: &str, chat_id: &str) -> Result<Option<bool>> {
        let conn = self.lock();
        let value = conn
            .query_row(
                "SELECT server_muted FROM chats WHERE id = ?1 AND profile = ?2",
                params![chat_id, profile],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        Ok(value.map(|v| v != 0))
    }

    /// The timestamp of the message the chat was read up to, if the horizon
    /// message is still cached (a horizon pointing at an evicted/deleted
    /// message resolves to `None`).
    fn horizon_timestamp(&self, profile: &str, chat_id: &str) -> Result<Option<String>> {
        let Some(message_id) = self.read_state(profile, chat_id)? else {
            return Ok(None);
        };
        Ok(self.message(chat_id, &message_id)?.map(|m| m.timestamp))
    }

    /// How many messages in `chat_id` are **newer than the stored read
    /// horizon** (M8) — the number an unread badge shows.
    ///
    /// With no horizon every cached message counts as unread. The same applies
    /// when the stored horizon message is no longer cached — we cannot resolve
    /// its timestamp, so the whole chat reads as unread rather than the badge
    /// silently dropping. Own messages are **not** excluded here: the caller/UI
    /// filters on [`Message::is_own`](crate::model::Message::is_own), which is
    /// unreliable for messages that only ever arrived over the raw wire (see
    /// U-001).
    pub fn unread_count(&self, profile: &str, chat_id: &str) -> Result<usize> {
        let horizon = self.horizon_timestamp(profile, chat_id)?;
        let conn = self.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages
             WHERE chat_id = ?1 AND (?2 IS NULL OR timestamp > ?2)",
            params![chat_id, horizon],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// The messages in `chat_id` newer than the stored read horizon, **newest
    /// first**, at most `limit` (M8 activity feed / unread preview).
    ///
    /// Like [`Store::unread_count`], no horizon means every message counts, and
    /// own messages are not filtered.
    pub fn unread_messages(
        &self,
        profile: &str,
        chat_id: &str,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let horizon = self.horizon_timestamp(profile, chat_id)?;
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, chat_id, sender_mri, sender_name, timestamp, content,
                    raw, reactions_json, reply_to, client_message_id, edited, is_own,
                    send_state
             FROM messages
             WHERE chat_id = ?1 AND (?2 IS NULL OR timestamp > ?2)
             ORDER BY timestamp DESC, id DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![chat_id, horizon, limit as i64], row_to_message)?;
        let mut messages = Vec::new();
        for message in rows {
            messages.push(message?);
        }
        Ok(messages)
    }

    // ----- teams, channels & members (M6) ----------------------------------

    /// Insert or update teams for `profile`, keyed by `(id, profile)`.
    ///
    /// Idempotent: the same team id updates the row and `updated_at` advances
    /// (never backwards). Returns the number of teams written.
    pub fn upsert_teams(&self, profile: &str, teams: &[Team]) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let now = now_unix() as i64;
        let mut written = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO teams (id, profile, name, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id, profile) DO UPDATE SET
                    name = excluded.name,
                    updated_at = MAX(teams.updated_at, excluded.updated_at)",
            )?;
            for team in teams {
                stmt.execute(params![team.id, profile, team.name, now])?;
                written += 1;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// List the cached teams for `profile`, name order (case-insensitive).
    pub fn list_teams(&self, profile: &str) -> Result<Vec<Team>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name FROM teams
             WHERE profile = ?1
             ORDER BY name COLLATE NOCASE ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![profile], row_to_team)?;
        let mut teams = Vec::new();
        for team in rows {
            teams.push(team?);
        }
        Ok(teams)
    }

    /// Insert or update channels, keyed by `(id, team_id)`.
    ///
    /// Idempotent. `profile` is a non-key column (matching `messages`): team and
    /// channel ids are account-unique in practice (see U-001). Returns the
    /// number of channels written.
    pub fn upsert_channels(&self, profile: &str, channels: &[Channel]) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let now = now_unix() as i64;
        let mut written = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO channels
                    (id, team_id, profile, name, description, membership_type,
                     web_url, is_channel, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id, team_id) DO UPDATE SET
                    profile = excluded.profile,
                    name = excluded.name,
                    description = excluded.description,
                    membership_type = excluded.membership_type,
                    web_url = excluded.web_url,
                    is_channel = excluded.is_channel,
                    updated_at = MAX(channels.updated_at, excluded.updated_at)",
            )?;
            for channel in channels {
                stmt.execute(params![
                    channel.id,
                    channel.team_id,
                    profile,
                    channel.name,
                    channel.description,
                    channel.membership_type,
                    channel.web_url,
                    channel.is_channel as i64,
                    now
                ])?;
                written += 1;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// The cached channels of one team, name order (case-insensitive).
    pub fn channels_for_team(&self, team_id: &str) -> Result<Vec<Channel>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, team_id, name, description, membership_type, web_url, is_channel
             FROM channels
             WHERE team_id = ?1
             ORDER BY name COLLATE NOCASE ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![team_id], row_to_channel)?;
        let mut channels = Vec::new();
        for channel in rows {
            channels.push(channel?);
        }
        Ok(channels)
    }

    /// Fetch one cached channel by `(team_id, id)`, or `None`.
    pub fn channel(&self, team_id: &str, channel_id: &str) -> Result<Option<Channel>> {
        let conn = self.lock();
        let channel = conn
            .query_row(
                "SELECT id, team_id, name, description, membership_type, web_url, is_channel
                 FROM channels WHERE team_id = ?1 AND id = ?2",
                params![team_id, channel_id],
                row_to_channel,
            )
            .optional()?;
        Ok(channel)
    }

    /// Delete one cached channel (M6 `delete_channel`). Returns whether a row
    /// existed. Team members are unaffected.
    pub fn delete_channel(&self, profile: &str, team_id: &str, channel_id: &str) -> Result<bool> {
        let conn = self.lock();
        let removed = conn.execute(
            "DELETE FROM channels WHERE id = ?1 AND team_id = ?2 AND profile = ?3",
            params![channel_id, team_id, profile],
        )?;
        Ok(removed > 0)
    }

    /// Insert or update one team's roster, keyed by `(id, team_id)`.
    ///
    /// `roles` is stored as a JSON column (like `messages.reactions_json`).
    /// Idempotent. Returns the number of members written.
    pub fn upsert_team_members(
        &self,
        profile: &str,
        team_id: &str,
        members: &[TeamMember],
    ) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let now = now_unix() as i64;
        let mut written = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO team_members
                    (id, team_id, profile, display_name, email, roles_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id, team_id) DO UPDATE SET
                    profile = excluded.profile,
                    display_name = excluded.display_name,
                    email = excluded.email,
                    roles_json = excluded.roles_json,
                    updated_at = MAX(team_members.updated_at, excluded.updated_at)",
            )?;
            for member in members {
                let roles = serde_json::to_string(&member.roles).map_err(CoreError::from)?;
                stmt.execute(params![
                    member.id,
                    team_id,
                    profile,
                    member.display_name,
                    member.email,
                    roles,
                    now
                ])?;
                written += 1;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// The cached roster of one team, display-name order (case-insensitive).
    pub fn team_members(&self, team_id: &str) -> Result<Vec<TeamMember>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, display_name, email, roles_json
             FROM team_members
             WHERE team_id = ?1
             ORDER BY display_name COLLATE NOCASE ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![team_id], row_to_team_member)?;
        let mut members = Vec::new();
        for member in rows {
            members.push(member?);
        }
        Ok(members)
    }

    // ----- files (M7) -----------------------------------------------------

    /// Insert or update cached driveItems for `profile`, keyed `(id, profile)`.
    ///
    /// Idempotent. `chat_id` is the chat a file was listed under (recents and
    /// folder listings pass `None`). Metadata is **additive**: `chat_id`,
    /// `drive_id`, `mime`, `web_url`, `download_url`, `sender`, `created`,
    /// `modified`, `share_url` and `attachment_id` only overwrite the stored
    /// value when the incoming one is non-`NULL` (`COALESCE`), so a later
    /// recents/folder listing cannot wipe the attachment id or sharing link a
    /// chat listing/copy-link filled in. `name`/`size`/`is_folder` take the
    /// incoming value and `updated_at` only advances. Returns the number of
    /// files written.
    pub fn upsert_files(
        &self,
        profile: &str,
        chat_id: Option<&str>,
        files: &[FileEntry],
    ) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let now = now_unix() as i64;
        let mut written = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO files
                    (id, profile, drive_id, chat_id, name, size, mime, web_url,
                     download_url, is_folder, created, modified, sender,
                     share_url, attachment_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                         ?13, ?14, ?15, ?16)
                 ON CONFLICT(id, profile) DO UPDATE SET
                    drive_id = COALESCE(excluded.drive_id, files.drive_id),
                    chat_id = COALESCE(excluded.chat_id, files.chat_id),
                    name = excluded.name,
                    size = excluded.size,
                    mime = COALESCE(excluded.mime, files.mime),
                    web_url = COALESCE(excluded.web_url, files.web_url),
                    download_url = COALESCE(excluded.download_url, files.download_url),
                    is_folder = excluded.is_folder,
                    created = COALESCE(excluded.created, files.created),
                    modified = COALESCE(excluded.modified, files.modified),
                    sender = COALESCE(excluded.sender, files.sender),
                    share_url = COALESCE(excluded.share_url, files.share_url),
                    attachment_id = COALESCE(excluded.attachment_id, files.attachment_id),
                    updated_at = MAX(files.updated_at, excluded.updated_at)",
            )?;
            for file in files {
                stmt.execute(params![
                    file.id,
                    profile,
                    file.drive_id,
                    chat_id,
                    file.name,
                    file.size as i64,
                    file.mime,
                    file.web_url,
                    file.download_url,
                    file.is_folder as i64,
                    file.created,
                    file.modified,
                    file.sender,
                    file.share_url,
                    file.attachment_id,
                    now
                ])?;
                written += 1;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// The cached files of one chat, newest-modified first (then name, id).
    pub fn files_for_chat(&self, profile: &str, chat_id: &str) -> Result<Vec<FileEntry>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, drive_id, name, size, mime, web_url, download_url,
                    is_folder, created, modified, sender, share_url, attachment_id
             FROM files
             WHERE profile = ?1 AND chat_id = ?2
             ORDER BY modified DESC, name COLLATE NOCASE ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![profile, chat_id], row_to_file)?;
        let mut files = Vec::new();
        for file in rows {
            files.push(file?);
        }
        Ok(files)
    }

    /// Fetch one cached driveItem by `(drive_id, id)` within `profile`, or
    /// `None`. The key is `(id, profile)`; the drive is an additional filter so
    /// a drive-scoped lookup never returns another drive's item.
    pub fn file(&self, profile: &str, drive_id: &str, item_id: &str) -> Result<Option<FileEntry>> {
        let conn = self.lock();
        let file = conn
            .query_row(
                "SELECT id, drive_id, name, size, mime, web_url, download_url,
                        is_folder, created, modified, sender, share_url, attachment_id
                 FROM files
                 WHERE profile = ?1 AND drive_id = ?2 AND id = ?3",
                params![profile, drive_id, item_id],
                row_to_file,
            )
            .optional()?;
        Ok(file)
    }

    /// Delete one cached driveItem by `(drive_id, id)`. Returns whether a row
    /// existed.
    pub fn delete_file(&self, profile: &str, drive_id: &str, item_id: &str) -> Result<bool> {
        let conn = self.lock();
        let removed = conn.execute(
            "DELETE FROM files WHERE id = ?1 AND drive_id = ?2 AND profile = ?3",
            params![item_id, drive_id, profile],
        )?;
        Ok(removed > 0)
    }

    // ----- call log (M9) --------------------------------------------------

    /// Insert or update local call-history rows for `profile`, keyed
    /// `(id, profile)`.
    ///
    /// Idempotent: a call is first written when it starts (`ended_at`/
    /// `duration_secs` `NULL`, outcome `unknown`) and updated in place when it
    /// ends, so one call is one row. `updated_at` only advances. Returns the
    /// number of rows written.
    pub fn upsert_call_log(&self, profile: &str, entries: &[CallLogEntry]) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let now = now_unix() as i64;
        let mut written = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO call_log
                    (id, profile, thread_id, direction, started_at, ended_at,
                     duration_secs, participants_json, outcome, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(id, profile) DO UPDATE SET
                    thread_id = excluded.thread_id,
                    direction = excluded.direction,
                    started_at = excluded.started_at,
                    ended_at = excluded.ended_at,
                    duration_secs = excluded.duration_secs,
                    participants_json = excluded.participants_json,
                    outcome = excluded.outcome,
                    updated_at = MAX(call_log.updated_at, excluded.updated_at)",
            )?;
            for entry in entries {
                let participants =
                    serde_json::to_string(&entry.participants).map_err(CoreError::from)?;
                stmt.execute(params![
                    entry.id,
                    profile,
                    entry.thread_id,
                    entry.direction.as_str(),
                    entry.started_at,
                    entry.ended_at,
                    entry.duration_secs.map(|d| d as i64),
                    participants,
                    entry.outcome.as_str(),
                    now
                ])?;
                written += 1;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// The newest `limit` call-history rows for `profile`, newest first.
    ///
    /// Ties (same `started_at`) fall back to the id so the order is stable.
    pub fn call_log(&self, profile: &str, limit: usize) -> Result<Vec<CallLogEntry>> {
        self.call_log_query(
            "SELECT id, thread_id, direction, started_at, ended_at, duration_secs,
                    participants_json, outcome
             FROM call_log
             WHERE profile = ?1
             ORDER BY started_at DESC, id DESC
             LIMIT ?2",
            params![profile, limit as i64],
        )
    }

    /// The newest `limit` call-history rows for one thread, newest first.
    pub fn call_log_for_thread(
        &self,
        profile: &str,
        thread_id: &str,
        limit: usize,
    ) -> Result<Vec<CallLogEntry>> {
        self.call_log_query(
            "SELECT id, thread_id, direction, started_at, ended_at, duration_secs,
                    participants_json, outcome
             FROM call_log
             WHERE profile = ?1 AND thread_id = ?2
             ORDER BY started_at DESC, id DESC
             LIMIT ?3",
            params![profile, thread_id, limit as i64],
        )
    }

    /// Fetch one call-history row by id within `profile`, or `None`.
    pub fn call_log_entry(&self, profile: &str, id: &str) -> Result<Option<CallLogEntry>> {
        let conn = self.lock();
        let entry = conn
            .query_row(
                "SELECT id, thread_id, direction, started_at, ended_at, duration_secs,
                        participants_json, outcome
                 FROM call_log WHERE id = ?1 AND profile = ?2",
                params![id, profile],
                row_to_call_log,
            )
            .optional()?;
        Ok(entry)
    }

    fn call_log_query(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<CallLogEntry>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params, row_to_call_log)?;
        let mut entries = Vec::new();
        for entry in rows {
            entries.push(entry?);
        }
        Ok(entries)
    }

    // ----- calendar meetings (M12) ----------------------------------------

    /// Insert or update cached meetings for `profile`, keyed `(id, profile)`.
    ///
    /// Idempotent: the same event id updates the row in place and `updated_at`
    /// only advances. Every carried field is overwritten (a re-fetch of the same
    /// event is authoritative); the five uncarried columns round-trip whatever
    /// was stored. Returns the number of meetings written.
    pub fn upsert_meetings(&self, profile: &str, meetings: &[Meeting]) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let now = now_unix() as i64;
        let mut written = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO meetings
                    (id, profile, subject, start, end, all_day, organizer,
                     organizer_email, is_organizer, join_url, join_meeting_id,
                     passcode, web_link, location, is_online, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                 ON CONFLICT(id, profile) DO UPDATE SET
                    subject = excluded.subject,
                    start = excluded.start,
                    end = excluded.end,
                    all_day = excluded.all_day,
                    organizer = excluded.organizer,
                    organizer_email = excluded.organizer_email,
                    is_organizer = excluded.is_organizer,
                    join_url = excluded.join_url,
                    join_meeting_id = excluded.join_meeting_id,
                    passcode = excluded.passcode,
                    web_link = excluded.web_link,
                    location = excluded.location,
                    is_online = excluded.is_online,
                    updated_at = MAX(meetings.updated_at, excluded.updated_at)",
            )?;
            for meeting in meetings {
                stmt.execute(params![
                    meeting.id,
                    profile,
                    meeting.subject,
                    meeting.start,
                    meeting.end,
                    meeting.all_day as i64,
                    meeting.organizer,
                    meeting.organizer_email,
                    meeting.is_organizer as i64,
                    meeting.join_url,
                    meeting.join_meeting_id,
                    meeting.passcode,
                    meeting.web_link,
                    meeting.location,
                    meeting.is_online as i64,
                    now
                ])?;
                written += 1;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// The cached meetings for `profile` starting at or after `from_secs`,
    /// soonest first, at most `limit`.
    ///
    /// `from_secs` is rendered to a UTC ISO-8601 string and compared as text
    /// against the stored Graph `start` value (the wire carries no timezone; see
    /// the v7 note). Meetings with no `start` are not "upcoming" and are
    /// excluded. Pass `0` to list every cached meeting that has a start.
    pub fn upcoming_meetings(
        &self,
        profile: &str,
        from_secs: i64,
        limit: usize,
    ) -> Result<Vec<Meeting>> {
        let from = iso8601_from_unix(from_secs);
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, subject, start, end, all_day, organizer, organizer_email,
                    is_organizer, join_url, join_meeting_id, passcode, web_link,
                    location, is_online
             FROM meetings
             WHERE profile = ?1 AND start IS NOT NULL AND start >= ?2
             ORDER BY start ASC, id ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![profile, from, limit as i64], row_to_meeting)?;
        let mut meetings = Vec::new();
        for meeting in rows {
            meetings.push(meeting?);
        }
        Ok(meetings)
    }

    /// Fetch one cached meeting by id within `profile`, or `None`.
    pub fn meeting(&self, id: &str, profile: &str) -> Result<Option<Meeting>> {
        let conn = self.lock();
        let meeting = conn
            .query_row(
                "SELECT id, subject, start, end, all_day, organizer, organizer_email,
                        is_organizer, join_url, join_meeting_id, passcode, web_link,
                        location, is_online
                 FROM meetings WHERE id = ?1 AND profile = ?2",
                params![id, profile],
                row_to_meeting,
            )
            .optional()?;
        Ok(meeting)
    }

    /// Delete one cached meeting (M12 `cancel`). Returns whether a row existed.
    pub fn delete_meeting(&self, profile: &str, id: &str) -> Result<bool> {
        let conn = self.lock();
        let removed = conn.execute(
            "DELETE FROM meetings WHERE id = ?1 AND profile = ?2",
            params![id, profile],
        )?;
        Ok(removed > 0)
    }

    /// Shared message query: rows come back newest-first; reverse for display.
    fn messages_query(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Message>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params, row_to_message)?;
        let mut messages = Vec::new();
        for msg in rows {
            messages.push(msg?);
        }
        messages.reverse();
        Ok(messages)
    }

    // ----- diagnostics ----------------------------------------------------

    /// Row counts across the three tables.
    pub fn counts(&self) -> Result<Counts> {
        let conn = self.lock();
        let count = |table: &str| -> Result<i64> {
            Ok(conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?)
        };
        Ok(Counts {
            accounts: count("accounts")?,
            chats: count("chats")?,
            messages: count("messages")?,
        })
    }
}

/// `Debug` for diagnostics: shows the schema version, never the path or the
/// contents (the handle does not even keep the path).
impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("schema_version", &self.schema_version().ok())
            .finish_non_exhaustive()
    }
}

/// Apply pending migrations in a single transaction.
///
/// Forward-only: we never downgrade. A newer `user_version` than this build
/// supports is a hard error so a future schema is never opened by an old build.
fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(CoreError::StoreSchemaVersion {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    if current == SCHEMA_VERSION {
        return Ok(());
    }

    let tx = conn.transaction()?;
    if current < 1 {
        tx.execute_batch(MIGRATION_V1)?;
    }
    if current < 2 {
        migrate_v2(&tx)?;
    }
    if current < 3 {
        migrate_v3(&tx)?;
    }
    if current < 4 {
        migrate_v4(&tx)?;
    }
    if current < 5 {
        migrate_v5(&tx)?;
    }
    if current < 6 {
        migrate_v6(&tx)?;
    }
    if current < 7 {
        migrate_v7(&tx)?;
    }
    if current < 8 {
        migrate_v8(&tx)?;
    }
    if current < 9 {
        migrate_v9(&tx)?;
    }
    if current < 10 {
        migrate_v10(&tx)?;
    }
    // Record the version *after* the DDL inside the same transaction, so a
    // crash can never leave a version marker ahead of the schema.
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

/// Apply the v2 additions: the `messages.send_state` column and `read_state`.
///
/// Idempotent by construction. A database whose `user_version` was reset (or a
/// foreign file carrying our tables) may already have the column, so the
/// `ALTER TABLE` is guarded by a `PRAGMA table_info` probe rather than relying
/// on the version marker alone.
fn migrate_v2(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    if !column_exists(tx, "messages", "send_state")? {
        tx.execute_batch(
            "ALTER TABLE messages ADD COLUMN send_state TEXT NOT NULL DEFAULT 'sent';",
        )?;
    }
    tx.execute_batch(MIGRATION_V2_TABLES)?;
    Ok(())
}

/// Apply the v3 additions: the `teams`, `channels` and `team_members` tables.
///
/// Every statement is `CREATE TABLE/INDEX IF NOT EXISTS`, so the migration is
/// re-entrant even if the version marker was reset.
fn migrate_v3(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(MIGRATION_V3_TABLES)?;
    Ok(())
}

/// Apply the v4 additions: the `files` driveItem cache.
///
/// Every statement is `CREATE TABLE/INDEX IF NOT EXISTS`, so the migration is
/// re-entrant even if the version marker was reset.
fn migrate_v4(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(MIGRATION_V4_TABLES)?;
    Ok(())
}

/// Apply the v5 additions: the `chats.muted` local mute flag (M8).
///
/// Like the v2 `send_state` column, `ALTER TABLE … ADD COLUMN` has no
/// `IF NOT EXISTS`, so the probe guards re-running after a version reset.
fn migrate_v5(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    if !column_exists(tx, "chats", "muted")? {
        tx.execute_batch("ALTER TABLE chats ADD COLUMN muted INTEGER NOT NULL DEFAULT 0;")?;
    }
    Ok(())
}

/// Apply the v6 additions: the `call_log` call-history table (M9).
///
/// Every statement is `CREATE TABLE/INDEX IF NOT EXISTS`, so the migration is
/// re-entrant even if the version marker was reset.
fn migrate_v6(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(MIGRATION_V6_TABLES)?;
    Ok(())
}

/// Apply the v7 additions: the `meetings` calendar cache (M12).
///
/// Every statement is `CREATE TABLE/INDEX IF NOT EXISTS`, so the migration is
/// re-entrant even if the version marker was reset.
fn migrate_v7(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(MIGRATION_V7_TABLES)?;
    Ok(())
}

/// Apply the v8 additions: the signed-in identity columns on `accounts` (M13).
///
/// Like the v2 `send_state` / v5 `muted` columns, `ALTER TABLE … ADD COLUMN` has
/// no `IF NOT EXISTS`, so each probe guards re-running after a version reset.
fn migrate_v8(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    if !column_exists(tx, "accounts", "self_mri")? {
        tx.execute_batch("ALTER TABLE accounts ADD COLUMN self_mri TEXT;")?;
    }
    if !column_exists(tx, "accounts", "oid")? {
        tx.execute_batch("ALTER TABLE accounts ADD COLUMN oid TEXT;")?;
    }
    Ok(())
}

/// Apply the v9 additions: the message-search index on `messages` (B1).
///
/// `CREATE INDEX IF NOT EXISTS`, so the migration is re-entrant even if the
/// version marker was reset.
fn migrate_v9(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(MIGRATION_V9_INDEX)?;
    Ok(())
}

/// Apply the v10 additions: the `chats.hidden` and `chats.server_muted`
/// columns (B2a).
///
/// Like the v2 `send_state` / v5 `muted` / v8 identity columns,
/// `ALTER TABLE … ADD COLUMN` has no `IF NOT EXISTS`, so each probe guards
/// re-running after a version reset. `hidden` defaults `0` (visible);
/// `server_muted` is **nullable** — `NULL` means the server mute state is
/// unknown (never written, or the write failed offline), which is distinct
/// from `0` (the server confirmed unmuted).
fn migrate_v10(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    if !column_exists(tx, "chats", "hidden")? {
        tx.execute_batch("ALTER TABLE chats ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0;")?;
    }
    if !column_exists(tx, "chats", "server_muted")? {
        tx.execute_batch("ALTER TABLE chats ADD COLUMN server_muted INTEGER;")?;
    }
    Ok(())
}

/// Whether `table` already has a column named `column` (case-insensitive).
fn column_exists(conn: &rusqlite::Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name.eq_ignore_ascii_case(column) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn row_to_chat(row: &rusqlite::Row<'_>) -> rusqlite::Result<Chat> {
    Ok(Chat {
        id: row.get(0)?,
        name: row.get(1)?,
        is_group: row.get::<_, i64>(2)? != 0,
        last_message_time: row.get(3)?,
        last_message_sender: row.get(4)?,
        last_message_preview: row.get(5)?,
        // Local-only flags are not persisted in v1.
        unread: false,
        pinned: false,
    })
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let reactions_json: String = row.get(7)?;
    Ok(Message {
        id: row.get(0)?,
        chat_id: row.get(1)?,
        sender_mri: row.get(2)?,
        sender_name: row.get(3)?,
        timestamp: row.get(4)?,
        content: row.get(5)?,
        raw: row.get(6)?,
        reactions: reactions_from_json(&reactions_json),
        reply_to: row.get(8)?,
        client_message_id: row.get(9)?,
        edited: row.get::<_, i64>(10)? != 0,
        is_own: row.get::<_, i64>(11)? != 0,
        send_state: SendState::from_db(&row.get::<_, String>(12)?),
    })
}

fn row_to_search_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SearchRow> {
    Ok(SearchRow {
        chat_id: row.get(0)?,
        message_id: row.get(1)?,
        sender: row.get(2)?,
        content: row.get(3)?,
        timestamp: row.get(4)?,
        is_own: row.get::<_, i64>(5)? != 0,
    })
}

fn row_to_team(row: &rusqlite::Row<'_>) -> rusqlite::Result<Team> {
    Ok(Team {
        id: row.get(0)?,
        name: row.get(1)?,
    })
}

fn row_to_channel(row: &rusqlite::Row<'_>) -> rusqlite::Result<Channel> {
    Ok(Channel {
        id: row.get(0)?,
        team_id: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        membership_type: row.get(4)?,
        web_url: row.get(5)?,
        is_channel: row.get::<_, i64>(6)? != 0,
    })
}

fn row_to_team_member(row: &rusqlite::Row<'_>) -> rusqlite::Result<TeamMember> {
    let roles_json: String = row.get(3)?;
    Ok(TeamMember {
        id: row.get(0)?,
        display_name: row.get(1)?,
        email: row.get(2)?,
        roles: roles_from_json(&roles_json),
    })
}

fn row_to_file(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileEntry> {
    Ok(FileEntry {
        id: row.get(0)?,
        drive_id: row.get(1)?,
        name: row.get(2)?,
        size: row.get::<_, i64>(3)? as u64,
        mime: row.get(4)?,
        web_url: row.get(5)?,
        download_url: row.get(6)?,
        is_folder: row.get::<_, i64>(7)? != 0,
        created: row.get(8)?,
        modified: row.get(9)?,
        sender: row.get(10)?,
        share_url: row.get(11)?,
        attachment_id: row.get(12)?,
    })
}

fn row_to_call_log(row: &rusqlite::Row<'_>) -> rusqlite::Result<CallLogEntry> {
    let participants_json: String = row.get(6)?;
    Ok(CallLogEntry {
        id: row.get(0)?,
        thread_id: row.get(1)?,
        direction: CallDirection::from_db(&row.get::<_, String>(2)?),
        started_at: row.get(3)?,
        ended_at: row.get(4)?,
        duration_secs: row.get::<_, Option<i64>>(5)?.map(|d| d.max(0) as u64),
        participants: participants_from_json(&participants_json),
        outcome: CallOutcome::from_db(&row.get::<_, String>(7)?),
    })
}

fn row_to_meeting(row: &rusqlite::Row<'_>) -> rusqlite::Result<Meeting> {
    Ok(Meeting {
        id: row.get(0)?,
        subject: row.get(1)?,
        start: row.get(2)?,
        end: row.get(3)?,
        all_day: row.get::<_, i64>(4)? != 0,
        organizer: row.get(5)?,
        organizer_email: row.get(6)?,
        is_organizer: row.get::<_, i64>(7)? != 0,
        join_url: row.get(8)?,
        join_meeting_id: row.get(9)?,
        passcode: row.get(10)?,
        web_link: row.get(11)?,
        location: row.get(12)?,
        is_online: row.get::<_, i64>(13)? != 0,
    })
}

/// Unix seconds → `YYYY-MM-DDTHH:MM:SSZ` (UTC, std only).
///
/// Used to turn [`Store::upcoming_meetings`]'s `from_secs` into the same
/// lexicographic space as the stored Graph `start` strings.
fn iso8601_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days + 719_468);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 0000-03-01 → (year, month, day). Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Serialize reactions for storage; the only fallible part of a message write.
fn reactions_to_json(reactions: &[Reaction]) -> Result<String> {
    serde_json::to_string(reactions).map_err(CoreError::from)
}

/// Parse a stored reactions blob. A corrupt/foreign blob degrades to "no
/// reactions" rather than failing the whole read — a message is still useful
/// without its reactions.
fn reactions_from_json(text: &str) -> Vec<Reaction> {
    serde_json::from_str(text).unwrap_or_default()
}

/// Parse a stored roles blob. Like reactions, a corrupt blob degrades to "no
/// roles" rather than failing the roster read.
fn roles_from_json(text: &str) -> Vec<String> {
    serde_json::from_str(text).unwrap_or_default()
}

/// Parse a stored call participants blob. Like reactions/roles, a corrupt blob
/// degrades to "no participants" rather than failing the call-history read.
fn participants_from_json(text: &str) -> Vec<String> {
    serde_json::from_str(text).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("squads-store-{tag}-{}", uuid::Uuid::new_v4()))
            .join("store.sqlite")
    }

    fn chat(id: &str, name: &str, time: Option<&str>) -> Chat {
        Chat {
            id: id.into(),
            name: name.into(),
            is_group: false,
            last_message_time: time.map(str::to_string),
            last_message_sender: Some("Alice".into()),
            last_message_preview: Some("hi".into()),
            unread: false,
            pinned: false,
        }
    }

    fn message(id: &str, chat_id: &str, ts: &str) -> Message {
        Message {
            id: id.into(),
            chat_id: chat_id.into(),
            sender_mri: "8:orgid:aaaa".into(),
            sender_name: "Alice".into(),
            timestamp: ts.into(),
            content: format!("body {id}"),
            raw: format!("<p>body {id}</p>"),
            reactions: Vec::new(),
            reply_to: None,
            client_message_id: None,
            edited: false,
            is_own: false,
            send_state: SendState::Sent,
        }
    }

    #[test]
    fn store_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Store>();
    }

    #[test]
    fn in_memory_opens_and_reaches_v1() {
        let store = Store::open_in_memory().expect("open");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(store.counts().unwrap(), Counts::default());
    }

    #[test]
    fn fresh_file_db_reaches_v1_and_reopen_is_idempotent() {
        let path = temp_path("fresh");
        {
            let store = Store::open(&path).expect("open");
            assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
            store
                .upsert_chats(
                    "default",
                    &[chat("c1", "One", Some("2026-01-01T00:00:00Z"))],
                )
                .unwrap();
        }
        // Reopening runs `migrate` again; it must be a no-op, not an error.
        {
            let store = Store::open(&path).expect("reopen");
            assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
            assert_eq!(store.counts().unwrap().chats, 1);
        }
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn migration_is_reentrant_when_user_version_is_reset() {
        // Simulates a database whose version marker was lost (or a foreign file
        // that happens to carry our tables): re-running v1 must be safe.
        let path = temp_path("reentrant");
        let store = Store::open(&path).expect("open");
        store
            .upsert_chats("default", &[chat("c1", "One", None)])
            .unwrap();
        {
            let conn = store.lock();
            conn.pragma_update(None, "user_version", 0).unwrap();
        }
        drop(store);

        let store = Store::open(&path).expect("reopen after reset");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(
            store.counts().unwrap().chats,
            1,
            "data survives re-migration"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn newer_foreign_schema_version_is_a_clear_error() {
        let path = temp_path("foreign");
        let store = Store::open(&path).expect("open");
        {
            let conn = store.lock();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 41)
                .unwrap();
        }
        drop(store);

        let err = Store::open(&path).unwrap_err();
        match err {
            CoreError::StoreSchemaVersion { found, supported } => {
                assert_eq!(found, SCHEMA_VERSION + 41);
                assert_eq!(supported, SCHEMA_VERSION);
            }
            other => panic!("expected StoreSchemaVersion, got {other:?}"),
        }
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn chat_upsert_is_idempotent_and_ordered_by_last_message_time() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_chats(
                "default",
                &[
                    chat("old", "Old", Some("2026-01-01T00:00:00Z")),
                    chat("new", "New", Some("2026-01-03T00:00:00Z")),
                    chat("mid", "Mid", Some("2026-01-02T00:00:00Z")),
                    chat("none", "None", None),
                ],
            )
            .unwrap();
        assert_eq!(store.counts().unwrap().chats, 4);

        // Re-upsert one chat with an updated name/preview: still four rows.
        let mut updated = chat("mid", "Mid renamed", Some("2026-01-04T00:00:00Z"));
        updated.last_message_preview = Some("newer".into());
        store.upsert_chats("default", &[updated]).unwrap();
        assert_eq!(store.counts().unwrap().chats, 4, "no duplicate row");

        let chats = store.list_chats("default", 10).unwrap();
        let ids: Vec<&str> = chats.iter().map(|c| c.id.as_str()).collect();
        // Newest first; the NULL-time chat sorts last.
        assert_eq!(ids, vec!["mid", "new", "old", "none"]);
        assert_eq!(chats[0].name, "Mid renamed");
        assert_eq!(chats[0].last_message_preview.as_deref(), Some("newer"));

        // `limit` is honoured.
        assert_eq!(store.list_chats("default", 2).unwrap().len(), 2);
        // `chat()` fetches one.
        assert_eq!(store.chat("new", "default").unwrap().unwrap().name, "New");
        assert!(store.chat("missing", "default").unwrap().is_none());
    }

    #[test]
    fn message_round_trips_reactions_reply_and_edited() {
        let store = Store::open_in_memory().unwrap();
        let mut msg = message("m1", "c1", "2026-01-01T00:00:00Z");
        msg.reactions = vec![Reaction {
            emoji: "👍".into(),
            count: 2,
            reactors: vec![
                crate::model::Reactor {
                    id: "8:orgid:bbbb".into(),
                    name: "Bob".into(),
                },
                crate::model::Reactor {
                    id: "8:orgid:cccc".into(),
                    name: String::new(),
                },
            ],
        }];
        msg.reply_to = Some("m0".into());
        msg.client_message_id = Some("123".into());
        msg.edited = true;
        msg.is_own = true;

        store.upsert_messages("default", &[msg.clone()]).unwrap();
        let got = store.messages("c1", 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], msg);

        // Upsert again (an edit): same row, no duplicate.
        let mut edited = msg.clone();
        edited.content = "edited body".into();
        store.upsert_messages("default", &[edited.clone()]).unwrap();
        assert_eq!(store.counts().unwrap().messages, 1);
        assert_eq!(store.messages("c1", 10).unwrap()[0].content, "edited body");
    }

    #[test]
    fn corrupt_reactions_json_degrades_to_empty() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_messages("default", &[message("m1", "c1", "t1")])
            .unwrap();
        {
            let conn = store.lock();
            conn.execute(
                "UPDATE messages SET reactions_json = 'not json' WHERE id = 'm1'",
                [],
            )
            .unwrap();
        }
        let got = store.messages("c1", 10).unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].reactions.is_empty());
    }

    #[test]
    fn messages_are_returned_oldest_first_and_limited() {
        let store = Store::open_in_memory().unwrap();
        let msgs = vec![
            message("a", "c1", "2026-01-01T00:00:00Z"),
            message("b", "c1", "2026-01-02T00:00:00Z"),
            message("c", "c1", "2026-01-03T00:00:00Z"),
        ];
        store.upsert_messages("default", &msgs).unwrap();

        let newest_two = store.messages("c1", 2).unwrap();
        assert_eq!(
            newest_two.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c"],
            "newest two, oldest-first"
        );
        let all = store.messages("c1", 10).unwrap();
        assert_eq!(
            all.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn messages_before_pages_backward_in_order() {
        let store = Store::open_in_memory().unwrap();
        let msgs = vec![
            message("a", "c1", "2026-01-01T00:00:00Z"),
            message("b", "c1", "2026-01-02T00:00:00Z"),
            message("c", "c1", "2026-01-03T00:00:00Z"),
            message("d", "c1", "2026-01-04T00:00:00Z"),
        ];
        store.upsert_messages("default", &msgs).unwrap();

        // Older than d, newest two → c then b (oldest-first).
        let page = store
            .messages_before("c1", "2026-01-04T00:00:00Z", 2)
            .unwrap();
        assert_eq!(
            page.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c"]
        );
        // Older than b → just a.
        let page = store
            .messages_before("c1", "2026-01-02T00:00:00Z", 2)
            .unwrap();
        assert_eq!(
            page.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["a"]
        );
        // Nothing before the oldest.
        assert!(
            store
                .messages_before("c1", "2026-01-01T00:00:00Z", 2)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn update_message_edit_touches_content_only_and_reports_misses() {
        let store = Store::open_in_memory().unwrap();
        let mut msg = message("m1", "c1", "t1");
        msg.reactions = vec![Reaction {
            emoji: "👍".into(),
            count: 1,
            reactors: Vec::new(),
        }];
        store.upsert_messages("default", &[msg]).unwrap();

        assert!(
            store
                .update_message_edit("default", "c1", "m1", "changed")
                .unwrap()
        );
        let got = store.messages("c1", 10).unwrap();
        assert_eq!(got.len(), 1, "an edit must not add a row");
        assert_eq!(got[0].content, "changed");
        assert!(got[0].edited);
        assert_eq!(got[0].sender_name, "Alice", "sender preserved");
        assert_eq!(got[0].reactions.len(), 1, "reactions preserved");

        // A miss is reported as `false`, not an error.
        assert!(
            !store
                .update_message_edit("default", "c1", "missing", "x")
                .unwrap()
        );
        assert!(
            !store
                .update_message_edit("other-profile", "c1", "m1", "x")
                .unwrap()
        );
    }

    #[test]
    fn delete_message_removes_only_the_named_row() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_messages(
                "default",
                &[message("a", "c1", "t1"), message("b", "c1", "t2")],
            )
            .unwrap();

        assert!(store.delete_message("default", "c1", "a").unwrap());
        let left = store.messages("c1", 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, "b");
        assert!(!store.delete_message("default", "c1", "a").unwrap());
    }

    #[test]
    fn profiles_are_isolated() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_chats(
                "work",
                &[chat("shared", "Work chat", Some("2026-01-01T00:00:00Z"))],
            )
            .unwrap();
        store
            .upsert_chats(
                "personal",
                &[chat(
                    "shared",
                    "Personal chat",
                    Some("2026-01-02T00:00:00Z"),
                )],
            )
            .unwrap();
        store
            .upsert_messages("work", &[message("m", "shared", "2026-01-01T00:00:00Z")])
            .unwrap();
        store
            .upsert_messages(
                "personal",
                &[message("m2", "shared", "2026-01-02T00:00:00Z")],
            )
            .unwrap();

        assert_eq!(store.list_chats("work", 10).unwrap()[0].name, "Work chat");
        assert_eq!(
            store.list_chats("personal", 10).unwrap()[0].name,
            "Personal chat"
        );
        assert_eq!(
            store.chat("shared", "work").unwrap().unwrap().name,
            "Work chat"
        );

        // Deleting one profile's chat leaves the other's intact.
        assert!(store.delete_chat("shared", "work").unwrap());
        assert!(store.chat("shared", "work").unwrap().is_none());
        assert!(store.chat("shared", "personal").unwrap().is_some());
        // The work profile's message for that chat is gone; the personal
        // profile's message (same chat id, different profile) survives.
        let left = store.messages("shared", 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, "m2");
        assert!(!store.delete_chat("shared", "work").unwrap());
    }

    #[test]
    fn accounts_round_trip_and_keep_created_at() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.get_account("default").unwrap().is_none());
        let account = Account {
            profile: "default".into(),
            tenant: Some("contoso.onmicrosoft.com".into()),
            personal: false,
            display_name: Some("Alice".into()),
            self_mri: Some("8:orgid:aaaa".into()),
            oid: Some("aaaa".into()),
            created_at: 1_700_000_000,
        };
        store.put_account(&account).unwrap();
        assert_eq!(store.get_account("default").unwrap().unwrap(), account);

        // Re-put with changed display name: created_at preserved.
        let mut updated = account.clone();
        updated.display_name = Some("Alice Smith".into());
        updated.created_at = 1_800_000_000;
        store.put_account(&updated).unwrap();
        let back = store.get_account("default").unwrap().unwrap();
        assert_eq!(back.display_name.as_deref(), Some("Alice Smith"));
        assert_eq!(back.created_at, 1_700_000_000, "created_at is preserved");
        // The M13 identity columns round-trip too.
        assert_eq!(back.self_mri.as_deref(), Some("8:orgid:aaaa"));
        assert_eq!(back.oid.as_deref(), Some("aaaa"));
    }

    #[test]
    fn counts_reflect_writes() {
        let store = Store::open_in_memory().unwrap();
        store
            .put_account(&Account {
                profile: "default".into(),
                tenant: None,
                personal: true,
                display_name: None,
                self_mri: None,
                oid: None,
                created_at: 1,
            })
            .unwrap();
        store
            .upsert_chats("default", &[chat("c1", "One", None)])
            .unwrap();
        store
            .upsert_messages("default", &[message("m1", "c1", "t")])
            .unwrap();
        let counts = store.counts().unwrap();
        assert_eq!(counts.accounts, 1);
        assert_eq!(counts.chats, 1);
        assert_eq!(counts.messages, 1);
    }

    #[test]
    fn updated_at_advances_across_upserts() {
        let store = Store::open_in_memory().unwrap();
        let before = now_unix() as i64;
        store
            .upsert_chats("default", &[chat("c1", "One", None)])
            .unwrap();
        let first: i64 = {
            let conn = store.lock();
            conn.query_row("SELECT updated_at FROM chats WHERE id='c1'", [], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert!(first >= before);
        store
            .upsert_chats("default", &[chat("c1", "One again", None)])
            .unwrap();
        let second: i64 = {
            let conn = store.lock();
            conn.query_row("SELECT updated_at FROM chats WHERE id='c1'", [], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert!(second >= first, "updated_at must not move backwards");
    }

    // ----- M5: optimistic send state, lookups, read state -----------------

    #[test]
    fn v1_database_upgrades_to_v2_defaulting_existing_rows_to_sent() {
        let path = temp_path("v1-to-v2");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            // Build a genuine v1 database: the historical DDL plus a row, with
            // the version marker set to 1 (no `send_state` column yet).
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute(
                "INSERT INTO messages
                    (id, chat_id, profile, sender_mri, sender_name, timestamp,
                     content, raw, reactions_json, reply_to, client_message_id,
                     edited, is_own, updated_at)
                 VALUES ('m1','c1','default','8:orgid:aaaa','Alice',
                     '2026-01-01T00:00:00Z','hi','<p>hi</p>','[]',NULL,NULL,0,0,1)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }

        let store = Store::open(&path).expect("open the v1 database");
        // Forward-only migration reached v2.
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        // The column now exists ...
        {
            let conn = store.lock();
            assert!(column_exists(&conn, "messages", "send_state").unwrap());
        }
        // ... the v2 table exists ...
        assert!(store.read_state("default", "c1").unwrap().is_none());
        // ... and the pre-existing row defaults to `sent`.
        let got = store.messages("c1", 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].send_state, SendState::Sent);
        assert_eq!(got[0].content, "hi");

        // Reopening again is a clean no-op.
        drop(store);
        let store = Store::open(&path).expect("reopen v2 database");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(store.messages("c1", 10).unwrap().len(), 1);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn message_and_client_id_lookups() {
        let store = Store::open_in_memory().unwrap();
        let mut a = message("a", "c1", "2026-01-01T00:00:00Z");
        a.client_message_id = Some("42".into());
        store
            .upsert_messages("default", &[a, message("b", "c1", "2026-01-02T00:00:00Z")])
            .unwrap();

        assert_eq!(
            store
                .message("c1", "a")
                .unwrap()
                .unwrap()
                .client_message_id
                .as_deref(),
            Some("42")
        );
        assert!(store.message("c1", "missing").unwrap().is_none());
        assert!(store.message("other", "a").unwrap().is_none());
        assert_eq!(
            store.message_by_client_id("c1", "42").unwrap().unwrap().id,
            "a"
        );
        assert!(store.message_by_client_id("c1", "nope").unwrap().is_none());
    }

    #[test]
    fn send_state_round_trips_and_updates_in_place() {
        let store = Store::open_in_memory().unwrap();
        let mut pending = message("p", "c1", "2026-01-01T00:00:00Z");
        pending.send_state = SendState::Pending;
        pending.is_own = true;
        pending.client_message_id = Some("p".into());
        store.upsert_messages("default", &[pending]).unwrap();

        assert_eq!(
            store.messages("c1", 10).unwrap()[0].send_state,
            SendState::Pending
        );
        assert!(
            store
                .update_message_send_state("default", "c1", "p", SendState::Failed)
                .unwrap()
        );
        let row = &store.messages("c1", 10).unwrap()[0];
        assert_eq!(row.send_state, SendState::Failed);
        assert_eq!(row.content, "body p", "only the state changed");

        // A miss is `false`, not an error; another profile must not match.
        assert!(
            !store
                .update_message_send_state("default", "c1", "missing", SendState::Sent)
                .unwrap()
        );
        assert!(
            !store
                .update_message_send_state("work", "c1", "p", SendState::Sent)
                .unwrap()
        );
    }

    #[test]
    fn read_state_round_trips_and_is_profile_scoped() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.read_state("default", "c1").unwrap(), None);
        store.set_read_state("default", "c1", "m5").unwrap();
        assert_eq!(
            store.read_state("default", "c1").unwrap().as_deref(),
            Some("m5")
        );
        // A newer horizon replaces the old one (one row per chat/profile).
        store.set_read_state("default", "c1", "m9").unwrap();
        assert_eq!(
            store.read_state("default", "c1").unwrap().as_deref(),
            Some("m9")
        );
        assert_eq!(store.read_state("work", "c1").unwrap(), None);
        assert_eq!(
            store.read_state("default", "c2").unwrap(),
            None,
            "other chats are unaffected"
        );
    }

    // ----- M6: teams, channels & members -----------------------------------

    fn team(id: &str, name: &str) -> Team {
        Team {
            id: id.into(),
            name: name.into(),
        }
    }

    fn channel(id: &str, team_id: &str, name: &str) -> Channel {
        Channel {
            id: id.into(),
            team_id: team_id.into(),
            name: name.into(),
            description: Some("about".into()),
            membership_type: Some("standard".into()),
            web_url: Some(format!("https://teams.microsoft.com/l/channel/{id}")),
            is_channel: crate::model::is_channel_conversation_id(id),
        }
    }

    fn member(id: &str, name: &str) -> TeamMember {
        TeamMember {
            id: id.into(),
            display_name: name.into(),
            email: Some(format!("{id}@example.com")),
            roles: vec!["owner".into()],
        }
    }

    #[test]
    fn v2_database_upgrades_to_v3_adding_team_tables() {
        let path = temp_path("v2-to-v3");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            // Build a genuine v2 database: the v1 DDL, the v2 `send_state`
            // column + `read_state` table, version marker 2.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute_batch(
                "ALTER TABLE messages ADD COLUMN send_state TEXT NOT NULL DEFAULT 'sent';",
            )
            .unwrap();
            conn.execute_batch(MIGRATION_V2_TABLES).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }

        let store = Store::open(&path).expect("open the v2 database");
        // Forward-only migration reached v3.
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        // The three v3 tables exist and are queryable (an empty result, not an
        // error, proves the DDL landed).
        assert!(store.list_teams("default").unwrap().is_empty());
        assert!(store.channels_for_team("team-1").unwrap().is_empty());
        assert!(store.team_members("team-1").unwrap().is_empty());
        // They are usable: write then read back.
        store.upsert_teams("default", &[team("t1", "One")]).unwrap();
        assert_eq!(store.list_teams("default").unwrap().len(), 1);

        // Reopening again is a clean no-op.
        drop(store);
        let store = Store::open(&path).expect("reopen v3 database");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(store.list_teams("default").unwrap().len(), 1);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn teams_and_channels_round_trip_and_are_profile_scoped() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.list_teams("default").unwrap().is_empty());

        store
            .upsert_teams("default", &[team("t2", "Zeta"), team("t1", "Alpha")])
            .unwrap();
        // Name order (case-insensitive), and idempotent re-upsert updates.
        let teams = store.list_teams("default").unwrap();
        assert_eq!(
            teams.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["t1", "t2"]
        );
        store
            .upsert_teams("default", &[team("t1", "Alpha renamed")])
            .unwrap();
        assert_eq!(store.list_teams("default").unwrap().len(), 2);
        assert_eq!(
            store.list_teams("default").unwrap()[0].name,
            "Alpha renamed"
        );
        // A different profile is isolated.
        assert!(store.list_teams("work").unwrap().is_empty());

        store
            .upsert_channels(
                "default",
                &[
                    channel("19:beta@thread.tacv2", "t1", "Beta"),
                    channel("19:alpha@thread.tacv2", "t1", "Alpha"),
                    channel("19:other@thread.tacv2", "t2", "Other"),
                ],
            )
            .unwrap();
        let t1 = store.channels_for_team("t1").unwrap();
        assert_eq!(
            t1.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["19:alpha@thread.tacv2", "19:beta@thread.tacv2"]
        );
        assert_eq!(t1[0].team_id, "t1");
        assert!(t1[0].is_channel);
        assert_eq!(store.channels_for_team("t2").unwrap().len(), 1);

        // Re-upsert one channel: no duplicate, fields updated.
        let mut renamed = channel("19:beta@thread.tacv2", "t1", "Beta renamed");
        renamed.description = None;
        store.upsert_channels("default", &[renamed]).unwrap();
        assert_eq!(store.channels_for_team("t1").unwrap().len(), 2);
        let beta = store
            .channel("t1", "19:beta@thread.tacv2")
            .unwrap()
            .unwrap();
        assert_eq!(beta.name, "Beta renamed");
        assert!(beta.description.is_none());

        // Delete drops only the named channel.
        assert!(
            store
                .delete_channel("default", "t1", "19:beta@thread.tacv2")
                .unwrap()
        );
        assert_eq!(store.channels_for_team("t1").unwrap().len(), 1);
        assert!(
            !store
                .delete_channel("default", "t1", "19:beta@thread.tacv2")
                .unwrap()
        );
        // A wrong-profile delete reports a miss.
        assert!(
            !store
                .delete_channel("work", "t1", "19:alpha@thread.tacv2")
                .unwrap()
        );
        assert_eq!(store.channels_for_team("t1").unwrap().len(), 1);
    }

    #[test]
    fn team_members_round_trip_roles_and_degrade_corrupt_json() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.team_members("t1").unwrap().is_empty());

        let mut bob = member("M2", "Bob");
        bob.roles = Vec::new();
        bob.email = None;
        store
            .upsert_team_members("default", "t1", &[member("M1", "Alice"), bob])
            .unwrap();
        let roster = store.team_members("t1").unwrap();
        assert_eq!(
            roster.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["M1", "M2"]
        );
        assert!(roster[0].is_owner());
        assert_eq!(roster[0].roles, vec!["owner".to_string()]);
        assert!(!roster[1].is_owner());
        assert!(roster[1].email.is_none());

        // Idempotent re-upsert: same two rows, updated display name.
        store
            .upsert_team_members("default", "t1", &[member("M1", "Alice Smith")])
            .unwrap();
        assert_eq!(store.team_members("t1").unwrap().len(), 2);
        assert_eq!(
            store.team_members("t1").unwrap()[0].display_name,
            "Alice Smith"
        );

        // A corrupt roles blob degrades to "no roles" rather than failing.
        {
            let conn = store.lock();
            conn.execute(
                "UPDATE team_members SET roles_json = 'not json' WHERE id = 'M1'",
                [],
            )
            .unwrap();
        }
        let roster = store.team_members("t1").unwrap();
        assert_eq!(roster.len(), 2);
        assert!(roster[0].roles.is_empty());
    }

    // ----- M7: files --------------------------------------------------------

    fn file(id: &str, drive: &str, name: &str) -> FileEntry {
        FileEntry {
            id: id.into(),
            drive_id: Some(drive.into()),
            name: name.into(),
            size: 42,
            mime: Some("text/plain".into()),
            web_url: Some(format!("https://sp/{id}")),
            download_url: Some(format!("https://dl/{id}")),
            is_folder: false,
            created: Some("2026-01-01T00:00:00Z".into()),
            modified: Some("2026-01-02T00:00:00Z".into()),
            sender: Some("Alice".into()),
            share_url: None,
            attachment_id: Some("att-1".into()),
        }
    }

    #[test]
    fn v3_database_upgrades_to_v4_adding_the_files_table() {
        let path = temp_path("v3-to-v4");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            // Build a genuine v3 database: v1 DDL, the v2 `send_state` column +
            // `read_state`, the v3 tables, version marker 3.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute_batch(
                "ALTER TABLE messages ADD COLUMN send_state TEXT NOT NULL DEFAULT 'sent';",
            )
            .unwrap();
            conn.execute_batch(MIGRATION_V2_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V3_TABLES).unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
        }

        let store = Store::open(&path).expect("open the v3 database");
        // Forward-only migration reached v4.
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        // The files table exists and is queryable (empty, not an error) ...
        assert!(store.files_for_chat("default", "c1").unwrap().is_empty());
        // ... and writable.
        store
            .upsert_files("default", Some("c1"), &[file("i1", "D1", "a.pdf")])
            .unwrap();
        assert_eq!(store.files_for_chat("default", "c1").unwrap().len(), 1);

        // Reopening again is a clean no-op.
        drop(store);
        let store = Store::open(&path).expect("reopen v4 database");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(store.files_for_chat("default", "c1").unwrap().len(), 1);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn files_round_trip_and_are_profile_scoped() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.files_for_chat("default", "c1").unwrap().is_empty());

        store
            .upsert_files("default", Some("c1"), &[file("i1", "D1", "B.txt")])
            .unwrap();
        let got = store.files_for_chat("default", "c1").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "i1");
        assert_eq!(got[0].drive_id.as_deref(), Some("D1"));
        assert_eq!(got[0].attachment_id.as_deref(), Some("att-1"));

        // drive+item lookup, and the drive is part of the filter.
        assert_eq!(
            store.file("default", "D1", "i1").unwrap().unwrap().name,
            "B.txt"
        );
        assert!(store.file("default", "D2", "i1").unwrap().is_none());
        assert!(store.file("default", "D1", "missing").unwrap().is_none());

        // Profile isolation.
        assert!(store.files_for_chat("work", "c1").unwrap().is_empty());
        assert!(store.file("work", "D1", "i1").unwrap().is_none());

        // Idempotent re-upsert: same row, updated name.
        store
            .upsert_files("default", Some("c1"), &[file("i1", "D1", "B renamed.txt")])
            .unwrap();
        let got = store.files_for_chat("default", "c1").unwrap();
        assert_eq!(got.len(), 1, "no duplicate row");
        assert_eq!(got[0].name, "B renamed.txt");

        // Delete drops only the named row.
        assert!(store.delete_file("default", "D1", "i1").unwrap());
        assert!(store.file("default", "D1", "i1").unwrap().is_none());
        assert!(!store.delete_file("default", "D1", "i1").unwrap());
        // A wrong-profile delete reports a miss.
        store
            .upsert_files("default", Some("c1"), &[file("i2", "D1", "keep.txt")])
            .unwrap();
        assert!(!store.delete_file("work", "D1", "i2").unwrap());
        assert_eq!(store.files_for_chat("default", "c1").unwrap().len(), 1);
    }

    #[test]
    fn file_upsert_is_additive_and_preserves_chat_and_enrichment() {
        let store = Store::open_in_memory().unwrap();
        // Listed from a chat: chat association + attachment id + sender.
        store
            .upsert_files("default", Some("c1"), &[file("i1", "D1", "Plan.docx")])
            .unwrap();

        // A later recents/folder listing (no chat, no attachment id, no sender)
        // must not wipe the chat association or the enrichment.
        let mut recent = file("i1", "D1", "Plan.docx");
        recent.attachment_id = None;
        recent.sender = None;
        recent.share_url = None;
        store.upsert_files("default", None, &[recent]).unwrap();
        assert_eq!(
            store.files_for_chat("default", "c1").unwrap().len(),
            1,
            "the chat association survives a chatless listing"
        );
        let row = store.file("default", "D1", "i1").unwrap().unwrap();
        assert_eq!(row.attachment_id.as_deref(), Some("att-1"));
        assert_eq!(row.sender.as_deref(), Some("Alice"));

        // create_link caches the share url (chat_id None) without losing the
        // chat association either.
        let mut linked = file("i1", "D1", "Plan.docx");
        linked.share_url = Some("https://sp/:w:/x".into());
        linked.attachment_id = None;
        store.upsert_files("default", None, &[linked]).unwrap();
        let row = store.file("default", "D1", "i1").unwrap().unwrap();
        assert_eq!(row.share_url.as_deref(), Some("https://sp/:w:/x"));
        assert_eq!(store.files_for_chat("default", "c1").unwrap().len(), 1);
        assert_eq!(
            row.attachment_id.as_deref(),
            Some("att-1"),
            "attachment still kept"
        );
    }

    // ----- M8: mute + unread ------------------------------------------------

    #[test]
    fn v4_database_upgrades_to_v5_adding_the_mute_column() {
        let path = temp_path("v4-to-v5");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            // Build a genuine v4 database: v1 DDL, v2 column + read_state, v3
            // tables, v4 files table, version marker 4.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute_batch(
                "ALTER TABLE messages ADD COLUMN send_state TEXT NOT NULL DEFAULT 'sent';",
            )
            .unwrap();
            conn.execute_batch(MIGRATION_V2_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V3_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V4_TABLES).unwrap();
            conn.execute(
                "INSERT INTO chats (id, profile, name, is_group, updated_at)
                 VALUES ('c1','default','One',0,1)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 4).unwrap();
        }

        let store = Store::open(&path).expect("open the v4 database");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        // The column exists and the pre-existing chat defaults to not-muted.
        {
            let conn = store.lock();
            assert!(column_exists(&conn, "chats", "muted").unwrap());
        }
        assert!(!store.is_chat_muted("default", "c1").unwrap());
        assert!(store.set_chat_muted("default", "c1", true).unwrap());
        assert!(store.is_chat_muted("default", "c1").unwrap());

        // Reopening again is a clean no-op and the flag survives.
        drop(store);
        let store = Store::open(&path).expect("reopen v5 database");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert!(store.is_chat_muted("default", "c1").unwrap());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn chat_mute_round_trips_and_is_profile_scoped() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_chats(
                "default",
                &[chat("c1", "One", None), chat("c2", "Two", None)],
            )
            .unwrap();

        assert!(!store.is_chat_muted("default", "c1").unwrap());
        assert!(store.set_chat_muted("default", "c1", true).unwrap());
        assert!(store.is_chat_muted("default", "c1").unwrap());
        assert_eq!(
            store.muted_chats("default").unwrap(),
            vec!["c1".to_string()]
        );

        // Unmuting removes it from the list.
        assert!(store.set_chat_muted("default", "c1", false).unwrap());
        assert!(store.muted_chats("default").unwrap().is_empty());

        // A chat that is not cached cannot be muted (the row does not exist);
        // another profile is isolated.
        assert!(!store.set_chat_muted("default", "missing", true).unwrap());
        assert!(!store.is_chat_muted("default", "missing").unwrap());
        assert!(!store.set_chat_muted("work", "c1", true).unwrap());
        assert!(!store.is_chat_muted("work", "c1").unwrap());
    }

    // ----- B2a: server mute / hide ------------------------------------------

    #[test]
    fn chat_hidden_round_trips_and_is_profile_scoped() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_chats(
                "default",
                &[chat("c1", "One", None), chat("c2", "Two", None)],
            )
            .unwrap();

        assert!(!store.is_chat_hidden("default", "c1").unwrap());
        assert!(store.set_chat_hidden("default", "c1", true).unwrap());
        assert!(store.is_chat_hidden("default", "c1").unwrap());
        assert!(!store.is_chat_hidden("default", "c2").unwrap());
        assert_eq!(
            store.hidden_chats("default").unwrap(),
            vec!["c1".to_string()]
        );

        // Unhiding removes it from the collapsed set.
        assert!(store.set_chat_hidden("default", "c1", false).unwrap());
        assert!(store.hidden_chats("default").unwrap().is_empty());

        // An uncached chat cannot be hidden; another profile is isolated.
        assert!(!store.set_chat_hidden("default", "missing", true).unwrap());
        assert!(!store.is_chat_hidden("default", "missing").unwrap());
        assert!(!store.set_chat_hidden("work", "c1", true).unwrap());
    }

    #[test]
    fn chat_server_mute_records_unknown_versus_confirmed() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_chats("default", &[chat("c1", "One", None)])
            .unwrap();

        // Never written → unknown (`None`), distinct from a confirmed unmute.
        assert_eq!(store.server_muted("default", "c1").unwrap(), None);
        assert!(
            store
                .set_chat_server_muted("default", "c1", Some(true))
                .unwrap()
        );
        assert_eq!(store.server_muted("default", "c1").unwrap(), Some(true));
        assert!(
            store
                .set_chat_server_muted("default", "c1", Some(false))
                .unwrap()
        );
        assert_eq!(store.server_muted("default", "c1").unwrap(), Some(false));
        // Clearing back to unknown is a real state, not a default.
        assert!(store.set_chat_server_muted("default", "c1", None).unwrap());
        assert_eq!(store.server_muted("default", "c1").unwrap(), None);

        // The server flag is independent of the local routing flag.
        assert!(store.set_chat_muted("default", "c1", true).unwrap());
        assert!(store.is_chat_muted("default", "c1").unwrap());
        assert_eq!(store.server_muted("default", "c1").unwrap(), None);
        assert!(
            !store
                .set_chat_server_muted("default", "missing", Some(true))
                .unwrap()
        );
    }

    #[test]
    fn v9_database_upgrades_to_v10_adding_hide_and_server_mute_columns() {
        let path = temp_path("v9-to-v10");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            // Build a genuine v9 database: v1 DDL, the v2 column + read_state,
            // the v3/v4/v6/v7 tables, the v5 mute column, the v8 identity
            // columns, the v9 search index, version marker 9, and one chat.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute_batch(
                "ALTER TABLE messages ADD COLUMN send_state TEXT NOT NULL DEFAULT 'sent';",
            )
            .unwrap();
            conn.execute_batch(MIGRATION_V2_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V3_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V4_TABLES).unwrap();
            conn.execute_batch("ALTER TABLE chats ADD COLUMN muted INTEGER NOT NULL DEFAULT 0;")
                .unwrap();
            conn.execute_batch(MIGRATION_V6_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V7_TABLES).unwrap();
            conn.execute_batch("ALTER TABLE accounts ADD COLUMN self_mri TEXT;")
                .unwrap();
            conn.execute_batch("ALTER TABLE accounts ADD COLUMN oid TEXT;")
                .unwrap();
            conn.execute_batch(MIGRATION_V9_INDEX).unwrap();
            conn.execute(
                "INSERT INTO chats (id, profile, name, is_group, updated_at)
                 VALUES ('c1','default','One',0,1)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 9).unwrap();
        }

        let store = Store::open(&path).expect("open the v9 database");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        {
            let conn = store.lock();
            assert!(column_exists(&conn, "chats", "hidden").unwrap());
            assert!(column_exists(&conn, "chats", "server_muted").unwrap());
        }
        // The pre-existing chat defaults to visible and an unknown server mute.
        assert!(!store.is_chat_hidden("default", "c1").unwrap());
        assert_eq!(store.server_muted("default", "c1").unwrap(), None);
        assert!(store.set_chat_hidden("default", "c1", true).unwrap());
        assert!(
            store
                .set_chat_server_muted("default", "c1", Some(true))
                .unwrap()
        );

        // Reopening again is a clean no-op and the state survives.
        drop(store);
        let store = Store::open(&path).expect("reopen v10 database");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert!(store.is_chat_hidden("default", "c1").unwrap());
        assert_eq!(store.server_muted("default", "c1").unwrap(), Some(true));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn unread_counts_follow_the_read_horizon() {
        let store = Store::open_in_memory().unwrap();
        let msgs = vec![
            message("m1", "c1", "2026-01-01T00:00:01Z"),
            message("m2", "c1", "2026-01-01T00:00:02Z"),
            message("m3", "c1", "2026-01-01T00:00:03Z"),
        ];
        store.upsert_messages("default", &msgs).unwrap();

        // No horizon: everything is unread.
        assert_eq!(store.unread_count("default", "c1").unwrap(), 3);
        // Newest-first.
        let unread = store.unread_messages("default", "c1", 10).unwrap();
        assert_eq!(
            unread.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["m3", "m2", "m1"]
        );

        store.set_read_state("default", "c1", "m1").unwrap();
        assert_eq!(store.unread_count("default", "c1").unwrap(), 2);
        assert_eq!(store.unread_messages("default", "c1", 10).unwrap().len(), 2);

        // A different chat is unaffected; another profile sees no horizon.
        assert_eq!(store.unread_count("default", "c2").unwrap(), 0);
        assert_eq!(store.unread_count("work", "c1").unwrap(), 3);
    }

    // ----- M9: call log -----------------------------------------------------

    fn call_entry(id: &str, thread: &str, started: i64) -> CallLogEntry {
        CallLogEntry::started(id, thread, CallDirection::Incoming, started)
            .with_participants(vec!["Alice".into()])
    }

    #[test]
    fn v5_database_upgrades_to_v6_adding_the_call_log_table() {
        let path = temp_path("v5-to-v6");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            // Build a genuine v5 database: v1 DDL, v2 column + read_state, v3
            // tables, v4 files table, v5 mute column, version marker 5.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute_batch(
                "ALTER TABLE messages ADD COLUMN send_state TEXT NOT NULL DEFAULT 'sent';",
            )
            .unwrap();
            conn.execute_batch(MIGRATION_V2_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V3_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V4_TABLES).unwrap();
            conn.execute_batch("ALTER TABLE chats ADD COLUMN muted INTEGER NOT NULL DEFAULT 0;")
                .unwrap();
            conn.pragma_update(None, "user_version", 5).unwrap();
        }

        let store = Store::open(&path).expect("open the v5 database");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        // The table exists and round-trips a row written after the upgrade.
        assert!(store.call_log("default", 10).unwrap().is_empty());
        store
            .upsert_call_log("default", &[call_entry("call-1", "t1", 100)])
            .unwrap();
        assert_eq!(store.call_log("default", 10).unwrap().len(), 1);

        // Reopening again is a clean no-op and the row survives.
        drop(store);
        let store = Store::open(&path).expect("reopen v6 database");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(store.call_log("default", 10).unwrap().len(), 1);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn call_log_round_trips_and_upsert_updates_in_place() {
        let store = Store::open_in_memory().unwrap();
        let mut entry = call_entry("call-1", "t1", 1_000);
        store.upsert_call_log("default", &[entry.clone()]).unwrap();

        // In progress: no end, outcome unknown.
        let got = store.call_log_entry("default", "call-1").unwrap().unwrap();
        assert_eq!(got, entry);
        assert_eq!(got.direction, CallDirection::Incoming);
        assert!(got.ended_at.is_none());
        assert!(got.duration_secs.is_none());
        assert_eq!(got.outcome, CallOutcome::Unknown);
        assert_eq!(got.participants, vec!["Alice".to_string()]);

        // Finishing the same call updates the row, never adds one.
        entry = entry.finished(1_042, CallOutcome::Completed);
        store.upsert_call_log("default", &[entry.clone()]).unwrap();
        assert_eq!(store.call_log("default", 10).unwrap().len(), 1);
        assert_eq!(
            store.call_log_entry("default", "call-1").unwrap().unwrap(),
            entry
        );
        assert_eq!(
            store
                .call_log_entry("default", "call-1")
                .unwrap()
                .unwrap()
                .duration_secs,
            Some(42)
        );

        // A corrupt participants blob degrades to empty, never an error.
        {
            let conn = store.lock();
            conn.execute(
                "UPDATE call_log SET participants_json = 'not json' WHERE id = 'call-1'",
                [],
            )
            .unwrap();
        }
        assert!(
            store
                .call_log_entry("default", "call-1")
                .unwrap()
                .unwrap()
                .participants
                .is_empty()
        );
    }

    #[test]
    fn call_log_is_ordered_newest_first_and_profile_scoped() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_call_log(
                "default",
                &[
                    call_entry("old", "t1", 100),
                    call_entry("new", "t2", 300),
                    call_entry("mid", "t1", 200),
                ],
            )
            .unwrap();
        store
            .upsert_call_log("work", &[call_entry("other", "t9", 400)])
            .unwrap();

        let all = store.call_log("default", 10).unwrap();
        assert_eq!(
            all.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["new", "mid", "old"],
            "newest first"
        );
        assert_eq!(store.call_log("default", 2).unwrap().len(), 2);

        // Per-thread filter, profile isolated.
        let thread = store.call_log_for_thread("default", "t1", 10).unwrap();
        assert_eq!(
            thread.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["mid", "old"]
        );
        assert!(
            store
                .call_log_for_thread("work", "t1", 10)
                .unwrap()
                .is_empty()
        );
        assert!(store.call_log_entry("work", "old").unwrap().is_none());
        assert!(store.call_log_entry("work", "other").unwrap().is_some());
    }

    // ----- M12: calendar meetings -------------------------------------------

    fn meeting(id: &str, subject: &str, start: Option<&str>) -> Meeting {
        Meeting {
            id: id.into(),
            subject: subject.into(),
            start: start.map(str::to_string),
            end: None,
            all_day: false,
            organizer: Some("Doe, Jane".into()),
            organizer_email: Some("jane@contoso.com".into()),
            is_organizer: false,
            join_url: Some(format!(
                "https://teams.microsoft.com/l/meetup-join/19%3ameeting_{id}%40thread.v2/0"
            )),
            join_meeting_id: None,
            passcode: None,
            web_link: None,
            location: None,
            is_online: true,
        }
    }

    #[test]
    fn v6_database_upgrades_to_v7_adding_the_meetings_table() {
        let path = temp_path("v6-to-v7");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            // Build a genuine v6 database: v1 DDL, v2 column + read_state, v3
            // tables, v4 files, v5 mute column, v6 call_log, version marker 6.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute_batch(
                "ALTER TABLE messages ADD COLUMN send_state TEXT NOT NULL DEFAULT 'sent';",
            )
            .unwrap();
            conn.execute_batch(MIGRATION_V2_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V3_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V4_TABLES).unwrap();
            conn.execute_batch("ALTER TABLE chats ADD COLUMN muted INTEGER NOT NULL DEFAULT 0;")
                .unwrap();
            conn.execute_batch(MIGRATION_V6_TABLES).unwrap();
            conn.pragma_update(None, "user_version", 6).unwrap();
        }

        let store = Store::open(&path).expect("open the v6 database");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        // The table exists and round-trips a row written after the upgrade.
        assert!(
            store
                .upcoming_meetings("default", 0, 10)
                .unwrap()
                .is_empty()
        );
        store
            .upsert_meetings(
                "default",
                &[meeting(
                    "E1",
                    "Standup",
                    Some("2026-09-24T09:00:00.0000000"),
                )],
            )
            .unwrap();
        assert_eq!(store.upcoming_meetings("default", 0, 10).unwrap().len(), 1);

        // Reopening again is a clean no-op and the row survives.
        drop(store);
        let store = Store::open(&path).expect("reopen v7 database");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(store.upcoming_meetings("default", 0, 10).unwrap().len(), 1);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn v7_database_upgrades_to_v8_adding_identity_columns() {
        let path = temp_path("v7-to-v8");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            // Build a genuine v7 database: the v1 accounts DDL (no identity
            // columns), every later table/column, version marker 7, and an
            // account row.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute_batch(
                "ALTER TABLE messages ADD COLUMN send_state TEXT NOT NULL DEFAULT 'sent';",
            )
            .unwrap();
            conn.execute_batch(MIGRATION_V2_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V3_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V4_TABLES).unwrap();
            conn.execute_batch("ALTER TABLE chats ADD COLUMN muted INTEGER NOT NULL DEFAULT 0;")
                .unwrap();
            conn.execute_batch(MIGRATION_V6_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V7_TABLES).unwrap();
            conn.execute(
                "INSERT INTO accounts (profile, tenant, personal, display_name, created_at)
                 VALUES ('default','contoso.onmicrosoft.com',0,'Alice',1700000000)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 7).unwrap();
        }

        let store = Store::open(&path).expect("open the v7 database");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        // The v8 columns exist; the pre-existing account reads with no identity.
        {
            let conn = store.lock();
            assert!(column_exists(&conn, "accounts", "self_mri").unwrap());
            assert!(column_exists(&conn, "accounts", "oid").unwrap());
        }
        let account = store.get_account("default").unwrap().unwrap();
        assert_eq!(account.display_name.as_deref(), Some("Alice"));
        assert!(account.self_mri.is_none());
        assert!(account.oid.is_none());

        // The identity round-trips through an update.
        let mut with_identity = account.clone();
        with_identity.self_mri = Some("8:orgid:aaaa".into());
        with_identity.oid = Some("aaaa".into());
        store.put_account(&with_identity).unwrap();
        let back = store.get_account("default").unwrap().unwrap();
        assert_eq!(back.self_mri.as_deref(), Some("8:orgid:aaaa"));
        assert_eq!(back.oid.as_deref(), Some("aaaa"));
        assert_eq!(back.created_at, 1_700_000_000, "created_at is preserved");

        // Reopening again is a clean no-op.
        drop(store);
        let store = Store::open(&path).expect("reopen v8 database");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(
            store
                .get_account("default")
                .unwrap()
                .unwrap()
                .self_mri
                .as_deref(),
            Some("8:orgid:aaaa")
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn meetings_round_trip_order_by_start_and_are_profile_scoped() {
        let store = Store::open_in_memory().unwrap();
        assert!(
            store
                .upcoming_meetings("default", 0, 10)
                .unwrap()
                .is_empty()
        );

        store
            .upsert_meetings(
                "default",
                &[
                    meeting("late", "Demo", Some("2026-10-02T14:00:00.0000000")),
                    meeting("early", "Standup", Some("2026-09-24T09:00:00.0000000")),
                    meeting("mid", "Sync", Some("2026-09-29T10:00:00.0000000")),
                    meeting("none", "No date", None),
                ],
            )
            .unwrap();

        // Soonest first; a meeting with no start is not "upcoming".
        let all = store.upcoming_meetings("default", 0, 10).unwrap();
        assert_eq!(
            all.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["early", "mid", "late"]
        );
        assert_eq!(all[0].subject, "Standup");
        assert!(all[0].is_online);
        assert_eq!(all[0].organizer.as_deref(), Some("Doe, Jane"));
        assert!(all[0].join_url.is_some());

        // The `from_secs` filter (2026-09-29T00:00:00Z = 1790553600).
        let from = store
            .upcoming_meetings("default", 1_790_553_600, 10)
            .unwrap();
        assert_eq!(
            from.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["mid", "late"]
        );
        // `limit` is honoured.
        assert_eq!(store.upcoming_meetings("default", 0, 1).unwrap().len(), 1);

        // `meeting()` fetches one; profile isolation.
        assert_eq!(
            store.meeting("mid", "default").unwrap().unwrap().subject,
            "Sync"
        );
        assert!(store.meeting("mid", "work").unwrap().is_none());
        assert!(store.upcoming_meetings("work", 0, 10).unwrap().is_empty());

        // Idempotent re-upsert: same row, updated subject.
        store
            .upsert_meetings(
                "default",
                &[meeting(
                    "mid",
                    "Sync renamed",
                    Some("2026-09-29T10:00:00.0000000"),
                )],
            )
            .unwrap();
        assert_eq!(store.upcoming_meetings("default", 0, 10).unwrap().len(), 3);
        assert_eq!(
            store.meeting("mid", "default").unwrap().unwrap().subject,
            "Sync renamed"
        );

        // Delete drops only the named row.
        assert!(store.delete_meeting("default", "mid").unwrap());
        assert!(store.meeting("mid", "default").unwrap().is_none());
        assert!(!store.delete_meeting("default", "mid").unwrap());
        // A wrong-profile delete reports a miss.
        assert!(!store.delete_meeting("work", "early").unwrap());
        assert_eq!(store.upcoming_meetings("default", 0, 10).unwrap().len(), 2);
    }

    #[test]
    fn iso8601_from_unix_matches_the_wire_ordering_space() {
        assert_eq!(iso8601_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_from_unix(1_790_553_600), "2026-09-28T00:00:00Z");
        // The Graph `start` text (no timezone, fractional seconds) still sorts
        // after an earlier UTC boundary.
        assert!("2026-09-28T09:00:00.0000000" >= iso8601_from_unix(1_790_553_600).as_str());
    }

    // ----- B1: message search ----------------------------------------------

    fn search_msg(chat_id: &str, id: &str, content: &str, ts: &str) -> Message {
        let mut m = message(id, chat_id, ts);
        m.content = content.into();
        m.raw = format!("<p>{content}</p>");
        m
    }

    fn index_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
            params![name],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    }

    #[test]
    fn v8_database_upgrades_to_v9_adding_the_search_index() {
        let path = temp_path("v8-to-v9");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            // Build a genuine v8 database: v1 DDL, the v2 `send_state` column +
            // `read_state`, v3/v4/v6/v7 tables, the v5 mute column, the v8
            // identity columns, version marker 8, and a searchable message.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute_batch(
                "ALTER TABLE messages ADD COLUMN send_state TEXT NOT NULL DEFAULT 'sent';",
            )
            .unwrap();
            conn.execute_batch(MIGRATION_V2_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V3_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V4_TABLES).unwrap();
            conn.execute_batch("ALTER TABLE chats ADD COLUMN muted INTEGER NOT NULL DEFAULT 0;")
                .unwrap();
            conn.execute_batch(MIGRATION_V6_TABLES).unwrap();
            conn.execute_batch(MIGRATION_V7_TABLES).unwrap();
            conn.execute_batch("ALTER TABLE accounts ADD COLUMN self_mri TEXT;")
                .unwrap();
            conn.execute_batch("ALTER TABLE accounts ADD COLUMN oid TEXT;")
                .unwrap();
            conn.execute(
                "INSERT INTO messages
                    (id, chat_id, profile, sender_mri, sender_name, timestamp, content,
                     raw, reactions_json, edited, is_own, send_state, updated_at)
                 VALUES ('m1','c1','default','8:orgid:aaaa','Alice',
                     '2026-01-01T00:00:00Z','a needle here','<p>x</p>','[]',0,0,'sent',1)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 8).unwrap();
        }

        let store = Store::open(&path).expect("open the v8 database");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        // The v9 index exists ...
        {
            let conn = store.lock();
            assert!(index_exists(&conn, "idx_messages_profile_time"));
        }
        // ... and the pre-existing row is searchable.
        let hits = store
            .search_messages("default", "needle", None, 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id, "m1");
        assert_eq!(hits[0].sender, "Alice");

        // Reopening again is a clean no-op.
        drop(store);
        let store = Store::open(&path).expect("reopen v9 database");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(
            store
                .search_messages("default", "needle", None, 10)
                .unwrap()
                .len(),
            1
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn search_messages_matches_case_insensitively_newest_first() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_messages(
                "default",
                &[
                    search_msg("c1", "old", "Ship it", "2026-01-01T00:00:00Z"),
                    search_msg("c2", "new", "we shipped", "2026-01-03T00:00:00Z"),
                    search_msg("c1", "mid", "SHIPPING", "2026-01-02T00:00:00Z"),
                    search_msg("c1", "no", "unrelated", "2026-01-04T00:00:00Z"),
                ],
            )
            .unwrap();

        let hits = store.search_messages("default", "ship", None, 10).unwrap();
        assert_eq!(
            hits.iter()
                .map(|h| h.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["new", "mid", "old"],
            "case-insensitive, newest first"
        );

        // `limit` and the chat filter are honoured.
        assert_eq!(
            store
                .search_messages("default", "ship", None, 2)
                .unwrap()
                .len(),
            2
        );
        let c1 = store
            .search_messages("default", "ship", Some("c1"), 10)
            .unwrap();
        assert_eq!(
            c1.iter().map(|h| h.message_id.as_str()).collect::<Vec<_>>(),
            vec!["mid", "old"]
        );

        // Profile isolation.
        assert!(
            store
                .search_messages("work", "ship", None, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn search_messages_orders_ties_by_rowid_and_escapes_wildcards() {
        let store = Store::open_in_memory().unwrap();
        // Identical timestamps: rowid DESC (later insert first) keeps it stable.
        store
            .upsert_messages(
                "default",
                &[
                    search_msg("c1", "first", "tie", "2026-01-01T00:00:00Z"),
                    search_msg("c1", "second", "tie", "2026-01-01T00:00:00Z"),
                ],
            )
            .unwrap();
        let hits = store.search_messages("default", "tie", None, 10).unwrap();
        assert_eq!(
            hits.iter()
                .map(|h| h.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "first"]
        );

        // `%` is a literal, not "match everything".
        store
            .upsert_messages(
                "default",
                &[
                    search_msg("c2", "pct", "100% sure", "2026-02-01T00:00:00Z"),
                    search_msg("c2", "plain", "no percent", "2026-02-02T00:00:00Z"),
                ],
            )
            .unwrap();
        let pct = store.search_messages("default", "%", None, 10).unwrap();
        assert_eq!(
            pct.iter()
                .map(|h| h.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["pct"]
        );

        // `_` matches only a literal underscore, not any single character.
        store
            .upsert_messages(
                "default",
                &[
                    search_msg("c3", "under", "a_b", "2026-03-01T00:00:00Z"),
                    search_msg("c3", "axb", "axb", "2026-03-02T00:00:00Z"),
                ],
            )
            .unwrap();
        let under = store.search_messages("default", "a_b", None, 10).unwrap();
        assert_eq!(
            under
                .iter()
                .map(|h| h.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["under"]
        );

        // A backslash is literal too (and cannot swallow a wildcard's escape).
        store
            .upsert_messages(
                "default",
                &[search_msg(
                    "c4",
                    "slash",
                    r"path\to",
                    "2026-04-01T00:00:00Z",
                )],
            )
            .unwrap();
        let slash = store
            .search_messages("default", r"path\to", None, 10)
            .unwrap();
        assert_eq!(slash.len(), 1);
        assert_eq!(slash[0].message_id, "slash");
    }

    #[test]
    fn search_messages_blank_query_is_empty_and_unicode_never_panics() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_messages(
                "default",
                &[search_msg(
                    "c1",
                    "u",
                    "café ☕ 日本語 😀",
                    "2026-01-01T00:00:00Z",
                )],
            )
            .unwrap();

        // Empty/whitespace queries never mean "everything"; limit 0 is empty.
        assert!(
            store
                .search_messages("default", "", None, 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .search_messages("default", "   ", None, 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .search_messages("default", "x", None, 0)
                .unwrap()
                .is_empty()
        );

        // A unicode substring matches exactly.
        let hits = store
            .search_messages("default", "日本語", None, 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "café ☕ 日本語 😀");
        assert!(!hits[0].is_own);

        // Odd queries never panic.
        for query in ["%", "_", "\\", "😀", "café", "%%", "__", "\\\\", "a%b_c"] {
            let _ = store.search_messages("default", query, None, 10);
        }
    }
}
