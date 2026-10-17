# Progress

Live status of the roadmap in [`PLAN.md`](PLAN.md). Updated as work lands.

Legend: ✅ done · 🟡 in progress · ⬜ not started · ⛔ blocked (see [BLOCKERS.md](BLOCKERS.md))

| # | Milestone | Status | Notes |
|---|---|---|---|
| M0 | Workspace & scaffolding | ✅ | Workspace + 3 crates; vendored protocol core; docs. |
| M1 | Authentication | ✅ | Device-code flow, five-token model, 0600 file store, testable `AuthTransport`. |
| M2 | Protocol clients & session core | ✅ | `ApiClient` over the vendored `TeamsClient`; `tokens_to_config`; lazy `Session::api()`; token refresh; offline `list_chats`/`read_messages` tests. |
| M3 | Local persistence & offline cache | ✅ | UI-free `model`; bundled-SQLite `Store` (`user_version` migrations, idempotent upserts, per-chat paging); JSON `Settings` with atomic saves; `Session` owns the store. |
| M4 | Realtime (Trouter) | ⬜ | |
| M5 | Messaging | ⬜ | |
| M6 | Teams & channels | ⬜ | |
| M7 | Files | ⬜ | |
| M8 | Presence & notifications | ⬜ | |
| M9 | Audio/video calls | ⬜ | |
| M10 | egui UI | ⬜ | |
| M11 | Polish & packaging | ⬜ | |

---

## M0 — Workspace & scaffolding

**Goal.** Turn the single-crate egui hello-world into a workspace with a vendored protocol
core, a UI-free core crate, and the egui app; document the architecture.

**Landed.**

- Root `Cargo.toml` converted to a workspace (`crates/squads-protocol`, `crates/squads-core`,
  `crates/squads-app`), with shared dependency versions and the egui-oriented
  dev/release profiles.
- Vendored `ost` (from the better-teams fork of `eisbaw/ost`) as `crates/squads-protocol`
  (package `squads-protocol`, library name `ost`), keeping its CLI as `squads-cli` for
  headless protocol testing. `LICENSE` preserved; attribution added in `NOTICE`.
- `crates/squads-core`: `AppDirs` (platform paths), `CoreError`/`Result`, and a
  `Session`/`SessionState` skeleton, with unit tests.
- `crates/squads-app`: the `squads` egui binary now depends on `squads-core` and renders
  the resolved session state, proving the workspace wiring.
- Docs: `PLAN.md`, `ARCHITECTURE.md`, `DECISIONS.md`, `PROGRESS.md`, `BLOCKERS.md`,
  `reference/` (live e2e probes), root `README.md`, `NOTICE`.

**Decisions.** ADR-001 (reuse `ost`), ADR-002 (workspace), ADR-006 (egui over GPUI).

**Blockers.** B-001 (headless: GUI cannot run), B-002 (no live tenant).

---

## M1 — Authentication

**Goal.** Sign a user in against Azure AD, mint the full Teams token set and
persist it securely, without letting the network leak into the UI or the tests.

**Landed.** New module `crates/squads-core/src/auth/`:

- `tokens` — the five-token model. `StoredToken` (`token` + absolute
  `expires_at`, five-minute refresh skew), `TokenKind`
  (`Spaces`/`Skype`/`Graph`/`Ic3`/`Recorder`), and `TokenSet` (refresh token,
  tenant, `region_gtms`, all five tokens) with per-kind accessors,
  `expired_kinds()`/`needs_refresh()` and a **redacting `Debug`** that never
  prints token contents.
- `pkce` — RFC 7636 helper: CSPRNG verifier (43 chars from 32 random bytes),
  `BASE64URL-NOPAD(SHA256(verifier))` challenge, random `state`, authorize-URL
  builder and redirect parser (`code`/`state` and `error`/`error_description`).
  Includes the RFC 7636 Appendix B known-answer test.
- `device_code` — RFC 8628 types: `DeviceCodeStart`, `TokenResponse` and
  `PollResult` with correct mapping of `authorization_pending`, `slow_down`,
  `authorization_declined`/`access_denied`, `expired_token` (and a hard error
  for `bad_verification_code`).
- `store` — `TokenStore` trait; `FileTokenStore` (default; one JSON file per
  profile under `AppDirs::config`, atomic temp-file+rename, forced mode `0600`
  on Unix) and `MemoryTokenStore`. `KeyringTokenStore` is documented but
  deferred behind a default-off `keyring` feature (ADR-008).
- `flow` — `AuthTransport` (native async-in-trait, every future `Send`) +
  `HttpTransport` (real AAD/`authsvc`/refresh grants, reusing the protocol
  crate's shared HTTP pool) + `AuthDriver` orchestrating
  device-code → poll → Skype exchange → Graph/IC3/Recorder grants → persist, and
  a `refresh()` that re-mints **only** the expired tokens. Progress is emitted as
  `SignInProgress` values (no stdout).
- `session` — wired in: `restore()`, `begin_sign_in(use_personal)`,
  `begin_sign_in_with_driver(..)` (fake transports), `poll_sign_in()`,
  `sign_out()`, `mark_ready()`, `profile()` and `tokens()`. Sign-in runs on a
  spawned task and reports progress over an `mpsc` channel.

**Decisions.** ADR-007 (async-in-trait transport for offline tests), ADR-008
(file store default, keyring behind a feature), ADR-009 (device-code first, PKCE
later).

**How to verify manually** (real tenant; not runnable in this headless env):

1. `cargo test -p squads-core` — 43 offline unit tests.
2. On a machine with a browser, from a scratch binary or `squads-cli`, run the
   device-code flow; AAD prints a code + `https://microsoft.com/devicelogin`.
   Complete it and confirm `~/.config/squads/tokens.json` is created with mode
   `600` (`stat -c '%a' …`) and contains `spaces`/`skype`/`graph`/`ic3`/`recorder`.
3. Re-run after the access tokens expire (or edit `expires_at`) and confirm only
   expired entries change.

**Blockers.** B-002 (no live tenant here), B-004 (browser PKCE loopback listener
not implemented yet), B-005 (`exchange_skype_token` uses the vendored client, so
that one call is not unit-testable).

---

## M2 — Protocol clients & session core

**Goal.** Wrap the vendored protocol clients behind a UI-free facade, expose the
`Session`/client lifecycle and region routing, and prove a `Session` can list
chats against a fake transport — entirely offline.

**Landed.** New module `crates/squads-core/src/api/` and `Session` wiring:

- `api::tokens_to_config(&TokenSet) -> ost::config::Config` — pure mapping of the
  M1 five-token model into the vendored config. Each token carries our
  **absolute** `expires_at` straight through (not `StoredToken::new`, which
  recomputes from a lifetime), `tenant_id` follows the tenant, and
  `region_gtms` is stored as its JSON string (`None` when our blob is `null`).
- `api::ApiClient` — owns an `ost::api::client::TeamsClient` built with
  `TeamsClient::from_config` (ADR-010). Exposes `list_chats(limit)` and
  `read_messages(chat_id, limit)` (thin over `ost::api::list_chats_data` /
  `read_messages_data`), the `region_gtms` blob, resolved service URLs
  (`chat_service_url` / `chatsvcagg_url` / `middle_tier_url`) and a request
  counter + `tracing` debug spans. `Debug` is redacting; construction is
  offline (the shared HTTP pool is only used on the first call).
  `ApiClient::from_tokens(&TokenSet)` is the entry point.
- `Session` (extended): `Session::api()` builds the client lazily from the
  current token set (`NotSignedIn` when absent) and is cached until the tokens
  change; `Session::has_api()`; `Session::ensure_fresh_tokens()` (async, needs a
  tokio runtime) refreshes when `tokens.needs_refresh()`, persists through the
  store and drops the cached client; `ensure_fresh_tokens_with_driver(..)` takes
  a fake transport for offline tests. `restore`/`sign_out`/sign-in completion
  all invalidate the cached client.
- **Exit criterion (offline):** `crates/squads-core/src/api/mod.rs` starts a
  one-shot `tokio::net::TcpListener` stub that answers the CSA conversations
  request, points `region_gtms.csaBase` at it, and asserts the parsed
  `ChatInfo`s (name, group flag, sender, preview, time). `read_messages` is
  covered the same way, and `session::tests::session_lists_chats_against_local_fake`
  drives the same stub through `Session::api()`.

**Decisions.** ADR-010 (vendored embedding patch).

**Tests.** `cargo test -p squads-core` — 55 offline unit tests (43 from M1 + 12
new: 7 in `api`, 5 in `session`). `cargo test -p squads-protocol` still passes
377.

**How to verify manually** (real tenant; not runnable in this headless env):

1. Sign in (M1), then from a scratch binary call
   `Session::api()?.list_chats(20)` and `read_messages(id, 50)`.
2. Confirm the returned `ChatInfo` names for 1:1 chats resolve to mate names
   (exercises the vendored roster/history lookups) and that
   `client.chat_service_url()` matches the tenant's region.

**Blockers.** B-005 (Skype exchange is not redirectable), B-006 (the
mate-name resolution branch and its process-global mate cache are not
exercised offline).

---

## M3 — Local persistence & offline cache

**Goal.** Persist accounts, chats and messages in a local SQLite database with a
versioned schema, behind a synchronous, offline-testable API; add a small JSON
settings file; and hand the store to `Session` so the UI (M10) and realtime
(M4) can read/write through one place.

**Landed.** New modules in `crates/squads-core/src/`:

- `model` — UI-facing domain types **decoupled from the protocol crate**:
  `Chat`, `Message`, `Reaction`/`Reactor`, with `Chat::from_info` /
  `From<&ChatInfo>` and `Message::from_info(chat_id, &MessageInfo)` /
  `From<&MessageInfo>`. Two wire facts are documented rather than guessed:
  `MessageInfo` has **no** edited flag (so `Message::edited` defaults `false` and
  is set by `mark_edited`/`with_edited`), and it cannot know the signed-in user
  (so `Message::is_own` is set by `mark_own(self_mri)`, tolerant of both a full
  `8:orgid:<guid>` MRI and a bare OID). `Chat.unread`/`pinned` are local-only UI
  flags, not persisted in v1. `Message`'s `Debug` deliberately prints `raw` only
  as a length, so HTML bodies are not dumped into logs.
- `store` — `Store`, a synchronous, `Send + Sync` (`Mutex<Connection>`)
  `rusqlite` (bundled) handle. `Store::open(path)` / `Store::open_in_memory()`;
  pragmas `journal_mode=WAL`, `foreign_keys=ON`, `busy_timeout=5s`,
  `synchronous=NORMAL`; **versioned migrations** driven by `PRAGMA user_version`
  (starts at v1, forward-only, applied in one transaction, every DDL statement
  `IF NOT EXISTS` so re-running is safe); a database newer than this build errors
  with `CoreError::StoreSchemaVersion`.
  Schema v1: `accounts(profile PK, …)`, `chats(id, profile, …)` with
  `PRIMARY KEY(id, profile)`, `messages(id, chat_id, …)` with
  `PRIMARY KEY(id, chat_id)`, plus indexes on `messages(chat_id, timestamp)` and
  `chats(profile, last_message_time)`. API: `upsert_chats`, `list_chats`,
  `chat`, `upsert_messages`, `messages`, `messages_before`, `delete_chat`,
  `put_account`, `get_account`, `counts`, `schema_version`. Upserts are
  idempotent (same id = update) and `updated_at` only advances
  (`MAX(existing, now)`); reactions are stored as a JSON column and a corrupt
  blob degrades to "no reactions" rather than failing the read.
- `settings` — `Settings { theme: Theme {System,Light,Dark}, window: {width,
  height} }`, serde `#[serde(default)]`, loaded from
  `AppDirs::settings_file()`, saved atomically (unique temp file + `rename`). A
  missing file yields defaults; unknown/partial fields load.
- `session` — `Session::new` now also opens the store from
  `AppDirs::store_file()`; `Session::store()` / `Session::attach_store(Arc<Store>)`
  expose/replace it, and `Session::with_token_store_and_db(..)` injects both
  stores (tests use an in-memory DB). The token-store accessor was renamed
  `store()` → `token_store()`.

**Decisions.** ADR-005 promoted to accepted; **ADR-011** (plain bundled SQLite,
not SQLCipher — encryption-at-rest is deferred and recorded as B-007);
**ADR-012** (`Mutex<Connection>` concurrency model).

**Tests.** `cargo test -p squads-core` — **85** offline unit tests (55 from
M1/M2 + 30 new: 8 `model`, 14 `store`, 6 `settings`, 2 `session`).
`cargo test -p squads-protocol` still passes **377**. New coverage includes:
fresh DB reaches v1; reopen + version-reset are idempotent; a newer foreign
`user_version` is a clear error; chat upsert idempotency + `last_message_time`
ordering (NULLs last); message round-trip with reactions JSON + `reply_to` +
`edited`; `messages_before` paging order; profile isolation; `updated_at`
advances; `Store::open_in_memory()`; settings JSON round-trip, atomic replace and
defaults for a missing file.

**How to verify manually** (real tenant; not runnable here): after signing in,
call `session.store().upsert_chats(profile, &chats)` from fetched data, then
`list_chats(profile, n)` in a fresh process — it must return the cached rows with
no network.

**Blockers.** B-007 (chat/message content is stored **unencrypted at rest**).
