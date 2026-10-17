# Progress

Live status of the roadmap in [`PLAN.md`](PLAN.md). Updated as work lands.

Legend: ✅ done · 🟡 in progress · ⬜ not started · ⛔ blocked (see [BLOCKERS.md](BLOCKERS.md))

| # | Milestone | Status | Notes |
|---|---|---|---|
| M0 | Workspace & scaffolding | ✅ | Workspace + 3 crates; vendored protocol core; docs. |
| M1 | Authentication | ✅ | Device-code flow, five-token model, 0600 file store, testable `AuthTransport`. |
| M2 | Protocol clients & session core | ✅ | `ApiClient` over the vendored `TeamsClient`; `tokens_to_config`; lazy `Session::api()`; token refresh; offline `list_chats`/`read_messages` tests. |
| M3 | Local persistence & offline cache | ✅ | UI-free `model`; bundled-SQLite `Store` (`user_version` migrations, idempotent upserts, per-chat paging); JSON `Settings` with atomic saves; `Session` owns the store. |
| M4 | Realtime (Trouter) | ✅ | Wrap the vendored Trouter loop; `RealtimeEvent` decode; `EventBus` broadcast fan-out; collector write-through; `Session::start_realtime`/`subscribe`. |
| M5 | Messaging | ✅ | `api` chat wrappers (page/send/reply/edit/delete/react/receipts); offline-first `Messaging` (open/load_older, optimistic `SendState` send + idempotent retry); shared `text` module; schema **v2** (`send_state`, `read_state`); `Session::messaging()`. |
| M6 | Teams & channels | ✅ | `api` teams wrappers (list teams/members, thread reply, channel/team writes); `model::{Team,Channel,TeamMember}`; schema **v3** (`teams`,`channels`,`team_members`); offline-first `Teams` (metadata cache, channel history via `Messaging`, optimistic channel post + thread reply); `Session::teams()`. |
| M7 | Files | ✅ | Graph files wrappers + `FileEntry`/`FileVersion`/`SharedLink` model; schema **v4** (`files`); offline-first `Files` (chat/folder/recents list, coarse download progress, upload simple-PUT vs resumable, links, versions, manage); `Session::files()`. |
| M8 | Presence & notifications | ✅ | Two presence backends (Graph + unified `presence.teams.microsoft.com`) behind one `Presence`; `text::mentions`; pure notification routing + `Notifier` (desktop behind a default-off feature); schema **v5** (`chats.muted`) + unread/activity derivation; `Session::presence()`/`notifications()`. |
| M9 | Audio/video calls | ✅ | `calls` layer: UI `CallState` machine + `CallDirection`; injectable epconv `CallSignaller` (create→join-SDP→invite, accept→media-answer, end); SDP/SRTP/RTP offline tests; `MediaSession` seam with `LoopbackMedia` (runs `macav::call_dry_run`), real audio behind default-off `audio`/`video-capture`; `on_notification` incoming calls (B-008 noted); recording wrappers; schema **v6** `call_log`; `Session::calls()`. |
| M10 | egui UI | ✅ | App shell (nav rail + list + timeline + composer + info), login/teams/files/calls/settings screens, tokio runtime bridge + Action pattern, virtualized timeline, headless `egui_kittest` tests. |
| M11 | Polish & packaging | ✅ | Pure `shortcuts` mapping + app dispatch; expanded forward-compatible `Settings` (accent, font scale, compact, notifications, download dir) + settings screen; light/dark + accents; `docs/PACKAGING.md` + `packaging/`; CI workflow; release `strip`; docs final pass (`STATUS.md`). |

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

---

## M4 — Realtime (Trouter)

**Goal.** Turn the vendored Trouter push stream into typed, UI-facing events and
write-through to the local store, without reimplementing the socket layer and
without any network or timing in the tests.

**Landed.** New module `crates/squads-core/src/realtime/`:

- `event` — the owned [`RealtimeEvent`] domain enum: `NewMessage { chat_id,
  message }`, `MessageEdited { chat_id, message_id, content, edited_id }`,
  `MessageDeleted { chat_id, message_id }`, `Typing { chat_id, sender_mri,
  sender_name }`, `ReadState { chat_id, viewer_mri, horizon, message_id }`,
  `ThreadUpdate { chat_id, change: ThreadChange }`, `Resync { reason }` and a
  defensive `Unknown { resource_type, raw }` that keeps the raw JSON verbatim so
  nothing is dropped. `decode(json) -> RealtimeEvent` and
  `decode_all(json) -> Vec<RealtimeEvent>` are **pure and total** (never panic):
  keys are matched case-insensitively, missing fields default, and both the
  single-object and `eventMessages`-array shapes (plus defensive `data`/`body`
  wrappers) decode. A batch yields one event per entry; `decode` returns the
  first. Native fields read: `id`, `content`, `imdisplayname`,
  `originalarrivaltime`/`composetime`, `from`, `messagetype`, `clientmessageid`,
  `properties` (incl. reactions via the public vendored
  `reaction_counts_from_values` and reply parents via `split_reply_quote`).
  `Debug` is redacting (`Unknown` prints the raw *length*, never the body).
- `bus` — `EventBus`, a `tokio::sync::broadcast` fan-out: `new(capacity)`,
  `subscribe() -> Receiver<RealtimeEvent>`, `publish(event) -> usize` (never
  blocks/errors). Lag is documented: a subscriber more than `capacity` behind
  receives `Lagged`; the free `recv(&mut rx)` helper converts that into a single
  `RealtimeEvent::Resync`, which is the client's cue to re-fetch. A `Resync` also
  flows through when the server reports message loss.
- `service` — `RealtimeService::start(handle, profile, tokens, store)` spawns two
  tasks on a caller-provided runtime: the **connection** task
  (`ost::trouter::connect_and_run_with_config(Some(cfg))`, `cfg` built by the M2
  `tokens_to_config`) and a **collector** that loops `event_hub::drain_wait`,
  decodes each payload and applies it. `stop()`/`Drop` abort both. The
  deterministic, clock-free core is `collect_once(profile, &Store, &EventBus) ->
  usize` (drains what is queued now, decodes, applies) — the loop is a thin
  wrapper, so tests drive it with no timing. Write-through: new message →
  `Store::upsert_messages`; edit → new `Store::update_message_edit` (content +
  `edited` only, so a partial edit payload cannot clobber sender/reactions);
  delete → new `Store::delete_message`; typing/read-state/thread/resync/unknown
  are bus-only in M4. The store write happens **before** the publish, so a
  consumer that reads the store on an event sees the new state.
- `session` — `Session::start_realtime()` (requires a tokio runtime and a token
  set; builds the config from the current `TokenSet` and stores the service),
  `stop_realtime()`, `realtime()`, `subscribe()` / `events()` (independent bus
  cursors). Token changes (restore, sign-in completion, refresh, sign-out) stop
  the feed so it never runs with credentials the session has moved on from.

**Decisions.** **ADR-013** (wrap the vendored Trouter loop: injected `Config`,
`event_hub` polling, collector design, no socket-layer fork; call frames are
filtered upstream).

**Tests.** `cargo test -p squads-core` — **120** offline unit tests (85 from
M1–M3 + 35 new: 19 `realtime::event`, 5 `realtime::bus`, 6
`realtime::service`, 3 `session` realtime, 2 new `store` methods).
`cargo test -p squads-protocol` still passes **377**. New coverage includes:
every `RealtimeEvent` variant with realistic fixtures; case-insensitive keys;
`eventMessages` batches; wrapper unwrapping; invalid JSON / missing ids → the
defensive `Unknown`; the bus fan-out, the zero-subscriber case and the
lag→`Resync` path; and the write-through integration (publish a new-message
fixture into `ost::event_hub`, `collect_once`, assert the row is in an in-memory
`Store` and the bus produced the event; the same for an edit updating an existing
row and a delete; `Resync` surfaced). The hub is process-global, so these tests
serialize on a module lock, use unique ids and drain first.

**How to verify manually** (real tenant; not runnable here): after signing in,
call `session.start_realtime()` on the core's tokio runtime, then
`session.subscribe()`; send yourself a message from another client and confirm a
`RealtimeEvent::NewMessage` arrives and `session.store().messages(chat_id, 10)`
has the row.

**Blockers.** B-008 (call frames are filtered out of the hub → call
notifications deferred to M9; `event_hub` is process-global; reconnect token
staleness).

---

## M5 — Messaging

**Goal.** Turn the M2 client + M3 store into the conversation service the
timeline/composer drive: offline-first history with backward paging, optimistic
sends with idempotent retry, edit/delete/reply/reactions/read receipts — all
testable offline against local TCP stubs, with a shared HTML/text story.

**Landed.**

- `api` (M2 module, extended) — typed, instrumented wrappers over the vendored
  chat calls, in the existing `ApiClient` style (redacting `Debug`, request
  counter, `tracing` spans): `read_messages_page` (keeps the `backwardLink`),
  `send_message` (caller-owned `clientmessageid` → `SentMessage`),
  `reply_message`, `find_message_by_client_id`, `edit_message`, `delete_message`,
  `send_reaction` / `remove_reaction`, `mark_read`, `read_receipts`. The
  protocol types are re-exported (`MessagesPage`, `SentMessage`, `ReadReceipt`).
  A richer **multi-request stub** (`api::http_stub`, scripted status/headers/
  bodies, full request capture incl. bodies, and an optional arrive/release gate
  for observing in-flight state) backs the offline tests; `one_shot_json_stub`
  remains.
- `model` — `SendState { Pending, Sent, Failed }` (`as_str`/`from_db`, serde,
  `Default = Sent`) added to `Message.send_state`; `with_send_state`/
  `set_send_state`; `Debug` prints it. Missing JSON/schema values default `Sent`.
- `text` — **one** shared, documented HTML⇄text module (`strip_html`,
  `html_escape`), reusing the M4 spacing-aware rules; `realtime::event` now calls
  it (its private `strip_html` is gone). Outgoing HTML stays the vendored
  fenced-code-aware `build_message_html`/`build_reply_html`. Tested for entities,
  nested tags, `<pre>`, `<at>` mentions, unicode and malformed input.
- `store` — **schema v2**. `messages.send_state` (guarded `ALTER TABLE`, existing
  rows default `'sent'`) and a `read_state(chat_id, profile, message_id, read_at)`
  table, plus methods `message`, `message_by_client_id`,
  `update_message_send_state`, `set_read_state`, `read_state`. `upsert_messages`
  persists `send_state`; `SCHEMA_VERSION = 2`.
- `messaging` — the offline-first conversation service `Messaging { profile,
  Arc<Store>, Arc<ApiClient>, in-memory cursors }`:
  - `cached` (local-only instant read) and `open` (fetch newest page, map +
    upsert, return the merged newest page + `backward_link`).
  - `load_older` (walk the stored `backwardLink`, upsert, return the older
    batch).
  - `send_text` — **optimistic**: `Pending` row (fresh `client_message_id`,
    `is_own`, id temporarily = the client id) is upserted before the POST; on
    success re-keyed to the server id and marked `Sent`; on failure marked
    `Failed` and retained.
  - `retry` — reuses the same `client_message_id`, first probing
    `find_message_by_client_id` so a landed POST is never double-sent.
  - `reply` (quote block with the parent snippet resolved from the store),
    `edit`, `delete`, `react` (add/remove, local count adjustment),
    `mark_read` (API + `read_state`), `read_receipts`.
  - `HistoryPage { messages (oldest-first), backward_link }`.
- `session` — the cached `ApiClient` is now `Arc`-shared so `Session::messaging()`
  can hand out a `Messaging` over the session's store + client (offline to
  build; `NotSignedIn` without tokens). `Session::api()` is unchanged for callers.

**Decisions.** **ADR-014** (optimistic sends + `SendState` + schema-v2
`send_state`), **ADR-015** (one shared `text` module; outgoing HTML stays
vendored), **ADR-016** (the paging cursor lives in memory, per `Messaging`
handle).

**Tests.** `cargo test -p squads-core` — **146** offline unit tests (120 from
M1–M4 + 26 new: 12 `messaging`, 9 `text`, 5 `store`). `cargo test -p
squads-protocol` still passes **377**. New coverage includes: the exact send
request shape (`<p>…</p>`, `"messagetype":"RichText/Html"`, our
`clientmessageid`) and server-id capture from the `Location` header; the
optimistic boundary (a `Pending` row exists *before* the gated stub replies,
`Sent` with the server id after); a 500 leaving `Failed` and a retry reusing the
**same** `client_message_id` (POST → probe GET → POST); edit/delete/reply/react/
mark_read request line + body against the stub with the store reflecting each
change; backward paging (page 1 `backwardLink` → `load_older` fetches page 2,
merged oldest-first, no duplicates); the **v1→v2 upgrade** (open a real v1
database → reopen → `send_state` exists and the old row defaults to `sent`); and
the `text` module cases.

**How to verify manually** (real tenant; not runnable here): sign in, build
`session.messaging()`, `open(chat)` and `load_older(chat)`; send a message and
watch the bubble go pending → sent (its id becomes the server id); send with the
network down, confirm the failed row survives, restore, and `retry` — the server
must receive exactly one message carrying the original `clientmessageid`. Edit,
delete, react and mark-read a message and confirm the store and the tenant
agree.

**Blockers.** **U-002** (untested live: real send/edit/react/mark-read, and the
channel-vs-chat reaction/receipt differences). B-007 (chat/message content is
still stored unencrypted at rest — the v2 columns add no new exposure).

---

## M6 — Teams & channels

**Goal.** Turn the M2 client + M3 store into the teams/channels service the M10
browser drives: joined teams, channels, rosters, channel history, optimistic
channel posts and thread replies, and team/channel writes routed to the surface
that actually serves them (chat service / Graph / middle tier) — all testable
offline against local TCP stubs.

**Landed.**

- `api` (M2 module, extended) — typed, instrumented wrappers over the vendored
  teams/channel calls, in the existing `ApiClient` style (redacting `Debug`,
  request counter, `tracing` spans): `list_teams` (Graph `/me/joinedTeams` +
  per-team channel fetch), `list_team_members`, `team_internal_id` (pure
  delegate) / `team_thread_id` (Graph `internalId` resolution),
  `create_channel` (Graph), `update_channel` / `delete_channel` (middle tier),
  `create_team` (middle tier), and `thread_reply` (chat service). Protocol types
  `TeamInfo` / `ChannelInfo` / `TeamMemberInfo` / `TeamCreateResult` are
  re-exported. The module docs now classify **which host serves which call**.
- `model` — `Team { id, name }`, `Channel { id, team_id, name, description,
  membership_type, web_url, is_channel }` (`Channel::from_info(team_id, &info)`
  / `From<(&str, &ChannelInfo)>`), `TeamMember { id, display_name, email, roles }`
  (`is_owner()`), and `is_channel_conversation_id(id)` (delegates to the vendored
  classifier). `Channel::is_channel` is derived from the id so a rare
  non-`tacv2`/`skype` id still lists but is not treated as a channel
  conversation.
- `store` — **schema v3**. `teams(id, profile)` name-keyed, `channels(id,
  team_id)` (+ `profile`, `is_channel`) and `team_members(id, team_id)` (+
  `profile`, `roles_json`), with `upsert_teams` / `list_teams` /
  `upsert_channels` / `channels_for_team` / `channel` / `delete_channel` /
  `upsert_team_members` / `team_members`. Upserts are idempotent and
  `updated_at` only advances; a corrupt `roles_json` degrades to "no roles".
  `SCHEMA_VERSION = 3`; the v2→v3 migration is `CREATE TABLE/INDEX IF NOT
  EXISTS`, so it is re-entrant, and the "newer version is a hard error"
  behaviour is unchanged.
- `teams` — the offline-first service `Teams { profile, Arc<Store>,
  Arc<ApiClient>, Messaging }`:
  - `cached_teams` / `list_teams` (fetch Graph, upsert teams **and** their
    channels, return cached), `cached_channels` / `channels`, `members` /
    `cached_members`.
  - `open_channel(channel_id, limit)` / `load_older_channel(channel_id)` —
    reuse `Messaging` (channel conversation ids go through the same chat-service
    endpoints as chats); the `Teams` handle holds one `Messaging` so the paging
    cursor survives between `open_channel` and `load_older_channel`.
  - `post_to_channel` — optimistic, reuses `Messaging::send_text`.
  - `reply_in_thread(channel_id, root_id, text)` — mints a `client_message_id`,
    upserts a `Pending` row under the derived thread conversation
    `<channel>;messageid=<root>` **before** the POST, calls the vendored
    `thread_reply_with_client_id`, then re-keys to the server id and marks
    `Sent` (or `Failed`, retained, on error). `thread_conversation` exposes the
    mapping.
  - `create_channel` (Graph, caches the returned channel), `update_channel`
    (middle tier, refreshes the cached row from the change args),
    `delete_channel` (middle tier, drops the cached row), `create_team` (middle
    tier, caches the new team + channels).
- `session` — `Session::teams()` mirrors `Session::messaging()`: shares the
  session's store and `Arc<ApiClient>`, offline to build, `NotSignedIn` without
  tokens.

**Decisions.** **ADR-017** (schema v3: teams/channels/team_members), **ADR-018**
(channel routing — chat service for messages, Graph for metadata, middle tier
for the writes Graph refuses; and the thread-reply conversation mapping
`<channel>;messageid=<root>`).

**Tests.** `cargo test -p squads-core` — **166** offline unit tests (146 from
M1–M5 + 20 new: 3 `model`, 3 `store`, 13 `teams`, 1 `session`). `cargo test -p
squads-protocol` still passes **377**. New coverage includes: `list_teams`
parsing + caching teams and channels into the store (stub `/me/joinedTeams` +
`/teams/{id}/channels`); `channels` refresh; member parsing incl. owner roles +
caching; `open_channel`/`load_older_channel` paging through the chat service;
`post_to_channel` optimistic pending→sent + exact RichText request shape;
`reply_in_thread` hitting the thread-reply path (URL **and** body carry the root
id) with the optimistic pending row stored under the thread conversation and the
same pending→sent re-key, plus a failure leaving a `Failed` row; `create_channel`
on Graph; `update_channel`/`delete_channel` PATCH/DELETE on the middle-tier base
(percent-encoded `/beta/teams/{thread}/channels/{channel}`, AAD bearer +
`X-Skypetoken`, never the Graph token); `create_team` POSTing the web body to
`/beta/teams/create`; the **v2→v3 upgrade** (open a hand-built v2 database →
reopen → the three tables exist and are writable); and the
`is_channel_conversation_id` classification.

**How to verify manually** (real tenant; not runnable here): sign in, build
`session.teams()`, `list_teams()` and confirm the teams/channels appear and are
cached (`cached_teams()` in a fresh process, no network); `members(team)` shows
owners; `open_channel(id, 50)`/`load_older_channel(id)` page history;
`post_to_channel` posts a top-level message and the bubble goes pending → sent;
`reply_in_thread(channel, root, text)` posts into the thread (the reply's
`conversationLink` must be `<channel>;messageid=<root>`) and renders under the
thread, not the channel root. Then `create_channel`, `update_channel`,
`delete_channel` and `create_team` and confirm the tenant agrees.

**Blockers.** **U-003** (untested live: channel reactions/receipts are
best-effort and **not wired**; `create_channel` rides Graph in the vendored
crate, not the middle tier; the exact middle-tier/Graph write acceptance is
unproven). B-007 (channel/team metadata joins chat content in the unencrypted
store).

---

## M7 — Files

**Goal.** Turn the M2 client + M3 store into the files service the M10 browser
drives: browse the files shared in chats/channels, drill into folders, list
recents, download with progress, upload (simple ≤4 MiB + resumable >4 MiB),
sharing links, version history and the rename/move/copy/delete manage actions —
all testable offline against local TCP stubs.

**Landed.**

- `api` (M2 module, extended) — typed, instrumented Graph wrappers in the
  existing `ApiClient` style (redacting `Debug`, request counter, `tracing`
  spans): `list_chat_files` (chat/channel Shared list; `include_folders`),
  `list_message_files` (timeline file chips), `list_folder_children`,
  `list_drive_recents`, `list_file_versions`, `restore_file_version`,
  `download_file`, `upload_file` (optional progress callback), `create_link`,
  `rename_file`, `move_file`, `copy_file`, `delete_file`. Protocol types
  `SharedFile` / `FileVersion` / `UploadProgress` are re-exported. The module
  docs now note that **files are Graph throughout** and that a chat list is
  mined from chat-service history + `/shares/{u!…}/driveItem` resolution while
  a channel list rides the channel `filesFolder`.
- `model` — `FileEntry` (owned mirror of `SharedFile`;
  `from_shared` / `From<&SharedFile>` / `From<SharedFile>`), `FileVersion`
  (`from_wire` / `From<&FileVersion>`) and `SharedLink`, so the store and the UI
  never touch the wire projection. `FileEntry.drive_id` stays `Option` (an
  unresolved chat attachment lists from message metadata alone and cannot be
  downloaded).
- `store` — **schema v4**. A `files` table keyed `(id, profile)` with an
  optional owning `chat_id`, plus indexes on `(profile, chat_id)` and
  `(drive_id, id)`; methods `upsert_files(profile, chat_id, &[FileEntry])`,
  `files_for_chat(profile, chat_id)`, `file(profile, drive_id, item_id)`,
  `delete_file(profile, drive_id, item_id)`. Upserts are idempotent and
  **additive** (`COALESCE` on `chat_id`/`drive_id`/`mime`/`web_url`/
  `download_url`/`sender`/`created`/`modified`/`share_url`/`attachment_id`, so a
  recents/folder listing never wipes a chat's attachment id or a cached link;
  `updated_at` only advances). `SCHEMA_VERSION = 4`; the v3→v4 migration is
  `CREATE TABLE/INDEX IF NOT EXISTS` (re-entrant) and the "newer version is a
  hard error" behaviour is unchanged.
- `files` — the offline-first service `Files { profile, Arc<Store>,
  Arc<ApiClient> }`:
  - `cached_chat_files` (local-only) and `list_chat_files` (fetch chat/channel
    files, upsert under the chat, return wire order; folders included).
  - `message_files` (one message's file chips), `list_folder(drive_id,
    item_id)` (drill in; files **and** folders), `recents()`.
  - `download(&FileEntry, dest_path, on_progress)` — wraps
    `download_file_data`, reporting `(0, size)` before and `(written, size)`
    after. The vendored helper buffers the whole body, so the progress is
    deliberately coarse (documented); a file with no `drive_id` is rejected
    before any network.
  - `upload(chat_id, local_path, on_progress)` — wraps
    `upload_file_data_with_progress`; ≤4 MiB is a single simple PUT, larger
    files create a resumable session and PUT `<320 KiB-multiple>` fragments
    (each with `Content-Range`), then post a `reference` file message; progress
    fires once per fragment.
  - `create_link` (caches the returned `share_url` on the cached row),
    `versions`, `restore_version`, `rename`, `move_file`, `copy_file` (returns
    the async monitor URL), `delete`.
- `session` — `Session::files()` mirrors `Session::messaging()` /
  `Session::teams()`: shares the session's store and `Arc<ApiClient>`, offline
  to build, `NotSignedIn` without tokens.

**Decisions.** **ADR-019** (schema v4: a `files` table keyed `(id, profile)`
with an optional `chat_id`, and the additive-upsert rule), **ADR-020** (upload
strategy: ≤4 MiB simple PUT vs. >4 MiB resumable session; and the coarse
download progress limitation).

**Tests.** `cargo test -p squads-core` — **191** offline unit tests (166 from
M1–M6 + 25 new: 3 `model`, 3 `store`, 18 `files`, 1 `session`). `cargo test -p
squads-protocol` still passes **377**. New coverage includes: the vendored
chunk planner boundaries (0, exactly one chunk, chunk+1, several+remainder, and
a floored chunk size), `content_range_value`, `upload_session_body`, and the
Graph `u!<base64url-nopad>` share-id form (pinned as a known answer, and
**proved on the wire** by the chat-list test resolving through
`/shares/{u!…}/driveItem`); a chat Shared list parsed from a chat-service page
+ a `/shares` resolve, cached under the chat; folder children (files **and**
folders) and recents parsed from Graph stubs and cached; download writes the
file and fires `(0,size)`/`(written,size)`; a **≤4 MiB upload does a single
`PUT …/content`** while a **>4 MiB upload creates an upload session and PUTs
two fragments carrying `Content-Range: bytes 0-5242879/5242881` and
`bytes 5242880-5242880/5242881`**, each posting the reference message after; a
drive-less download rejected pre-network without a request; `create_link`
POSTing `createLink` and caching the share url without losing the chat
association; versions parsing (editor name, sparse size 0); rename/delete on
the driveItem updating the cache; copy returning the async `Location`; the
**v3→v4 upgrade** (open a hand-built v3 database → reopen → the `files` table
exists and is writable, data survives); and the additive-upsert rule (a
chatless recents listing keeps the chat association, attachment id and sender;
a copy-link upsert keeps them too).

**How to verify manually** (real tenant; not runnable here): sign in, build
`session.files()`, `list_chat_files(chat)` and confirm the Shared tab lists the
files (and that `cached_chat_files(chat)` returns them in a fresh process with
no network); drill into a folder with `list_folder(drive_id, item_id)`;
`recents()` lists OneDrive/SharePoint recent files; `download(&file, path, …)`
writes the file and `upload(chat, path, …)` posts it (a >4 MiB file must create
an upload session and PUT chunks with `Content-Range`); `create_link` returns a
view-only link; `versions` lists history and `restore_version` restores one;
`rename`/`move_file`/`copy_file`/`delete` and confirm the tenant agrees.

**Blockers.** **U-004** (untested live: real SharePoint/OneDrive acceptance of
the Graph list/resolve/upload/download/createLink/versions/manage calls; the
chat-service file post's `reference` shape and the attachment-GUID-from-eTag
bubble match; the resumable-session fragment size/`202` semantics; and the
chat-vs-channel upload routing). B-007 (file names/URLs/senders join chat
content in the unencrypted store).

---

## M8 — Presence & notifications

**Goal.** Turn the M2 client/Graph surface and the M3 store into (a) a
transport-injectable presence service spanning Teams' two presence services and
(b) a notification router plus unread/activity derivation — all testable offline
against local TCP stubs.

**Landed.**

- `text` (M5 module, extended) — `mentions(raw) -> Vec<Mention { mri, name }>`
  (M8). Recognises Teams `<at …>Name</at>` tags (index-only, `id="8:…"`,
  `mri="…"` and `userId="…"` attributes; inner text HTML-stripped) **and**
  plain-text `@name` tokens at a word boundary. Pure and total: malformed tags
  are skipped, an email-like `a@b.c` is not a mention, and it never panics.
- `presence` (new) — the M8 presence service:
  - Types: `Availability` (the real wire strings — `Available`, `AvailableIdle`,
    `Busy`, `BusyIdle`, `DoNotDisturb`, `BeRightBack`, `Away`, `Offline`; an
    `is_available()` helper; unknown → `PresenceUnknown`), `Activity` (`InACall`,
    `Presenting`, `OffWork`, …; unknown → `Unknown`), `ContactPresence { mri,
    availability, activity, status_message }`, `OwnPresence`.
  - A `PresenceBackend` enum of two impls: **`GraphBackend`** (wraps the
    vendored `get_presence_data`/`set_presence` through a new
    `ApiClient::get_presence` / `set_user_preferred_presence` — `GET
    /me/presence`, `POST /me/presence/setUserPreferredPresence`) and
    **`UnifiedBackend`** (reqwest against `presence.teams.microsoft.com`:
    `POST {base}/v1/presence/getpresence/` with `{"ids":[…]}` and
    `PUT {base}/v1/me/forceavailability/`). The unified base is **overridable**
    (`region_gtms.presenceBase` or an explicit base) and it takes the **token to
    send** as a parameter (Bearer) — see ADR-021/B-009.
  - `Presence` service (mirrors `Teams`/`Files`): `get(ids)` (batch; Graph has
    no batch, so it returns empty and `own()` is the Graph read), `own()`,
    `set(availability, activity)`, with in-memory caching
    (`cached`/`cached_all`/`cached_own`).
  - Pure, defensive mapping: `Availability::from_wire` / `Activity::from_wire`
    (case/`_`/`-` tolerant, unknown → sentinel),
    `OwnPresence::from_graph(&PresenceInfo)`, `parse_unified_presence` /
    `parse_unified_own` (top-level array, `value`/`presence`/`presences`/`result`
    wrappers, or a single entry; availability/activity flat **or** nested under
    `presence`; status message as a string or `{message|text}`; entries without
    an MRI are dropped; never panics).
- `notifications` (new) — routing + unread/activity:
  - `Notification { kind, chat_id, title, body, mention }` and `NotificationKind`
    (`NewMessage`, `Mention`, `Reply`, plus `Reaction`/`MissedCall`
    placeholders — no reaction/call realtime event exists yet, B-008).
  - `Notifier` trait (`Send + Sync`) with `LogNotifier` (default),
    `RecordingNotifier` (tests) and a `notify-rust`-backed `DesktopNotifier`
    behind the **default-off** `desktop-notifications` cargo feature (ADR-022).
  - Pure `route(event, ctx)`: only a `NewMessage` from someone else in a
    non-muted chat notifies; a message that mentions us (by MRI or display name,
    via `text::mentions`) is `Mention`, a reply to one of our own messages is
    `Reply`, else `NewMessage`. Edits, deletes, typing, read-state, thread
    updates, resyncs and unknowns return `None`.
  - `Notifications` service: `notify_event` (route + deliver), `unread_summaries`,
    `unread_total` and `activity(ctx, limit)` derived from the store.
- `store` — **schema v5**. `chats.muted` (guarded `ALTER TABLE`; existing rows
  default 0) persists the local per-chat mute, with `set_chat_muted` /
  `is_chat_muted` / `muted_chats`. Unread is **derived**, not stored:
  `unread_count(profile, chat)` and `unread_messages(profile, chat, limit)`
  (newest-first) count messages newer than the `read_state` horizon.
  `SCHEMA_VERSION = 5`; the v4→v5 migration is forward-only and re-entrant.
- `session` — `Session::presence()` (unified, base from
  `region_gtms.presenceBase`, skype token), `Session::graph_presence()`,
  `Session::notifications()` (store + notifier), `Session::notifier()` /
  `Session::set_notifier()`.

**Decisions.** **ADR-021** (two presence backends, unified by default, and the
unverified unified token/audience — defaults to the skype token). **ADR-022**
(the `Notifier` trait and desktop notifications behind a default-off feature).

**Tests.** `cargo test -p squads-core` — **230** offline unit tests (191 from
M1–M7 + 39 new: 7 `text` mentions, 15 `presence`, 12 `notifications`, 3 `store`,
2 `session`). `cargo test -p squads-protocol` still passes **377**. New coverage
includes: `Availability`/`Activity` wire mapping incl. unknowns and aliases; the
unified mapping fixtures (array/wrapper/nested, unknown values, status-message
shapes, entries without an MRI); the **unified backend against a local stub**
asserting the path (`POST /v1/presence/getpresence/`, `PUT
/v1/me/forceavailability/`), the `{"ids":[…]}` / availability+activity bodies and
the `authorization: Bearer skype-token` header; the **Graph backend through the
existing stub** (`GET /me/presence` with the Graph token) and the
`setUserPreferredPresence` body; mention detection (`<at>` with/without an MRI,
`@name`, entities, email rejection, malformed tags); the routing rules (own →
None, muted → None incl. a muted mention → None, mention → `Mention`, reply-to-own
→ `Reply`, ordinary → `NewMessage`, every non-new-message event → None); unread
derivation from `read_state` (no horizon → all unread, horizon → the remainder,
newest-first, profile isolation); and the **v4→v5 upgrade** (a hand-built v4
database gains `chats.muted`, defaults to unmuted, and round-trips through a
reopen).

**How to verify manually** (real tenant; not runnable here): sign in, build
`session.presence()` and `get(&[mri])`/`own()` for a colleague and yourself;
`set(Availability::Busy, None)` and confirm the tenant shows Busy. Subscribe to
`session.subscribe()`, feed `notify_event`, and confirm a colleague's message
notifies, your own does not, and a muted chat stays silent. `unread_summaries()`
after `Messaging::mark_read` drops the badge.

**Blockers.** **B-009** (the unified presence token audience/Bearer choice is
**unverified**; the Graph presence routes 403 live with the Teams first-party
token). **B-010** (desktop notifications cannot be exercised headlessly; quiet
hours are not implemented). **U-005** (live presence acceptance, the unified
request/response shapes, and the mute/reaction/call-event derivations are
unverified).



---

## M9 — Audio/video calls

**Goal.** Turn the vendored `ost::calling` protocol code into the call layer the
M10 call screen drives: a UI-facing call state machine, injectable epconv
signalling (place/accept/end), an offline-testable media seam, incoming-call
handling and local call history — all testable **offline and deterministically**,
with no device, camera, microphone, tenant or network in the default build.

**Landed.** New module `crates/squads-core/src/calls/`:

- `state` — the UI lifecycle [`CallState`] (`Idle`, `Placing`, `Ringing`,
  `Connecting`, `Connected`, `Ended { reason }`), [`EndReason`] and the
  [`CallMachine`] holding a [`CallDirection`]. Deliberately **independent of the
  vendored `CallState`**. Every transition is explicit and checked (`place`,
  `ring`, `connecting`, `connected`, `end`, `reset`); an illegal move returns a
  [`TransitionError`] rather than corrupting the state.
- `signaling` — [`CallSignaller`], an injectable-`reqwest::Client` +
  injectable-endpoint wrapper over the vendored two-phase epconv flow:
  [`CallSignaller::place_call`] (`create_conversation` → `join_conversation_with_sdp_video`
  → `invite_user`), [`CallSignaller::accept_incoming`] (`accept_call_with_video`
  → `send_media_answer`) and [`CallSignaller::end_call`]. [`CallEndpoints`]
  derives `epconv_url` from `region_gtms` (`epconvUrl` → vendored
  `calling_conversationServiceUrl` → derivation from
  `calling_potentialCallRequestUrl` → documented fallback) and takes the
  `trouter_surl` from `region_gtms.trouterSurl`. [`CallIdentity`] decodes the
  skype JWT (pure) to recover our MRI/OID/name/tenant. Offers come from
  `sdp::generate_sdp_offer` / `generate_av_sdp_offer`; answers from
  `sdp::generate_sdp_answer_full`.
- `media` — the [`MediaSession`] trait (start/stop/send-audio/receive) with
  [`LoopbackMedia`] (runs `macav::call_dry_run` — the whole tone→PCMU→RTP→SRTP→
  echo + video NAL pipeline — and loop-backs PCMU frames), `AudioMedia` behind
  the default-off `audio` feature and a documented `VideoMedia` **placeholder**
  behind `video-capture`.
- `incoming` — [`IncomingCall`] + pure [`parse_incoming`] over the vendored
  `parse_call_notification` (caller, participants, modalities, SDP offer,
  acceptance/media-answer/end links). The module documents **B-008** (call frames
  are filtered out of `event_hub`, so no producer exists yet) and the
  recommended fix.
- `recording` — [`CallRecorder`] wrapping the vendored recording module:
  socket-free [`CallRecorder::add_bot`] / [`CallRecorder::stop`] (request-shape
  tested offline) and the live-only [`CallRecorder::start`] (needs a connected
  `TrouterSocket` and a live recorder service). Pure
  [`derive_add_participant_url`] / [`extract_conversation_id`] mirror the
  vendored private helpers.
- `CallService` (in `calls`) — ties the above together: `on_notification`
  (parse → `Ringing` → record), `place` (`Placing` → `Connecting`), `accept`
  (`Ringing` → `Connecting`), `mark_connected` (`→ Connected`), `decline`/`end`
  (→ `Ended`, recording the outcome), `missed` (records `Missed` and delivers the
  M8 [`NotificationKind::MissedCall`] placeholder, finally produced). Every call
  start/end is written through to the store.

**Model & store.**

- `model` — [`CallDirection`], [`CallOutcome`] and [`CallLogEntry`]
  (`started`/`with_participants`/`finished`, deriving `duration_secs`).
- `store` — **schema v6**: a `call_log` table keyed `(id, profile)` with
  `thread_id`, `direction`, `started_at`, `ended_at`, `duration_secs`,
  `participants_json` and `outcome`, plus an index on
  `(profile, thread_id, started_at)`. Methods `upsert_call_log` (idempotent:
  start then finish update one row), `call_log`, `call_log_for_thread` and
  `call_log_entry`. The v5→v6 migration is `CREATE TABLE/INDEX IF NOT EXISTS`
  (re-entrant); a corrupt participants blob degrades to empty.

**Session.** `Session::calls()` mirrors the other services: shares the session's
store and notifier, derives endpoints from `region_gtms`, carries the
IC3/skype/recorder tokens, and decodes the identity from the skype JWT when it
names one. Offline to build; `NotSignedIn` without tokens.

**Decisions.** **ADR-023** (signalling via the vendored epconv with an injectable
HTTP client and endpoints), **ADR-024** (media behind default-off
`audio`/`video-capture` features; the loopback backs the offline tests),
**ADR-025** (schema v6 `call_log`).

**Tests.** `cargo test -p squads-core` — **281** offline unit tests (230 from
M1–M8 + **51** new: 8 `calls::state`, 13 `calls::signaling`, 7 `calls::media`,
4 `calls::incoming`, 5 `calls::recording`, 8 `calls::tests` (service), 2 `model`,
3 `store`, 1 `session`). `cargo test -p squads-protocol` still passes **377**.
New coverage includes: every legal state transition and the rejected ones (with
the reported `from`/`to`); endpoint derivation (our key → vendored key →
potential-URL derivation → fallback) and skype-JWT identity decoding (skypeid,
oid, non-JWT); **a realistic audio+video SDP offer parsed and answered**,
asserting the media lines, codecs, fresh ICE credentials, per-section
candidates and distinct SRTP keys; **RTP encode/decode** and **SRTP parse →
derive keys → create_context → protect → unprotect** round-trips (plus a
tampered-packet auth failure); the **media loopback** (`macav::call_dry_run`
success + echo detected + 5 video NALs, PCMU send/receive through a
`Box<dyn MediaSession>`); the **signalling request shapes** against a local stub
(`create_conversation` → `join-with-SDP`, the callee invite, the acceptance +
media-answer bodies, the end POST, and a no-offer failure with no request); the
**recording request shapes** (recorder-bot `addParticipant` body and the
stop-recording/stop-transcription POSTs); **`on_notification`** on a realistic
video invitation (and the non-call / malformed / stringified-body cases); the
`CallService` flows (place → connecting → connected, failed placement ending and
logging `Failed`, incoming ring → accept → connected, decline, end, missed with a
`MissedCall` notification); and the **v5→v6 upgrade** (a hand-built v5 database
gains `call_log`, rows round-trip and upsert updates in place) plus call-log
ordering and profile isolation.

**How to verify manually** (real tenant + devices; not runnable here): sign in,
build `session.calls()`, `on_notification` a captured call frame (or, once B-008
is fixed, feed the realtime feed), `accept`, start a `MediaSession`, and confirm
the state machine walks `Ringing → Connecting → Connected`; place a 1:1 and a
group call and confirm the tenant rings; check `session.store().call_log(profile,
20)` records both; record a call (needs a live recorder service) and confirm the
recording appears in OneDrive.

**Blockers.** **B-008** (call frames are filtered out of `event_hub`, so
incoming calls have no realtime producer; M9 provides the parser/seam and
documents the fix). **B-011** (the real `trouter_surl` comes from the negotiated
Trouter session, not `region_gtms`; live call acceptance/recording and real
device media cannot be exercised here). **U-006** (live acceptance of the
signalling/recording shapes, the remote-answer transition and the actual media
path are unverified).

---

## M10 — egui UI

**Goal.** Turn the UI-free core into a real egui application: an app shell
(nav rail + chat list + timeline + composer + info), the login / teams / files /
calls / settings screens, an async runtime bridge, and headless `egui_kittest`
tests — all compiling and testable **without a display, network or tenant**.

**Landed.** `crates/squads-app/src/` was restructured from the M0 stub into:

- `runtime.rs` — `AppRuntime`: owns a multi-threaded `tokio::runtime::Runtime`
  (ADR-004) and bridges it to the UI with a `tokio::sync::mpsc` channel of
  `AppEvent`s. `spawn(future)` runs a future that resolves to one `AppEvent` and
  sends it back; `spawn_task(future)` runs a fire-and-forget task that publishes
  through a cloneable `EventSender`; `drain()` returns everything pending with a
  non-blocking `try_recv`. `EventSender::send` calls `ctx.request_repaint()`
  once, so a background result wakes the UI without polling (ADR-027).
- `app.rs` — `SquadsApp` holds the `Session`, the shared `Arc<Store>`, the
  service handles (`Arc<Messaging>`/`Arc<Teams>`/`Arc<Files>`/`Arc<CallService>`,
  built from the session once signed in), the view state (page, selection,
  drafts, caches, toasts) and a `Vec<Action>`. `impl eframe::App`:
  - `logic` drains `AppEvent`s (and forwards `RealtimeEvent`s from the M4 bus
    through a spawned forwarding task), applies them, and requests a repaint
    **only when something changed**; while a device-code sign-in runs it polls
    `Session::poll_sign_in` on a 250 ms timer (the sign-in task reports over a
    std channel that cannot wake the UI).
  - `ui` composes the panels, then runs `apply_actions()` — the zapfast
    "UI → Action → apply" pattern (ADR-026). Views never touch the network.
- `theme.rs` — a small semantic `Palette` (own/other bubble, pending/failed,
  presence dots, accent) plus `install(ctx)` / `apply(ctx)`; dark by default, a
  `Settings::theme` override, re-applied on toggle and persisted.
- `views/` — one module per screen: `login` (device-code flow, work/personal
  toggle, user code + verification URL, errors), `shell` (top bar, nav rail,
  list column, central page), `chat_list` (store chats with preview, time,
  unread badge and pending/failed markers), `conversation` (a
  `ScrollArea::show_rows` **virtualized** timeline: own/other bubbles, sender,
  time, edited flag, reply context, reactions, `SendState` styling with a retry
  button, and a "load older" affordance wired to `Messaging::load_older`),
  `composer` (multiline input: **Enter sends, Shift+Enter newline**, Send
  disabled while empty, an Attach button whose action path exists), `teams`
  (expand a team → its channels → open a channel conversation), `files`
  (`Files::list_chat_files` cache with size/date and a Download action),
  `call` (place/accept/decline/end driven by `CallService` + `CallState`, the
  local recents list, and an explicit device-not-available note), and
  `settings` (theme toggle, profile, sign-out, version).
- `main.rs` — boot: tracing, `AppDirs::discover()`, `Settings::load`, build the
  app, `eframe::run_native` with the existing viewport options.
- `tests.rs` — headless UI tests via `egui_kittest` + AccessKit
  (`Harness::new_eframe`), built over an in-memory `Store` and a
  `Session` on a `MemoryTokenStore` (no network, no services): the login screen
  renders when signed out; the shell renders seeded chats and selecting one
  shows its seeded messages; typing in the composer and pressing Enter queues a
  send; the teams and files views render seeded rows; and switching nav sections
  updates the page. `eframe`'s `accesskit` feature is enabled and every input
  widget carries an AccessKit label.

**Decisions.** **ADR-026** (app architecture: one tokio runtime + channel
bridge, and the UI → Action → apply pattern), **ADR-027** (`request_repaint`
discipline: only on an applied event, never unconditionally), **ADR-028**
(headless UI testing via `egui_kittest` + `accesskit`).

**Tests.** `cargo test -p squads-app` — 5 headless UI tests. `cargo test -p
squads-core` still passes **281**; `cargo test -p squads-protocol` still passes
**377**. `cargo build --workspace` (default features) is device/network-free.

**How to run the GUI locally** (needs a display; not possible here — B-001):

1. `cargo run -p squads-app` (binary name `squads`). It opens the login screen
   when no credentials are stored; with no display this step fails (B-001).
2. Press **Sign in**; AAD shows a device code + `https://microsoft.com/devicelogin`
   in the window. Complete it and the shell appears with the cached chat list,
   then the live list arrives.
3. Click a chat: the timeline renders (cached instantly, then refreshed), the
   composer sends optimistically (pending → sent; a failure keeps a retry
   button), and the info toggle shows the side panel.
4. Nav to **Teams** (expand a team → open a channel), **Files** (select a chat →
   its files, Download), **Calls** (place/accept/end; recency list), and
   **Settings** (theme toggle persists, sign-out clears the session).

**Blockers.** **B-001** (the window cannot be shown or visually verified here;
verification is compile + the `egui_kittest` tests). **B-012** (the attachment
picker is a stub — the `Files::upload` action path exists but no file dialog
opens; device media is absent so calls are signalling-only, B-011).

---

## M11 — Polish & packaging

**Goal.** Make the app keyboard-driven, give it real persisted preferences and a
second theme, and finish the repository for other people: build/packaging notes,
CI, and a final documentation pass — all still building and testing **offline
with default features**.

**Landed.**

- `crates/squads-app/src/shortcuts.rs` (new) — the **pure** shortcut mapping.
  `resolve(key, modifiers) -> Option<Shortcut>` covers `Ctrl/Cmd+K` (quick
  switcher), `Ctrl/Cmd+F` (in-conversation search), `Esc` (close overlays /
  clear search), `Ctrl/Cmd+,` (settings), `Ctrl/Cmd+Shift+M` (mute placeholder),
  `j`/`k` and `Down`/`Up` (chat navigation) and `Ctrl/Cmd+Enter` (send). It
  looks only at the key and modifiers, so it is unit-tested with hand-built
  `egui::Modifiers` and no display (ADR-029).
- `app.rs` — the **dispatch** half: `handle_shortcuts` reads raw
  `egui::Event::Key` presses in `ui()`, and `apply_shortcut` turns each
  `Shortcut` into a queued `Action` (the M10 pipeline). New actions:
  `ToggleQuickSwitcher`, `FocusTimelineSearch`, `CloseOverlays`, `OpenSettings`,
  `NextChat`, `PrevChat`, `SendDraft`, `ToggleMute`, plus the settings actions
  below. Plain `j`/`k`/arrows are suppressed while a text field is focused, and
  `Ctrl/Cmd+Enter` is left to the focused composer so it cannot double-send.
- `views/quick_switcher.rs` (new) — a floating overlay that filters the cached
  chats and opens the picked one; `Esc`/Close closes it.
- `views/conversation.rs` — an in-conversation **message filter** (shown by
  `Ctrl/Cmd+F`) that filters the virtualized timeline, and a compact row height.
- `views/settings.rs` — the real preferences screen: theme, **accent**, **font
  scale** slider, **compact mode**, notification switches, a **download
  directory** field, and the account/sign-out block.
- `crates/squads-core/src/settings.rs` — `Settings` grows `accent`, `font_scale`,
  `compact_mode`, `notifications_enabled`, `notify_only_mentions` and
  `download_dir`, with a hand-written `Default` and **container-level**
  `#[serde(default)]` so an old file missing the new keys still loads with the
  meaningful defaults (ADR-030). `Settings::effective_font_scale` clamps a
  hand-edited value. New `Accent` enum.
- `crates/squads-core/src/notifications.rs` — `NotificationContext` gains
  `notifications_enabled` and `notify_only_mentions` (both permissive by
  default, so existing behaviour is unchanged); `route` honours them. The app
  builds the context from the persisted settings and the store's muted chats and
  routes each realtime `NewMessage` through the session's notifier.
- `theme.rs` — light **and** dark visuals, a small accent palette, the font
  scale via `Context::set_zoom_factor`, and compact spacing; re-applied and
  persisted on every settings change, and applied on the next start.
- `packaging/` — `dev.squads.squads.desktop`, a placeholder `dev.squads.squads.svg`
  icon and a short `README.md`. `docs/PACKAGING.md` has the concrete Linux
  (install + AppImage/Flatpak sketches + system libs), macOS (`.app` layout +
  `Info.plist`) and Windows (zip/MSI) instructions, the runtime prerequisites,
  and the config/data/cache locations.
- `.github/workflows/ci.yml` — push/PR: `cargo fmt -p squads-core -p squads-app
  -- --check`, `cargo clippy -p squads-core -p squads-app --all-targets` (no
  `-D warnings`), `cargo test --workspace`, `cargo build --release` (ADR-031).
- Root `Cargo.toml` — the release profile keeps `lto = "thin"` /
  `codegen-units = 1` and adds `strip = "symbols"` (release-only, so tests are
  unaffected).
- Docs — new `STATUS.md` (one-screen state, ADR list, blockers, how to verify);
  `README.md` feature matrix + run/config/troubleshooting; `docs/README.md`
  index; this PROGRESS section; ADR-029/030/031; `PLAN.md` M11 ticked.

**Decisions.** **ADR-029** (shortcuts: pure mapping + app dispatch),
**ADR-030** (settings forward-compatibility via container-level serde defaults),
**ADR-031** (CI scope — our crates only — and the release profile).

**Tests.** `cargo test -p squads-app` — **18** (the 5 M10 headless tests + 6
`shortcuts` unit tests + 2 `theme` tests + 5 new headless tests: the command
shortcuts open/close the overlays, `apply_shortcut` dispatches (including the
draft send), `j`/`k` navigate the chat list, the settings screen edits
preferences, and the notification context follows the settings). `cargo test -p
squads-core` — **285** (281 from M1–M9 + 2 `settings` and 2 `notifications`
tests: the old-file-missing-new-keys forward-compatibility case, the font-scale
clamp, disabled notifications, and notify-only-mentions). `cargo test -p
squads-protocol` still passes **377**. `cargo build --workspace` and `cargo
build --workspace --release` both succeed; `cargo fmt -p squads-core -p
squads-app -- --check` is clean.

**How to verify by hand** (needs a display; B-001):

1. `cargo run -p squads-app`, sign in, and press each shortcut:
   `Ctrl/Cmd+K` (quick switcher), `Ctrl/Cmd+F` (message search), `Esc` (close),
   `Ctrl/Cmd+,` (settings), `Ctrl/Cmd+Shift+M` (mute placeholder),
   `j`/`k` (chat navigation), `Ctrl/Cmd+Enter` (send).
2. In **Settings** choose Light + an accent, drag the font scale, toggle compact
   mode and the notification switches, set a download folder; restart and confirm
   the choices are still in effect (`~/.config/squads/settings.json`).
3. Follow `docs/PACKAGING.md` for a local install and `docs/STATUS.md` for the
   full hand-verification checklist.

**Blockers.** **B-001**/**B-012** (the UI is verified by compilation and the
headless `egui_kittest` tests, not by pixels). **B-010** (notification *delivery*
is still a log notifier; the settings switch gates routing, not a real toast).
The mute shortcut and the attachment picker remain placeholders (B-011/B-012).

### Post-M11 — cross-platform release pipeline

Added [`.github/workflows/release.yml`](../.github/workflows/release.yml) (ADR-032):
a native-runner matrix builds a downloadable archive for Linux (`.tar.gz`),
macOS Intel + Apple Silicon (`.zip` containing an ad-hoc-signed `Squads.app`) and
Windows (`.zip`), uploads them as Actions artifacts, and on `v*` tags publishes a
GitHub Release with all of them attached. Builds use `--locked --release` with
**default features**, matching the verified state. Installers
(AppImage/Flatpak/MSI) and real signing/notarization remain manual — see
`PACKAGING.md` §6 and B-013. Cutting a release: `git tag v0.1.0 && git push
origin v0.1.0`.
