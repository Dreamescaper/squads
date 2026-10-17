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
//! The schema version lives in `PRAGMA user_version` and starts at **1**.
//! Migrations are forward-only and re-entrant: every statement is
//! `IF NOT EXISTS`, so re-running the current version is a no-op. A database
//! whose `user_version` is **newer** than this build understands is rejected
//! with [`CoreError::StoreSchemaVersion`] rather than silently mangled.
//!
//! ## Who writes what
//!
//! M3 provides only the store API. **Realtime write-through lands in M4** and
//! optimistic sends in M5; both will call the same idempotent
//! [`Store::upsert_messages`] / [`Store::upsert_chats`] (keyed by server id) and
//! read history through [`Store::messages`] / [`Store::messages_before`].
//! Nothing in this module talks to the network.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::auth::tokens::now_unix;
use crate::error::{CoreError, Result};
use crate::model::{Chat, Message, Reaction};

/// Newest schema version this build knows how to create/migrate to.
pub const SCHEMA_VERSION: i32 = 1;

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

/// One local account record.
///
/// `profile` is the account key the rest of the core uses (see
/// [`DEFAULT_PROFILE`](crate::auth::store::DEFAULT_PROFILE)); `tenant` and
/// `display_name` are optional because a personal account may know neither at
/// first write.
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
    /// Unix seconds when the row was first written.
    pub created_at: i64,
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
            "INSERT INTO accounts (profile, tenant, personal, display_name, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(profile) DO UPDATE SET
                tenant = excluded.tenant,
                personal = excluded.personal,
                display_name = excluded.display_name",
            params![
                account.profile,
                account.tenant,
                account.personal as i64,
                account.display_name,
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
                "SELECT profile, tenant, personal, display_name, created_at
                 FROM accounts WHERE profile = ?1",
                params![profile],
                |row| {
                    Ok(Account {
                        profile: row.get(0)?,
                        tenant: row.get(1)?,
                        personal: row.get::<_, i64>(2)? != 0,
                        display_name: row.get(3)?,
                        created_at: row.get(4)?,
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
                     edited, is_own, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
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
                    raw, reactions_json, reply_to, client_message_id, edited, is_own
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
                    raw, reactions_json, reply_to, client_message_id, edited, is_own
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
    // Record the version *after* the DDL inside the same transaction, so a
    // crash can never leave a version marker ahead of the schema.
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
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
    })
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
}
