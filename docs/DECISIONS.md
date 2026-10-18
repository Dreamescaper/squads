# Architecture Decisions (ADRs)

Decisions are recorded newest-last. Each entry states context, decision and consequences.

---

## ADR-001 — Reuse the MIT `ost` Teams protocol core rather than reimplementing

**Status:** accepted (M0)

**Context.** Microsoft Teams has no public API for chat or calling. The first-party
OAuth token carries 35 delegated Graph scopes but *not* `Chat.Read`/`Presence.*`, so chat
runs over the proprietary `ng.msg` chat service (authenticated with a `skypetoken`),
realtime runs over Microsoft Trouter (socket.io v1 over WSS), and calls run over epconv
SDP signalling with ICE/TURN/SRTP/RTP. All of this is reverse-engineered, fragile, and
volatile. Reimplementing it from scratch is a multi-person-year effort and cannot be
validated without a live tenant.

Two Rust implementations exist and are MIT-licensed:

- [`eisbaw/ost`](https://github.com/eisbaw/ost) — the original Rust Teams client.
- [`mrowlinson/better-teams`](https://github.com/mrowlinson/better-teams) — a vendored,
  heavily patched fork that adds files (Graph drives), presence, teams/channels, CSA and
  middle-tier routing, plus `lib.rs` exposing the modules as a reusable library.

**Decision.** Vendor the `better-teams` fork of `ost` into this repository as
`crates/squads-protocol` (package `squads-protocol`, library name `ost`, MIT). Build our
own cross-platform UI (`squads-app`) and session/orchestration layer (`squads-core`) on
top of it.

**Consequences.**

- We inherit a working, battle-tested protocol implementation for auth, chat, files,
  presence, teams/channels and calling.
- We must track upstream changes manually; the vendored code is ours to maintain.
- The interesting engineering work becomes the UI, the session/event layer, persistence,
  cross-platform media, and packaging — which is exactly the gap (`better-teams` is
  macOS-only).
- Licensing: `ost` is MIT. The better-teams-derived patches carry an
  "MIT with Microsoft exclusion" notice, preserved verbatim in
  `crates/squads-protocol/LICENSE` and `NOTICE`. We are not Microsoft, so the exclusion
  does not restrict us; it must be redistributed unchanged.

---

## ADR-002 — Cargo workspace with three crates

**Status:** accepted (M0)

**Context.** A single binary crate would mix protocol, orchestration, storage and UI,
making it hard to test the non-UI half and to keep compile times sane.

**Decision.** A workspace with:

- `crates/squads-protocol` — vendored protocol core (lib `ost`) + `squads-cli` binary.
- `crates/squads-core` — session lifecycle, config paths, event bus, local store.
- `crates/squads-app` — the egui application (`squads` binary).

**Consequences.**

- `squads-core` is unit-testable without a display or UI framework.
- The protocol crate's CLI binary is deliberately kept: in a headless environment it is
  the only way to exercise real authentication and API calls from the terminal.
- Shared dependency versions live in `[workspace.dependencies]`.

---

## ADR-003 — GPUI for the UI layer

**Status:** superseded by ADR-006 (M0)

**Context.** The reference `zapfast` chat client is *egui*, not GPUI (it is an
immediate-mode app). GPUI is Zed's retained-mode, GPU-accelerated framework. The user
requested GPUI.

**Decision.** Use `gpui` (crates.io, currently `0.2.x`) and follow its entity/render
model: `Application::new().run`, `cx.new`, `Entity<T>`, `Render`/`RenderOnce`, `cx.spawn`.

**Consequences.**

- Neovim-style retain-mode view layer must be authored from scratch; `zapfast` informs the
  *architecture* (backend-per-account, Command/Event channels, virtualized lists) but its
  view code is not portable.
- GPUI on Linux requires a display server; see `BLOCKERS.md` for the headless-verification
  limitation.
- Superseded once the RAM/CPU cost of a GPU-only toolkit was weighed against the goal
  (see ADR-006).

---

## ADR-004 — Dedicated tokio runtime in the core, bridged to the UI by channels

**Status:** accepted (M0, implementation in M2/M4)

**Context.** The protocol core is fully async on tokio (`reqwest`, `tokio-tungstenite`).
egui/eframe has no async executor at all — the update loop is synchronous — so background
work must live elsewhere regardless.

**Decision.** `squads-core` owns a multi-threaded tokio runtime on its own thread (the
"backend"), like `zapfast`. UI→core commands and core→UI events travel over channels; the
UI drains incoming events at the top of each frame and requests a repaint when the event
stream produced anything. Tokio `mpsc` receivers are executor-agnostic, so a small bridge
can integrate them into either world.

**Consequences.**

- Clean separation; the core can be driven by tests with a fake transport.
- One runtime per account (multi-account ready).
- The UI never blocks: it only reads what the core has already produced.

---

## ADR-005 — Local-first SQLite store

**Status:** accepted (M3) — see ADR-011/ADR-012 for the storage engine and
concurrency refinements

**Context.** Teams is chat-heavy; the UI must feel instant and work with stale/offline
data. `zapfast` caches everything in SQLite.

**Decision.** Persist accounts, chats, messages, files and settings in SQLite
(`rusqlite`, bundled), with a versioned schema and migrations. Realtime events
write-through; the UI reads from the store.

**Consequences.**

- Requires a schema/migration discipline and a repository abstraction in `squads-core`.
- Enables offline browsing, fast startup and cross-account search.

---

## ADR-006 — Use egui/eframe instead of GPUI (footprint priority)

**Status:** accepted (M0) — supersedes ADR-003

**Context.** The product goal is a Teams client that is *easy on RAM and CPU*. GPUI
(Zed's, Apache-2.0, `blade-graphics`) is **GPU-only**: it always creates a GPU context
(Vulkan on Linux/Windows, Metal on macOS), has no software fallback, and its dev-time
RAM baseline (GPU driver + glyph atlas + swapchain, inflated by driver RSS) is higher
than a CPU-rendered immediate-mode toolkit. On machines without a working Vulkan driver
(VMs, remote desktops, older Intel, some compositors) it silently falls back to
`llvmpipe` software Vulkan, which is *catastrophic* for CPU. GPUI is also pre-1.0 with
sparse documentation and fast-moving APIs, and our only reference client is egui.

**Decision.** Use **egui + eframe** with the **glow (OpenGL)** backend, minimal feature
set, and **strict event-driven repainting** (never call `request_repaint` unless
something is animating). Repaint-on-input keeps idle CPU near zero while avoiding a
GPU-only baseline.

**Consequences.**

- Lower RAM baseline and lower CPU when idle; OpenGL 2.1 is far more widely available
  than Vulkan, so fewer machines fall back to software rendering.
- Immediate-mode views redraw per frame — so we must not animate gratuitously or the CPU
  advantage disappears. This is an explicit design rule, documented in the app crate.
- Big ecosystem and a directly mappable reference (`zapfast`), so UI velocity is higher.
- We gain **headless UI testing** via `egui_kittest` (AccessKit-driven interactions), which
  materially mitigates the no-display constraint (B-001).
- Trade-off: immediate-mode layout is less "declarative"; complex virtualized lists need
  hand-rolled height caching (as `zapfast` does).

**Note.** Crate graph, the UI-free `squads-core`, and all protocol work are unaffected —
only `squads-app` and this document change.

---

## ADR-007 — Async `AuthTransport` trait for network-free auth testing

**Status:** accepted (M1)

**Context.** M1's sign-in orchestrates several network round-trips (device code,
token poll, Skype exchange, three refresh grants). The vendored crate's
`exchange_skype_token` uses a private shared HTTP client, and the acceptance
criteria demand a deterministic **offline** full-sign-in test. Hard-coding
`reqwest` calls into the driver would make that impossible without a live tenant.

**Decision.** Define a small `AuthTransport` trait describing exactly the
network operations the flow needs (`request_device_code`, `poll_device_code`,
`exchange_skype`, `refresh_grant`). `AuthDriver<T: AuthTransport>` is generic
over it; `HttpTransport` is the real implementation and a hand-written fake is
used in tests. Use **native async-in-trait** (`-> impl Future + Send`) rather
than the `async-trait` crate, so no extra dependency and no `Box` per call.

**Consequences.**

- The complete sign-in and refresh logic is unit-tested offline, including the
  "only re-mint expired tokens" property.
- The HTTP implementation is tested against a local one-shot TCP stub; only
  `exchange_skype` (which delegates to the vendored client) cannot be redirected
  (see B-005).
- `Session` stays non-generic: it holds only a progress channel, and a generic
  method (`begin_sign_in_with_driver`) injects any transport.

---

## ADR-008 — File token store by default; OS keyring behind a feature

**Status:** accepted (M1)

**Context.** M1 lists "OS keyring + 0600 file fallback". A keyring dependency
pulls platform backends (Secret Service/DBus on Linux) that are unavailable in
this headless CI environment, and would make the default build fragile there.

**Decision.** Ship `FileTokenStore` as the default: one JSON file per profile
under `AppDirs::config`, written atomically (unique temp file + `rename`) and
forced to mode `0600` on Unix regardless of umask. Provide
`MemoryTokenStore` for tests. Declare a default-off `keyring` cargo feature and
document `KeyringTokenStore` as a TODO; the `keyring` crate is **not** added yet.

**Consequences.**

- `cargo build`/`cargo test` work on any machine with no OS secret service.
- Tokens at rest are protected by file permissions (0600), not by the OS keyring.
  Adding the keyring is a self-contained follow-up behind the existing feature
  flag.
- The store is keyed by profile, which is how multi-account is realized
  (`default` plus `tokens-<sanitized>.json`).

---

## ADR-009 — Device-code first; browser PKCE helpers only

**Status:** accepted (M1)

**Context.** M1 asks for both a device-code flow and a browser PKCE flow. The
device-code flow is fully driveable from a headless CLI and is what the vendored
`ost` crate already uses; the browser flow additionally needs a loopback HTTP
listener and a real browser/tenant to test.

**Decision.** Implement the **device-code flow end-to-end** (request, poll with
`slow_down` handling, error-code mapping) as the primary sign-in path, and land
the **PKCE building blocks** (`verifier`/`challenge`/`state`, authorize URL,
redirect parsing, RFC 7636 KAT) as tested, reusable pieces. The loopback
listener that completes the browser flow is deferred to a follow-up (B-004)
because it cannot be verified in this environment.

**Consequences.**

- A working, testable sign-in path exists now, with no unusable UI code.
- The PKCE pieces are ready to wire into the M10 login screen / a future CLI
  subcommand without re-deriving them.

---

## ADR-010 — Vendored `ost` embedding patch: `TeamsClient::from_config` and runtime base URLs

**Status:** accepted (M2) — a small, already-applied patch to `crates/squads-protocol`

**Context.** ADR-001 says we **wrap** the vendored `ost` core instead of
reimplementing it. M2 needs to drive its chat client from *our* token store and
against a local fake, but upstream's `TeamsClient` was built for the `squads-cli`
binary:

- `new()` / `new_for_profile()` load `config.toml` from disk and may kick off an
  AAD refresh via `ost::auth::oauth::refresh_for` — neither of which we want in
  `squads-core` (we own tokens and refresh in M1/M2).
- The regional base URLs (`graphBase`, `csaBase`, `middleTier`, `chatService`,
  `chatServiceAggregator`) were read from `region_gtms` only under `#[cfg(test)]`,
  so an embedder could not route through a stub or honour `authsvc`'s regional
  endpoints at runtime.
- `list_chats_data` built its CSA URL from a hard-coded host, ignoring
  `csa_base()`.

Reimplementing the client or the chat calls would violate ADR-001; leaving it
unpatchable would make M2's exit criterion (offline `list_chats` against a fake)
impossible.

**Decision.** Apply a minimal embedding patch to the vendored crate (three
changes, no behaviour change for `squads-cli`):

1. `TeamsClient::from_config(config: ost::config::Config) -> TeamsClient` — a
   **public** constructor that builds a client from an in-memory config, with no
   disk access and no automatic refresh.
2. The `region_gtms` overrides for `graphBase`, `csaBase`, `middleTier`,
   `chatService` and `chatServiceAggregator` are honoured **at runtime**
   (the `#[cfg(test)]` gate was removed), so an embedder can point the client at
   a local stub or a regional endpoint.
3. `list_chats_data` builds its CSA URL from `csa_base()` instead of a
   hard-coded host.

**Consequences.**

- `squads-core` can own the client lifecycle: `tokens_to_config` maps our
  `TokenSet` into `ost::config::Config`, and `ApiClient` wraps the client built
  with `from_config`. Token refresh stays in our M1 `AuthDriver`.
- Region routing becomes a first-class, testable feature: the M2 exit-criterion
  test points `region_gtms.csaBase` at a local TCP stub and asserts parsed
  chats, with no live tenant.
- The patch is intentionally tiny and upstream-compatible: `new()` /
  `new_for_profile()` still behave exactly as before, so `squads-cli` is
  unaffected. `docs/PROGRESS.md` records it as a vendored divergence to
  re-apply if the crate is ever re-vendored.
- Caveat: `graph_base()` / `csa_base()` remain private / `pub(crate)`, so our
  `ApiClient` exposes the *public* resolved URLs (`chat_service_url`,
  `chatsvcagg_url`, `middle_tier_url`) plus the raw `region_gtms` blob. If a
  later milestone needs the resolved Graph/CSA URLs directly, that is a
  follow-up patch.

---

## ADR-011 — Plain bundled SQLite (`rusqlite`), not SQLCipher, for now

**Status:** accepted (M3) — encryption-at-rest deferred, tracked as B-007

**Context.** M3 needs a local, offline-first cache for accounts, chats and
messages. ADR-005 already fixed the store on SQLite. The open question is
whether to encrypt the database file. Chat content is sensitive personal data,
so "encrypt at rest" is a real product consideration. The two obvious options
are:

- **`rusqlite` with the `bundled` feature** — SQLite compiled into the binary
  (no system `libsqlite3`), plaintext file.
- **`rusqlite` with `bundled-sqlcipher`** (or `sqlcipher`) — transparent
  page-level encryption, keyed from somewhere (an OS keyring or a passphrase).

Our auth token store already has an OS-keyring story only behind a default-off
feature (ADR-008), because the headless/CI environment has no Secret Service. A
database key has the same problem *plus* a bootstrapping problem: where does the
key come from on first run, and how is it recovered if the keyring is absent?

**Decision.** Use **plain `rusqlite` with `bundled`** for M3. Do **not** add
SQLCipher yet. Keep the storage layer behind `squads_core::store::Store` so the
engine is an implementation detail that can change without touching callers.

**Consequences.**

- Zero platform dependencies: the workspace builds and tests anywhere, fully
  offline, with no key management.
- **Chat and message content (including sender names, MRIs and raw HTML) is
  stored unencrypted at rest**, protected only by the user's filesystem/OS
  account permissions. This is recorded as **B-007** and *must* be revisited
  before a general release.
- A future encryption ADR would need to decide: the key source (OS keyring vs.
  passphrase vs. a key derived from the OS account), the migration of an
  existing plaintext file to an encrypted one (and the reverse on
  downgrade), the interaction with WAL (SQLCipher encrypts WAL too), and how CI
  tests the path without a keyring. Swapping `bundled` for `bundled-sqlcipher`
  is a one-line dependency change but is *not* sufficient on its own.
- `Store`'s public API is engine-agnostic, so the swap is contained to this
  crate.

---

## ADR-012 — `Store` is `Send + Sync` via `Mutex<rusqlite::Connection>`

**Status:** accepted (M3)

**Context.** The M2/M4 core shares one store between the UI thread and the
realtime task, so the handle must be `Send + Sync` and cheap to clone. `rusqlite`
has no async API and `Connection` is `Send` but **not** `Sync`. Options
considered:

1. `Mutex<Connection>` behind an `Arc` — one serialized connection.
2. An `r2d2`/hand-rolled pool of `Connection`s — many concurrent readers.
3. Per-thread connections (thread-local), or an actor owning the connection.

**Decision.** Ship option 1: `Store { conn: Mutex<Connection> }`, shared as
`Arc<Store>`. All methods are synchronous and lock for the duration of one
statement (or one transaction for a batch). Document the serialization in the
module docs.

**Consequences.**

- `Store` is trivially `Send + Sync` (asserted by a unit test), so `Session` can
  hold `Arc<Store>` and M4 can clone it freely.
- **Access is serialized**: two threads never query concurrently. At M3's scale
  (occasional batched writes, sub-millisecond reads) this is comfortably fast,
  and it removes a whole class of "database is locked" bugs. `busy_timeout` +
  WAL still protect against other processes touching the file.
- If contention ever matters, the fix is a small reader pool (SQLite allows many
  concurrent readers under WAL) — an internal change, since callers only see
  `Arc<Store>`.
- No async DB layer: a future async caller must not hold the lock across an
  `.await`. That is a documented convention, not enforced by the type system.

---

## ADR-013 — Wrap the vendored Trouter loop; poll `event_hub` into a typed bus

**Status:** accepted (M4)

**Context.** M4 needs live Teams events (new message, edit, delete, typing,
read-state, thread updates) in `squads-core`. The vendored protocol crate already
implements Trouter end to end — socket.io v1 over WSS negotiate → session →
WebSocket → registrar, the `3:::` ack contract, gzip/base64 payload unwrapping,
and reconnect with exponential backoff (1s…64s). It exposes:

- `ost::trouter::connect_and_run_with_config(config: Option<Config>)`, a new
  SQUADS PATCH entry point that runs forever and reuses an injected in-memory
  `Config` (built by our M2 `tokens_to_config`) across reconnects;
- `ost::trouter::data_frame_event(payload) -> Option<String>`, which decodes one
  `3:::` data frame into the pushed chat-service event JSON and **deliberately
  filters call frames out** (`NGCallManagerWin` / `SkypeSpacesWeb` → `None`);
- `ost::event_hub`, a process-global, bounded (1024, drops oldest) FIFO with
  `drain` / `drain_wait` that the frame handler feeds.

The question was whether to (a) wrap that loop, (b) fork the socket layer into
`squads-core`, or (c) reconnect through a different embedding API.

**Decision.** **Wrap it (a).** `squads-core` does not touch the socket layer:

1. `RealtimeService::start` spawns `connect_and_run_with_config(Some(cfg))` on a
   caller-provided tokio runtime, with `cfg = tokens_to_config(current TokenSet)`.
   We rely on the vendored reconnect/backoff and the ack contract unchanged.
2. A **collector task** polls `ost::event_hub::drain_wait`, decodes each payload
   into our owned [`RealtimeEvent`], writes through to the shared `Arc<Store>`
   and publishes onto a `tokio::sync::broadcast` `EventBus`.
3. The collector's clock-free core is `collect_once(&Store, &EventBus) -> usize`
   (drain-now → decode → apply); the task loop is a thin wrapper, so the whole
   pipeline is unit-testable offline with no timing (`event_hub::publish` a
   fixture, call `collect_once`).

We chose the `event_hub` **polling** bridge (rather than, say, having the
vendored loop call a callback) because `event_hub` is the crate's existing
embedder boundary, it needs no further patch to `squads-protocol`, and it lets
the collector live on the core's runtime independent of the connection task.
`decode` is pure and total, so unknown wire shapes never panic and never silently
vanish: they become `RealtimeEvent::Unknown { resource_type, raw }`, which keeps
the raw JSON.

**Consequences.**

- No duplication of the fragile socket.io/registrar/reconnect logic (ADR-001);
  protocol fixes come from the vendored crate. We do not fork it.
- The full pipeline — decode, fan-out, write-through — is deterministic and
  offline-testable via `collect_once`; only the connection loop itself needs a
  live tenant.
- `event_hub` is **process-global** and bounded (oldest dropped over 1024), so
  the collector drains it fully on every pass and tests that touch it must
  serialize — recorded as B-008.
- `drain_wait` blocks the calling thread on a condition variable, so the
  collector wants a multi-threaded runtime (the core owns one, ADR-004) and its
  wait is bounded (250 ms) so `stop()` is responsive.
- **Call events are unfixable here**: `data_frame_event` filters them out before
  the hub, so `RealtimeEvent` cannot represent call signalling. Incoming-call
  handling stays in the vendored path (auto-answer unless `TEAMS_MANUAL_CALLS`)
  and is deferred to M9 — recorded as B-008.
- The vendored loop reuses one `Config` across reconnects, so a reconnect after
  the access tokens expire carries stale credentials; `squads-core` stops the
  feed whenever its own token set changes (restore/sign-in/refresh/sign-out) and
  the caller restarts it — the residual gap is recorded as B-008.

---

## ADR-014 — Optimistic sends with `SendState` and a schema-v2 `send_state` column

**Status:** accepted (M5)

**Context.** Sending has to feel instant and must never double-post. The
vendored chat service is idempotent *if* every POST for one logical send carries
the same `clientmessageid` (`ost::api::send_message_with_client_id` +
`find_message_by_client_id`, OstMac §106). The UI, meanwhile, needs a bubble it
can render before the server answers and a retry affordance when a POST fails.

**Decision.**

1. Add local-only delivery state to [`model::Message`]: a
   `SendState { Pending, Sent, Failed }` field, defaulting to `Sent` (every
   message read off the wire has, by definition, reached the server).
2. `Messaging::send_text` is **optimistic**: it mints a `client_message_id`,
   upserts a `Pending` row (`is_own = true`, id temporarily = the client id)
   *before* the network call, then POSTs. On success the row is re-keyed to the
   server id from `sent_id_from_response` and marked `Sent`; on failure it is
   marked `Failed` and **retained**. The same shape serves `retry`.
3. Persist the state in a new **schema v2** `messages.send_state` column
   (`TEXT NOT NULL DEFAULT 'sent'`), so existing databases upgrade forward and
   every pre-existing row defaults to `sent`. The `ALTER TABLE` is guarded by a
   `PRAGMA table_info` probe so re-running the migration (e.g. a reset
   `user_version`) stays safe — SQLite has no `ADD COLUMN IF NOT EXISTS`.
4. `retry` reuses the same `client_message_id`; before re-posting it calls
   `find_message_by_client_id` to discover whether the timed-out attempt landed,
   reconciling to `Sent` instead of posting twice. One logical send is never two
   server messages.

**Consequences.**

- The UI can render pending/failed bubbles and a retry button from the store
  alone; a failure keeps the typed text (the failed row survives) so nothing is
  lost.
- The re-key (delete placeholder + insert server-id row) means a message's id
  changes once, on confirmation. Consumers must look rows up by
  `client_message_id` (the stable key, via `Store::message_by_client_id`) rather
  than assume a stable id across the pending→sent transition.
- A message whose receipt names **no** server id is marked `Sent` in place and
  keeps the client id as its row id; if the realtime `NewMessage` later arrives
  with a different server id, that is a second row until a resync. Recorded as
  U-002.
- `send_state` is local-only state; it is never sent on the wire.

---

## ADR-015 — One shared text/HTML module; outgoing HTML stays vendored

**Status:** accepted (M5)

**Context.** M4 carried a *local* `strip_html` in `realtime/event.rs` because
the vendored helpers are private. M5 needs the same HTML→text behaviour for
history reads and optimistic sends, and a single escaping rule for tests. The
risk is drift: two slightly different strip implementations, or a hand-rolled
outgoing-HTML builder that disagrees with the wire.

**Decision.** Move the HTML⇄text helpers into one documented module,
`crates/squads-core/src/text.rs` (`strip_html`, `html_escape`), and reuse it
from `realtime::event` and `messaging`. `strip_html` keeps the vendored
spacing-aware rules (block-tag boundaries become one space, inline tags vanish,
common entities decoded) and is unit-tested for entities, nested tags, `<pre>`,
`<at>` mentions, unicode and malformed input. **Outgoing** HTML is *not*
reimplemented: it is always the vendored, fenced-code-aware
`ost::api::build_message_html` (and `build_reply_html` for quotes).

**Consequences.**

- One behaviour, one place to fix; the M4 tests keep passing unchanged.
- We inherit the vendored markdown-ish fenced-code dialect exactly, so our
  outgoing messages render identically to the reference clients.
- `strip_html` decodes only a small named-entity set; uncommon numeric
  references are not decoded (documented in the module).

---

## ADR-016 — The backward-paging cursor lives in memory, per `Messaging` handle

**Status:** accepted (M5)

**Context.** Backward paging walks the server's `_metadata.backwardLink`
(`ost::api::read_messages_page`). The cursor could be persisted (a new column or
table) or held in memory. Persisting it would add write-path complexity and a
fresh migration for little benefit: after a restart, re-reading the newest page
is cheap and is already the offline-first path.

**Decision.** Keep the cursor **in memory**, keyed by `chat_id`, inside the
`Messaging` service (`Mutex<HashMap<String, Option<String>>>`):
`Some(link)` = an older page is available, `None` = history exhausted, absent =
`open` has not been called. `open` sets it from the newest page's
`backward_link`; `load_older` consumes it and stores the next one.

**Consequences.**

- No extra schema/migration and no cursor write on the hot read path.
- The cursor is **per `Messaging` handle**: `Session::messaging()` returns a
  fresh handle, so a caller that pages must keep the handle it got back.
  `load_older` on a fresh handle returns nothing until `open` runs — which is
  also the correct offline-first sequence (render cache, then refresh, then
  page). Covered by a paging test.
- If a future requirement needs server-exact paging across restarts, the cursor
  can be persisted behind the existing `Messaging` API without changing callers.

---

## ADR-017 — Teams/channel metadata in a schema-v3 side cache

**Status:** accepted (M6)

**Context.** M6 needs to browse joined teams, their channels and rosters, and
cache them for an instant, offline-capable M10 browser. Two shapes were
possible: (a) reuse `chats` for channels (a channel *is* a conversation), or
(b) add dedicated tables. Reusing `chats` would conflate the chat list with the
channel list (different ordering, no team grouping, different write paths), and
channels/teams carry metadata `chats` does not (team ownership, channel
membership type / web url, roster roles).

**Decision.** Add a **schema v3** with three tables — `teams(id, profile)`,
`channels(id, team_id)` (plus the `profile` and `is_channel` columns) and
`team_members(id, team_id)` (plus `profile` and `roles_json`) — with the batch
upserts and ordered queries listed in PROGRESS (M6). IDs are the wire ids
(Graph group id for a team, `19:…@thread.tacv2` for a channel, the membership id
for a roster entry). Migration v2→v3 is `CREATE TABLE/INDEX IF NOT EXISTS`
(re-entrant); `SCHEMA_VERSION` is 3 and a newer `user_version` remains a hard
error. `roles` round-trips as a JSON column with the same
corrupt-blob-degrades-to-empty rule as message reactions.

**Consequences.**

- The channel browser reads entirely from the store; `list_teams` is the only
  network path and it write-throughs teams **and** their channels in one pass
  (the Graph listing carries channels inline).
- `channels` and `team_members` are keyed by their owning id but carry a
  `profile` column, matching the M3 `messages` convention; profile-scoped reads
  are not (yet) enforced for them, mirroring the documented U-001 assumption
  that conversation/team ids are account-unique.
- No encryption change: team/channel/member metadata joins chat content in the
  plaintext store (B-007).
- The service is free to add a `teams`/`channels` UI flag later without a new
  table, exactly as `chats.unread`/`pinned` remain local-only.

---

## ADR-018 — Channel routing: chat service for messages, Graph for metadata, middle tier for the writes Graph refuses

**Status:** accepted (M6)

**Context.** A channel looks like a chat (it is a conversation id), but Teams
splits channel operations across three hosts, and the native chat service does
not uniformly accept channel-shaped calls:

- **Messages** (history, top-level posts, thread replies) live on the native
  chat service (`ng.msg`, `skypetoken`) — the same endpoints as chats. The
  vendored crate already routes a channel *thread reply* to the derived
  conversation `<channelId>;messageid=<rootId>` (the reply-chain shape the
  server stamps on replies), rejects non-channel ids, and accepts a
  `clientmessageid` for idempotency.
- **Metadata** (joined teams, channels, rosters, the team `internalId`) is
  **Graph**.
- **Writes Graph refuses** with the first-party token — channel edit/delete and
  team creation — go to the **apps-platform middle tier**, shaped like the Teams
  web client's own requests (vendor §GRAPHSWEEP3). Channel **creation** is the
  exception: the vendored `create_channel_data` still posts to Graph
  (`POST /teams/{id}/channels`).

Reimplementing any of these in `squads-core` would violate ADR-001 and duplicate
fragile, reverse-engineered shapes.

**Decision.** `squads-core` **wraps** the vendored calls and makes the routing
explicit at the `ApiClient` / `Teams` boundary:

1. **Chat service** — `ApiClient::{send_message, read_messages_page,
   thread_reply}`; `Teams::{open_channel, load_older_channel, post_to_channel,
   reply_in_thread}` reuse [`Messaging`] / `thread_reply_with_client_id`
   unchanged. Channel conversation ids are accepted as-is.
2. **Graph** — `ApiClient::{list_teams, list_team_members, team_thread_id,
   create_channel}`.
3. **Middle tier** — `ApiClient::{update_channel, delete_channel, create_team}`;
   a group id is resolved to the team thread id first (Graph `internalId`), and
   the channel writes are keyed by that thread id.

The **thread-reply conversation mapping** is documented, not hidden: a channel
reply is stored under the derived conversation `<channel_id>;messageid=<root_id>`
with the root echoed in `Message::reply_to`; the UI must group replies by that
thread id, not the channel id. `Teams::thread_conversation` exposes the mapping
so callers cannot drift.

**Consequences.**

- Every routing decision is testable offline: the write tests point `graphBase`
  and `middleTier` at the same local stub and assert the exact method/path/auth
  (`/me/joinedTeams`, `/beta/teams/{thread}/channels/{channel}` with AAD bearer +
  `X-Skypetoken`, `/beta/teams/create`, and the chat-service
  `<channel>;messageid=<root>` reply URL).
- **Channel reactions and read receipts are not wired.** The chat-service
  reaction path 404s on channel threads and its consumption horizon 403s; the
  alternative Graph reaction paths are unverified live. They are deferred to a
  live-verified follow-up and recorded as **U-003**.
- **`create_channel` rides Graph**, not the middle tier, because that is what the
  vendored helper does; if Graph refuses it with the Teams token, routing it
  through the middle tier is a contained change in `ApiClient::create_channel`
  (the milestone brief expected the middle tier — this is a vendor-vs-brief
  discrepancy, recorded in U-003).
- If the vendored crate later moves channel creation to the middle tier, the
  wrapper picks up the change with no caller impact.

---

## ADR-019 — Files metadata in a schema-v4 cache keyed `(id, profile)`

**Status:** accepted (M7)

**Context.** M7 needs to cache the driveItems the files browser shows (chat
Shared tabs, folder children, recents, uploaded files) for an instant,
offline-capable M10. Two key choices were on the table:
`(drive_id, item_id)` — the natural Graph identity of a driveItem — or
`(id, profile)`, the convention the rest of the store already uses (`chats` and
`teams` are keyed `(id, profile)`, `messages` `(id, chat_id)`, `channels`
`(id, team_id)`). `drive_id` is **optional** in the wire projection
(`SharedFile.drive_id: Option<String>`): a chat file that would not resolve to a
driveItem lists from message metadata alone with no drive.

**Decision.** Add a **schema v4** `files` table keyed **`(id, profile)`** with
non-key `drive_id` and an optional `chat_id` column, plus indexes on
`(profile, chat_id)` and `(drive_id, id)`. `SCHEMA_VERSION` is 4 and the v3→v4
migration is `CREATE TABLE/INDEX IF NOT EXISTS` (re-entrant); a newer
`user_version` remains a hard error. API: `upsert_files(profile, chat_id,
&[FileEntry])`, `files_for_chat(profile, chat_id)`, `file(profile, drive_id,
item_id)`, `delete_file(profile, drive_id, item_id)`.

The key rationale:

- `(id, profile)` matches the store's existing convention and keeps the
  `profile` isolation uniform, while `drive_id` stays a clean `Option` — keying
  on it would force a `''` sentinel for drive-less attachments and break the
  `Option` round-trip (SQLite unique keys treat `NULL`s as distinct).
- The `file(drive_id, item_id)` lookup still filters on **both** `drive_id` and
  `id`, so the drive is honoured; a driveItem id collision across two drives in
  one profile would (worst case) have the second listing replace the first,
  which is the same bounded, untested assumption U-001 already documents for
  chat ids.
- `chat_id` is a non-key column (mirroring `channels.team_id`): a chat listing
  sets it, recents/folder listings leave it `NULL`. Upserts are **additive** —
  `chat_id`, `drive_id`, `mime`, `web_url`, `download_url`, `sender`,
  `created`, `modified`, `share_url` and `attachment_id` only overwrite when the
  incoming value is non-`NULL` (`COALESCE`), so a later recents/folder listing
  cannot wipe the attachment id that matches a message bubble, or the sharing
  link a `create_link` cached.

**Consequences.**

- The files browser reads entirely from the store; `list_chat_files` is the
  network path and write-throughs the chat's files in one pass.
- A file shared in two chats has one row whose `chat_id` follows the last
  listing; `files_for_chat` returns it for that chat only. This mirrors M6's
  one-team-per-channel model; if per-chat multi-membership is ever needed, a
  small junction table replaces the column without changing `Files`' API.
- Listed order comes from the wire (live) and from `modified DESC` (cached);
  the two can differ because a chat list is ordered by message time, not
  driveItem modified time. Documented on `Files`.
- No encryption change: file names/URLs/senders join chat content in the
  plaintext store (B-007).

---

## ADR-020 — Upload by size (≤4 MiB simple PUT, >4 MiB resumable) and coarse download progress

**Status:** accepted (M7)

**Context.** Graph offers two upload transports. Files up to **4 MiB** can go in
one `PUT /…/content`; larger files require an **upload session**
(`createUploadSession` → sequential fragment `PUT`s, each carrying a
`Content-Range`, non-final fragments answering `202`, the last `200/201` with
the driveItem). The vendored uploader already implements both and chooses by
size (`> 4 * 1024 * 1024` → session, fragments of 16 × 320 KiB = 5 MiB), then
posts the uploaded item into the chat as a `reference` attachment message
(chat service first, Graph fallback). Download has the opposite problem: the
vendored `download_file_data` **buffers the whole response body** into memory
and only then writes the file.

**Decision.** `squads-core` **wraps** the vendored transport unchanged and makes
the split explicit:

1. `ApiClient::upload_file(chat_id, local_path, progress)` and
   `Files::upload(chat_id, local_path, on_progress)` pass through to
   `upload_file_data_with_progress`. The ≤4 MiB simple-PUT vs. >4 MiB resumable
   choice, the 5 MiB fragment size and the reference-message post are the
   vendored behaviour; the wrapper only adds instrumentation and the store
   upsert.
2. Progress is `(bytes_sent, bytes_total)`, reported by the vendored code once
   per fragment (a simple PUT reports once at completion).
3. `Files::download` reports progress **around** the buffered transfer:
   `(0, size)` before and `(written, size)` after, where `size` is the cached
   driveItem size. The module docs state plainly that the body is buffered, so
   there is no streaming and no intermediate progress; a consumer that needs a
   real progress bar would have to replace the vendored helper with a streaming
   read (a future change).

**Consequences.**

- Upload and download are thin, testable wrappers; the exact wire shapes
  (`PUT …/content`, `POST …/createUploadSession`, fragment `Content-Range`,
  the `reference` post) are pinned by offline stub tests.
- **Download progress is coarse by design.** Callers must not assume a smooth
  0→100% ramp; the callback fires twice (and `size` may be `0` for an
  unresolved attachment).
- A >4 MiB upload allocates the whole file in memory (the vendored helper reads
  it into a `Vec<u8>` and the session fragmenter slices it); acceptable for now,
  recorded as a future streaming follow-up.
- Uploads inherit the vendored idempotency shape (`clientmessageid` + verify on
  retry) so a retried upload is not posted twice.


---

## ADR-021 — Two presence backends, unified by default; and the unverified unified token/audience

**Status:** accepted (M8)

**Context.** Teams exposes presence through **two different services**, and M8
must read and write it:

- **Graph** — `GET /me/presence` and `POST
  /me/presence/setUserPreferredPresence`, the vendored
  `get_presence_data`/`set_presence`. These are **own-presence only** (no batch)
  and, live, **403**: the Teams first-party OAuth token carries 35 delegated
  scopes but *not* `Presence.Read*`/`Presence.ReadWrite` (the vendored crate
  documents this itself).
- **Unified** — `presence.teams.microsoft.com`, what the Teams web/native
  clients use and what `better-teams` switched to: `POST
  /v1/presence/getpresence/` (batch) and `PUT …/v1/me/forceavailability/` (set
  own). The **token audience is not confirmed**: `better-teams` minted a token
  for the `https://presence.teams.microsoft.com` resource, but we only hold the
  M1 `spaces`/`skype`/`graph`/`ic3`/`recorder` tokens, and we cannot verify the
  accepted header/audience without a live tenant.

We also need to test all of this **offline**, which rules out hard-coding a base
URL or an un-injectable HTTP client.

**Decision.**

1. Ship **both** backends behind one `PresenceBackend` enum (the brief allowed a
   trait or an enum; an enum keeps `Session::presence()` a concrete type while
   the two impls keep their own state):
   - `GraphBackend` wraps the vendored Graph calls through the shared
     `ApiClient` (two new wrappers, `ApiClient::get_presence` /
     `set_user_preferred_presence`).
   - `UnifiedBackend` uses `reqwest` against a **base URL parameter** (default
     `https://presence.teams.microsoft.com`, overridable via
     `region_gtms.presenceBase` or an explicit base) and takes the **token to
     send** as a parameter.
2. `Session::presence()` builds the **unified** backend by default (Graph 403s
   live), reading `presenceBase` from `region_gtms` and sending the **skype**
   token as a Bearer — this is a documented, **unverified** choice, not a fact.
   `Session::graph_presence()` exposes the Graph backend for a suitably-scoped
   token.
3. All parsing is **pure and defensive**
   (`Availability::from_wire`/`Activity::from_wire`,
   `parse_unified_presence`/`parse_unified_own`): unknown values degrade to
   `PresenceUnknown`/`Unknown`, unknown response wrappers and nested
   `presence` objects are tolerated, entries without an MRI are dropped, and
   nothing panics.
4. **Schema v5** adds `chats.muted` (the local per-chat mute the router honours);
   unread is **derived** from `messages` + `read_state`, so no new table.

**Consequences.**

- The presence service is fully testable offline: the Graph leg goes through the
  existing stub, and the unified leg points its base URL at a local stub and
  asserts the exact path/body/`Bearer` header — the best verification the
  headless environment allows.
- The **unified token/audience is unverified** (B-009): if the skype token is
  the wrong one, only the token parameter changes (or a token-audience mint is
  added in M1); the request/response shapes are pinned by the stub tests.
- The unified request/response **shapes** are also unverified against the live
  service (U-005); the mapping accepts several plausible wrappers to reduce the
  blast radius when a real response is finally seen.
- The `Presence` cache is **in-memory only** (presence is ephemeral); nothing is
  persisted, so a restart starts from `PresenceUnknown` until the next fetch.
- The `GraphBackend` is retained even though it 403s live, because it is the
  vendored code path and needs no new token plumbing.

---

## ADR-022 — A `Notifier` trait, and desktop notifications behind a default-off feature

**Status:** accepted (M8)

**Context.** M8 needs to surface notifications (desktop toasts, and the unread
badges the UI derives). Delivery is inherently platform-bound: on Linux,
`notify-rust` talks to the freedesktop notification daemon over **DBus**, which
this headless/CI environment does not have (no display, no session bus) —
exactly the situation ADR-008 already faced for the OS keyring. Routing,
however, must be deterministic and offline-testable.

**Decision.**

1. Split **routing** from **delivery**. `route(event, ctx) -> Option<Notification>`
   is a pure function (own messages never notify; only new messages notify;
   per-chat mute is honoured; mentions and replies-to-own are flagged), so the
   rules are unit-tested with no I/O.
2. Delivery goes through an object-safe `Notifier` trait
   (`fn notify(&self, &Notification)`, `Send + Sync`) held as `Arc<dyn Notifier>`
   on the `Session`. Ship `LogNotifier` (default) and `RecordingNotifier`
   (tests).
3. The `notify-rust`-backed `DesktopNotifier` is gated behind a **default-off**
   `desktop-notifications` cargo feature; the default build never compiles
   `notify-rust`, DBus or a notification daemon. (The brief allowed adding the
   dependency only if it stays default-off and the default build needs none;
   that holds.)
4. **Quiet hours are not implemented.** Suppressing during a window needs a
   clock and per-user settings the brief did not settle, and any clock in the
   router would make it non-deterministic; recorded as B-010.

**Consequences.**

- The default `cargo build`/`cargo test` is unaffected by the feature: no DBus,
  no notification daemon, no display.
- The desktop path cannot be exercised here (B-010); `DesktopNotifier` compiles
  under the feature and only fails at runtime with a `tracing` warning if the
  daemon is absent.
- A future settings screen can add quiet hours purely on top of the pure router
  (pass the window into `NotificationContext`) without touching the notifier.
- Reaction and missed-call notification kinds are present as **placeholders**
  but unreachable: the M4 bus carries no reaction event (reactions arrive as
  message edits) and filters call frames out entirely (B-008), so no producer
  exists until those events do.





---

## ADR-023 — Call signalling wraps the vendored epconv flow, with an injectable HTTP client and endpoints

**Status:** accepted (M9)

**Context.** Placing/accepting/ending a Teams call runs over the reverse-engineered
**epconv** two-phase flow: `create_conversation` (POST `/api/v2/epconv`) returns a
`conversationController`, phase 2 `join_conversation_with_sdp[_video]` POSTs the
SDP offer to it, and a 1:1 additionally `invite_user`s the callee; accepting an
incoming call POSTs the acceptance link then the SDP answer to the `mediaAnswer`
link; ending POSTs the `end` link. The vendored `ost::calling::signaling` already
implements every one of these shapes (ADR-001). What M9 needed was a service the
UI can drive **and** tests can point at a local stub: the vendored functions take
an `&reqwest::Client` and explicit URLs already, but the *derivation* of those
URLs (`epconv_url` from `region_gtms`, `trouter_surl` from the negotiated Trouter
session) and the call-placement orchestration live in the vendored CLI path, not
behind a reusable API.

**Decision.** Add [`calls::signaling`] as a **thin wrapper**:

1. `CallSignaller` holds an **injectable `reqwest::Client`** and
   **injectable [`CallEndpoints`]** (`epconv_url` + `trouter_surl`); tests build it
   with a `reqwest::Client` and point both at a local TCP stub.
2. `place_call` orchestrates the vendored functions in the documented order
   (create → join-with-SDP → optional invite); `accept_incoming` runs
   accept → media-answer; `end_call` posts the end link. No wire shape is
   reimplemented.
3. `CallEndpoints::from_region_gtms` derives the endpoints from the M1
   `region_gtms` blob with a documented precedence: our `epconvUrl` key → the
   vendored `calling_conversationServiceUrl` → a derivation from
   `calling_potentialCallRequestUrl` → the documented fallback
   `DEFAULT_EPCONV_URL`. `trouter_surl` comes from `region_gtms.trouterSurl`
   (the negotiated session's `surl` is not in `region_gtms`; see B-011).
4. `CallIdentity` decodes the skype JWT (a pure base64url+JSON operation,
   mirroring the vendored private `extract_mri_from_skype_token`) to recover our
   MRI/OID/name/tenant, so `Session::calls()` needs no extra network call.

**Consequences.**

- The entire signalling request shape is pinned **offline** against a local stub
  (method/path/`Authorization: Bearer`/`X-Skypetoken`/`ms-teams-*` headers/body),
  which is the strongest verification this credential-free environment allows.
- SDP offers are generated by the vendored `sdp::generate_sdp_offer` /
  `generate_av_sdp_offer`; answers by `sdp::generate_sdp_answer_full`. The
  generated SDP advertises **placeholder** host candidates (default ports) — the
  real media ports belong to [`calls::media`], and a caller that binds sockets can
  pass its own.
- The remote *answer* is not observable over the M4 bus (B-008), so `place` ends
  at `Connecting` and the caller drives `CallService::mark_connected` once media
  is up. This is explicit rather than a silent optimistic `Connected`.
- Live acceptance of every shape remains unverified (U-006); a protocol change
  upstream is inherited because nothing is reimplemented.

---

## ADR-024 — Media behind default-off `audio`/`video-capture` features; the offline loopback backs the tests

**Status:** accepted (M9)

**Context.** A call needs a media session (capture/playback/encode). Real audio
needs `cpal` (ALSA headers on Linux) and real video needs V4L2 + `openh264` +
**SDL2**; this headless/CI environment has **no** display, camera, microphone or
SDL2 dev libraries. M9's exit criterion, though, is that the media pipeline be
exercised **where possible** — and the vendored crate already provides a fully
offline one: `macav::call_dry_run()` runs tone → PCMU → RTP → SRTP protect → SRTP
unprotect → RTP → PCMU → echo detect plus a video NAL packetize/depacketize
round-trip, with no network, auth or hardware.

**Decision.**

1. Define a `MediaSession` trait (start/stop/send-audio/receive) as the seam
   between signalling and media I/O.
2. Ship `LoopbackMedia` as the **default, always-compiled** implementation: it
   wraps `macav::call_dry_run()` (backing the exit-criterion test) and
   loop-backs PCMU frames for the send/receive contract.
3. Put real capture/playback behind **default-off** `squads-core` features that
   forward to the vendored ones: `audio` → `squads-protocol/audio` (`AudioMedia`)
   and `video-capture` → `squads-protocol/video-capture` (`VideoMedia`). The
   default build and default tests need no ALSA or SDL2, exactly as with the OS
   keyring (ADR-008) and desktop notifications (ADR-022).
4. `VideoMedia` is a **documented placeholder**: it compiles under the feature but
   is not wired (it returns `NotReady`), because SDL2 is unavailable here and the
   real wiring is a follow-up.

**Consequences.**

- `cargo build`/`cargo test` (default features) are device-free and
  deterministic; the media pipeline is still exercised end to end through the
  loopback.
- Real capture/playback cannot be exercised here (B-011) and `video-capture`
  must not be enabled in CI; the feature wiring is the only thing verified.
- A future cross-platform media implementation replaces `LoopbackMedia` behind
  the same `MediaSession` trait without touching signalling or the UI.

---

## ADR-025 — Local call history in a schema-v6 `call_log` table

**Status:** accepted (M9)

**Context.** The M10 call screen needs a recents list, but Teams exposes **no**
call-history API through the reverse-engineered surfaces (the native chat service
has no call-log endpoint, and Graph has none either). The only source is what we
observe locally: a call we placed, accepted, declined or missed.

**Decision.** Add a **schema v6** `call_log` table keyed `(id, profile)`:

- `id` is the wire `callId` when an incoming invitation names one
  (`debugContent.callId`), else a generated uuid; `thread_id` is the conversation
  (empty when the invitation does not carry it); `direction` is
  `incoming`/`outgoing`; `started_at`/`ended_at` are unix seconds;
  `duration_secs` is derived (`ended_at − started_at`, floored at 0);
  `participants_json` is a JSON array (same corrupt-blob-degrades-to-empty rule as
  `reactions_json`/`roles_json`); `outcome` is
  `completed`/`declined`/`missed`/`failed`/`cancelled`/`unknown`.
- `upsert_call_log` is idempotent: a call is written when it starts
  (`ended_at`/`duration_secs` `NULL`, outcome `unknown`) and **updated in place**
  when it ends, so one call is one row. Reads: `call_log` (newest first),
  `call_log_for_thread`, `call_log_entry`.
- `SCHEMA_VERSION` is 6; the v5→v6 migration is `CREATE TABLE/INDEX IF NOT
  EXISTS` (re-entrant), and a newer `user_version` remains a hard error.

**Consequences.**

- The recents list reads entirely from the store with no network; recording is
  best-effort (a store error is logged, never fails a call).
- Call history is **local only**: it does not sync across devices and does not
  include calls placed from other clients. Documented, not a defect of the
  protocol wrapper.
- No encryption change: call participant names join chat content in the
  plaintext store (B-007).

---

## ADR-026 — App architecture: one tokio runtime + channel bridge, and the UI → Action → apply pattern

**Status:** accepted (M10)

**Context.** egui has no async executor: `eframe::App::ui` is a synchronous
paint pass and `logic` runs even while the window is hidden. The whole core is
async on tokio (`reqwest`, `tokio-tungstenite`, the Trouter loop). ADR-004
already fixed the direction — the core owns a runtime and the UI drains a
channel — but M10 had to realise it and decide how a view turns a click into a
network call without holding a borrow of the app across an `.await` and without
mutating shared state mid-layout.

**Decision.**

1. `squads-app::runtime::AppRuntime` owns a multi-threaded `tokio::runtime::Runtime`
   and an unbounded `tokio::sync::mpsc` channel of `AppEvent`s. `spawn(future)`
   runs a future that resolves to one `AppEvent` and sends it back;
   `spawn_task(future)` runs a fire-and-forget task that publishes through a
   cloneable `EventSender` (used for the realtime forwarding loop and the
   multi-event call actions); `drain()` returns everything pending via
   `try_recv`.
2. The services are held as `Arc<Messaging>` / `Arc<Teams>` / `Arc<Files>` /
   `Arc<CallService>` so a spawned future can own a handle past the frame and
   the borrow checker is never fought: `dispatch` clones the `Arc`, moves the
   owned arguments into the `async move` block, and returns.
3. **UI → Action → apply** (the pattern `zapfast` documents): a view only reads
   state through `SquadsApp` accessors and records intent with
   `SquadsApp::push_action(Action::…)`. After the frame is drawn,
   `apply_actions()` drains the queue, logs each action and calls `dispatch`,
   which either mutates local view state or spawns the backend call. Views never
   call the network and never mutate shared state beyond local widget state.

**Consequences.**

- One runtime per app (multi-account ready, ADR-004); the UI thread never blocks
  and never awaits.
- Actions are inspectable: the applied log
  (`SquadsApp::applied_actions`) is what the headless tests assert against, so a
  widget's contract is testable without a backend.
- The bridge is executor-agnostic in both directions: the UI drains a channel it
  does not block on, and background tasks only send.
- A future multi-account UI would hold several `(Session, services)` bundles
  behind the same bridge; nothing in `AppRuntime` assumes one account.

---

## ADR-027 — `request_repaint` discipline: repaint only on an applied event

**Status:** accepted (M10)

**Context.** ADR-006 chose egui specifically to keep idle RAM/CPU low, and made
strict event-driven repainting an explicit design rule. Immediate-mode UIs
redraw every frame they are asked to; a naive app calls `request_repaint()`
every frame (or animates gratuitously) and the CPU advantage disappears. M10
adds background work whose results arrive asynchronously, which is exactly where
apps drift into polling.

**Decision.**

1. The app **never** requests a repaint unconditionally. `logic` drains the
   event channel, and requests a repaint **only when at least one event was
   applied** (`changed = !events.is_empty()`).
2. When a background task publishes an `AppEvent`, `EventSender::send` calls
   `egui::Context::request_repaint()` once. That call is conditional on there
   being an event, so an idle backend and an idle UI both cost nothing.
3. The one deliberate exception is a **device-code sign-in in progress**: the
   sign-in task reports over a `std::sync::mpsc` channel that cannot wake egui,
   so while `signing_in` the app calls `request_repaint_after(250 ms)` to poll
   `Session::poll_sign_in`. The timer is armed only during sign-in and is
   dropped the moment the flow reaches `Ready`/`Failed`.

**Consequences.**

- An idle window (signed out, or signed in with no traffic) repaints only on
  input, so idle CPU stays ~0% — the property ADR-006 set out to protect.
- Views must not animate for decoration; a spinner is acceptable only in a
  genuinely transient state (the sign-in progress indicator).
- The rule is documented at the top of `main.rs`, `runtime.rs` and `app.rs` so a
  future contributor does not "fix" a missing repaint by adding a global timer.

---

## ADR-028 — Headless UI testing with `egui_kittest` + `accesskit`

**Status:** accepted (M10)

**Context.** This environment is headless (B-001): no display, no compositor,
so the window can never be opened or screenshotted here. ADR-006 anticipated
this and cited `egui_kittest` (AccessKit-driven, no display) as the mitigation.
M10 needs to prove the screens render and respond, and the tests must be offline
and deterministic (B-002: no tenant).

**Decision.**

1. Enable `eframe`'s **`accesskit`** feature (so the accessibility tree is built
   in the normal binary too) and add `egui_kittest` with its **`eframe`** feature
   as a dev-dependency; tests drive the real `eframe::App` through
   `Harness::new_eframe`.
2. Give every input widget an AccessKit label (`ui.label(...).labelled_by(...)`
   for text fields, plain text for buttons/checkboxes), so
   `harness.get_by_label("…")` finds it and the harness's accessibility check
   passes.
3. Construct the app for tests with `SquadsApp::headless(..)`: an in-memory
   `Store` seeded directly, a `Session` on a `MemoryTokenStore`, and **no
   services, no network tasks and no realtime**. The shell is shown whenever the
   injected session reports `Ready`; everything rendered comes from the store.
4. Tests interact by label (`get_by_label(..).click()`, `type_text`, `key_press`)
   and assert either a rendered label or the app's applied-action log.

**Consequences.**

- The UI is exercised for real (layout, widget wiring, the Enter-to-send path)
  without a display, and the tests are deterministic and offline.
- The tests cover **wiring, not pixels**: they do not catch visual regressions
  (overlap, colour, spacing). Screenshot/snapshot testing would need the `wgpu`
  renderer and is out of scope here; manual QA remains a developer step
  (B-001/B-012).
- Adding a widget without a label breaks the accessibility check, which is a
  useful forcing function for accessible UI.
- The action log is a first-class test seam: a widget can be verified to have
  queued the right `Action` even when the backend call itself cannot run.

---

## ADR-029 — Keyboard shortcuts: a pure mapping in `shortcuts`, dispatch owned by the app

**Status:** accepted (M11)

**Context.** M11 asks for keyboard-driven operation (`Ctrl/Cmd+K` quick switcher,
`Ctrl/Cmd+F` in-conversation search, `Esc`, `Ctrl/Cmd+,` settings,
`Ctrl/Cmd+Shift+M` mute, `j`/`k` navigation, `Ctrl/Cmd+Enter` send). The brief
also demands the mapping be a **pure function unit-tested without a display**,
while the existing app pattern (ADR-026) is "views queue `Action`s; the app
applies them after the frame". Shortcuts therefore have two halves — *what key
means what* and *what the app does about it* — and mixing them (e.g. reading
`egui::InputState` inside a view) would make the mapping untestable headlessly.

**Decision.**

1. `crates/squads-app/src/shortcuts.rs` owns the mapping:
   `resolve(key: egui::Key, modifiers: egui::Modifiers) -> Option<Shortcut>`.
   It is **pure and total** — it looks only at the key and modifiers, never at
   app state — and is unit-tested directly with hand-built `Modifiers`.
2. Cross-platform `Ctrl`/`Cmd` is expressed with `Modifiers::command` alone
   (egui sets it to `ctrl` off macOS and to ⌘ on it).
3. `SquadsApp` owns the **dispatch**: `handle_shortcuts` reads the raw
   `egui::Event::Key { pressed: true, .. }` events from the input state and
   calls `apply_shortcut`, which turns each `Shortcut` into a queued `Action`
   (reusing the M10 action pipeline). Plain `j`/`k`/arrows are ignored while a
   text field has focus (`Context::egui_wants_keyboard_input`), so typing is not
   hijacked; `Ctrl/Cmd+Enter` is owned by the focused composer (to avoid a
   double send) and only dispatched by the global handler when no field is
   focused.
4. Shortcuts whose feature is incomplete are **documented placeholders**, not
   missing bindings: `ToggleMute` has no media device in this build and toggles
   an app-level flag plus a toast; `Send` is a no-op with no conversation open.
   The mapping and dispatch exist and are tested regardless.

**Consequences.**

- The whole mapping is covered by ordinary unit tests (no harness, no display);
  the dispatch is covered by `apply_shortcut` + the `egui_kittest` harness.
- Adding a shortcut is a two-line change (a `resolve` arm and an `apply_shortcut`
  arm) with an obvious test site.
- The mapping intentionally knows nothing about focus; that policy lives in the
  app so it can be changed without touching the pure function.

---

## ADR-030 — Settings forward-compatibility: container-level serde defaults

**Status:** accepted (M11)

**Context.** M11 grows `Settings` from the M3 placeholder
(`theme`, `window`) into the real preferences screen: `accent`, `font_scale`,
`compact_mode`, `notifications_enabled`, `notify_only_mentions` and
`download_dir`. Existing users already have a `settings.json` with only the old
keys, and it must keep loading. The M3 field types also mean a **type-default**
fallback is wrong: a missing `font_scale` must default to `1.0` (not `0.0`) and a
missing `notifications_enabled` to `true` (not `false`).

**Decision.** Keep `#[serde(default)]` at the **container** level (not per
field), so every missing key is filled from `Settings`'s hand-written `Default`
implementation — which supplies the *meaningful* defaults above — and implement
`Default` manually rather than deriving it. `Accent` and the existing `Theme`
are snake_case enums with a `#[default]` variant, so an unknown/absent value
degrades to the default. Out-of-range values (a hand-edited `font_scale`) are
clamped by `Settings::effective_font_scale` rather than trusted.

**Consequences.**

- An old or partial file loads; the M11 tests pin the "old file missing every
  new key" case and the exact default values.
- Adding a field later is a one-line change plus a `Default` entry; unknown
  *extra* fields were already tolerated by serde.
- The app applies the persisted choices on start (theme/accent/font
  scale/compact via `theme::install`, notification switches via the
  `NotificationContext`, download directory via `download_target`), so a choice
  made in the settings screen is in effect on the next launch.

---

## ADR-031 — CI scope (our crates only) and the release profile

**Status:** accepted (M11)

**Context.** M11 adds CI. The workspace contains the vendored
`squads-protocol` crate, which carries **pre-existing warnings and its own
formatting** that we must not fail on (and must not "fix", per ADR-001 — the
vendored source is kept close to upstream). Our own crates also carry a few
**intentional** lints (the `manual_async_fn` warnings on the ADR-007
`AuthTransport` impls and their test fakes), so a blanket `-D warnings` would be
wrong.

**Decision.** `.github/workflows/ci.yml` runs on push/PR:

1. `cargo fmt -p squads-core -p squads-app -- --check` — formatting for **our
   crates only**.
2. `cargo clippy -p squads-core -p squads-app --all-targets` — lint our crates,
   **without** `-D warnings` (pre-existing/intentional lints are reported, not
   fatal).
3. `cargo test --workspace` — the whole suite (including the vendored 377).
4. `cargo build --release` — the shipped configuration.

The release profile keeps `lto = "thin"` and `codegen-units = 1`, and adds
`strip = "symbols"` for a smaller binary. Stripping is release-only, so the dev
profile the tests use is untouched.

**Consequences.**

- CI is green on the vendored crate's warnings by construction, and stays
  meaningful for our code.
- The release artifact is smaller; symbolized release backtraces require
  changing one line (`strip = "debuginfo"`).
- Cross-platform bundles are *documented* (ADR/PACKAGING) but not built in CI;
  producing and signing installers needs platform runners and credentials this
  environment does not have.

---

## ADR-032 — Cross-platform release artifacts on native runners

**Status:** accepted (post-M11)

**Context.** M11 shipped packaging *instructions* but nothing downloadable: there
was no CI job producing artifacts, so a user had to build from source on each OS.
Cross-compiling the whole stack (egui/glow, winit, `native-tls`, bundled SQLite)
is fragile and would still not validate the platform's own linker/signing story.

**Decision.** Add `.github/workflows/release.yml` with a build matrix over
**native runners**:

- `ubuntu-latest` → `x86_64-unknown-linux-gnu` → `.tar.gz`
- `macos-14` → `aarch64-apple-darwin` → `.zip` (`Squads.app`)
- `macos-14` → `x86_64-apple-darwin` → `.zip` (`Squads.app`); the Intel slice
  is cross-built on the arm64 runner because GitHub retired the `macos-13`
  (x86_64) image
- `windows-latest` → `x86_64-pc-windows-msvc` → `.zip`

It builds with `cargo build --locked --release --target <triple>` using **default
features** (so `audio`/`video-capture`/`keyring`/`desktop-notifications` stay off
and no ALSA/SDL2/DBus is required), packages a per-platform archive that includes
`LICENSE`/`NOTICE`, and uploads it as an Actions artifact. On `v*` tags a final
job downloads every artifact and publishes a GitHub Release.

**Consequences.**

- Users get runnable archives for all three OSes without a toolchain; macOS ships
  a real `.app` bundle (ad-hoc signed so it launches locally).
- No signing/notarization and no AppImage/Flatpak/MSI: those need
  Developer ID / Authenticode secrets and extra tooling, and remain manual
  (B-013, `PACKAGING.md` §6). The macOS bundle therefore will not pass Gatekeeper
  for distribution.
- The matrix duplicates a per-target cache key and installs Linux system deps only
  on Linux; Windows/macOS need none for the default feature set.
- Releases are driven by tags (`git tag v0.1.0 && git push origin v0.1.0`); the
  version is taken from the tag, falling back to `[workspace.package].version`.

---

## ADR-033 — Calendar service, a schema-v7 `meetings` cache, and the join hand-off to calls

**Status:** accepted (M12)

**Context.** M12 adds a calendar tab. Teams exposes the signed-in user's
meetings through Graph `calendarView` (the vendored `calendar.rs`/`calweek.rs`
lanes), and a meeting is joined by dialing its native thread id (the existing
`CallService` path). Three things had to be decided: how the wire projection is
modeled, where meetings are cached, and how a join string becomes a call.

The vendored `MeetingInfo` projection carries only `id`, `subject`, `start`,
`end`, `join_url`, `organizer`, `organizer_email`, `is_organizer` and
`is_online`; `start`/`end` are Graph `dateTime` strings **verbatim**
(`YYYY-MM-DDTHH:MM:SS.fffffff`, no timezone — the projection drops `timeZone`).
The brief asked for `all_day`, `join_meeting_id`, `passcode`, `web_link` and
`location` too, but the projection does not expose them and the protocol crate
is off-limits.

**Decision.**

1. **Model.** `model::Meeting` is the owned, protocol-independent mirror of
   `MeetingInfo` (`Meeting::from_info` / `From<&MeetingInfo>`). The five fields
   the projection does not carry are **modeled for forward-compatibility** and
   default `false`/`None` from the wire; the type docs state this explicitly
   rather than guessing values. `start`/`end` stay the wire strings (so the
   store orders and filters them lexicographically); `Meeting::Debug` is
   redacting (a join URL can embed a passcode).
2. **Store.** A **schema v7** `meetings` table keyed `(id, profile)` (the store's
   convention), with idempotent `upsert_meetings`, `upcoming_meetings(profile,
   from_secs, limit)` (start-ordered; the `from_secs` boundary is rendered to
   ISO-8601 and compared as text against the wire `start`), `meeting(id,
   profile)` and `delete_meeting`. `SCHEMA_VERSION = 7`; the v6→v7 migration is
   `CREATE TABLE/INDEX IF NOT EXISTS` (re-entrant) and a newer `user_version`
   remains a hard error. No numeric `start_secs` is stored (the wire carries no
   timezone, so a UTC epoch would be a lie).
3. **Service.** `calendar::Calendar { profile, Arc<Store>, Arc<ApiClient> }`
   mirrors `Teams`/`Files`: `cached_upcoming` (local-only), `upcoming`
   (fetch + upsert + return the cached upcoming set), `week(start_secs, days,
   limit)` (fetch + cache), `schedule` (validate first), `cancel` (DELETE +
   drop the cache), and `resolve_join`. Every Graph call is a thin, instrumented
   wrapper on `ApiClient`.
4. **Join hand-off.** `resolve_join(target_or_url, passcode)` classifies a
   pasted string with the vendored pure `parse_join_url`: a native thread id
   (`meetup-join/19:…@thread…` or a bare thread id) → kind `thread`; another
   `https` link → `url`; anything else → `unknown`. A **numeric** meeting id is
   attempted through Graph and, because the Teams first-party token is not
   granted `OnlineMeetings.Read` (the vendored `graph_get` refuses
   `/me/onlineMeetings` via `graph_denied` before any request), falls back to
   the browser `/meet/<id>?p=` link (kind `meeting-id`). The UI hands a `thread`
   target to `CallService::place` (the existing call path) and shows every other
   target.
5. **Lobby.** `lobby_step` / the `Lobby` holder wrap the vendored pure
   `lobby_next` state machine (`idle → joining → lobby → admitted`, `failed`
   reachable, `reset` from anywhere); no network, no clock.

**Consequences.**

- The calendar tab reads entirely from the store; `upcoming`/`week` are the only
  network paths and they write-through in one pass.
- Every request shape is pinned **offline** against a local TCP stub (the
  `calendarView` path/params, the `POST`/`DELETE` bodies, the join
  classification) — the strongest verification this credential-free environment
  allows.
- **Join-by-id is unusable with the Teams sign-in** (`OnlineMeetings.Read` is
  not granted); the numeric path always returns the browser fallback. Recorded
  as U-007; if a suitably-scoped token ever appears, only the fallback branch
  changes.
- The lobby machine is pure and tested, but **no realtime producer drives it**
  (call frames are filtered out of the M4 bus, B-008); the UI feeds it from the
  call-signalling callbacks once that exists.
- No encryption change: meeting subjects/organizer names join chat content in
  the plaintext store (B-007).
- Adding a calendar field later (e.g. a real `all_day` once the projection
  carries it) is a model + v7 column + `from_info` change, no caller impact.

---

## ADR-034 — Self identity: whoami → a cached identity, schema-v8 `accounts`, and where `mark_own` is applied

**Status:** accepted (M13)

**Context.** M3 gave [`Message`](../crates/squads-core/src/model.rs) a
`mark_own(self_mri)` helper precisely because `MessageInfo` cannot know who the
signed-in user is, and M5's optimistic paths set `is_own = true` directly. But
**nothing ever obtained the MRI**: `ost::api::me::whoami_data` and
`ost::api::chat::own_member_mri` were unused, so every message loaded from the
server (`Messaging::open` / `load_older`) and every realtime `NewMessage` kept
`is_own = false`. Our own history rendered on the other side, and the M8
notification router could not recognise our own messages either. Two questions
had to be settled: how the identity is obtained and cached, and *where* the
`mark_own` pass runs.

**Decision.**

1. **Obtain it from Graph `/me`.** A new `ApiClient::whoami` wraps the vendored
   `whoami_data(client) -> UserInfo`; `model::Identity::from_whoami(&UserInfo,
   tenant)` derives the MRI with the vendored `own_member_mri` (`8:orgid:<oid>`).
   An empty id yields an empty MRI rather than a bogus `8:orgid:`.
2. **Cache it in memory *and* on disk.** `Session` holds an `Option<Identity>`
   and persists it to the `accounts` row in **schema v8** (`accounts.self_mri`,
   `accounts.oid`; guarded `ALTER TABLE`, existing rows default `NULL`). The
   app fetches whoami asynchronously after sign-in/restore and delivers it as
   `AppEvent::IdentityLoaded`; `Session::set_identity` caches and persists it,
   and `restore()` rebuilds it from the store offline, so `mark_own` survives a
   restart without a fresh whoami.
3. **Apply `mark_own` at the two places that map wire messages**, not in the UI:
   `Messaging` carries the self MRI (`with_self_mri` / `set_self_mri`, seeded
   from the persisted account so a restart keeps it) and marks every page it
   maps; the realtime collector takes the MRI
   (`collect_once` / `RealtimeService::start`) and marks a pushed `NewMessage`
   **before** it is cached and published, so the store and the bus event agree.
   The conversation view *additionally* re-marks the timeline against
   `Session::self_mri()` on reload, so history cached before whoami resolved
   still renders on the own side (a belt-and-braces pass, not the source of
   truth).

**Consequences.**

- Server-loaded history and realtime messages now carry the correct `is_own`;
  the notification router's "own message → no notification" rule works for
  pushed messages too.
- The identity is a **cache**, not a credential: an empty/stale `self_mri`
  degrades to "nothing is own" rather than mis-marking; a later whoami refresh
  re-marks.
- `accounts` gains two nullable columns and `Account` two `Option` fields
  (additive; the store's key and every other table are unchanged). B-007 still
  applies: the MRI/OID join chat content in the plaintext store.
- The self MRI is captured when the realtime feed starts; a whoami that resolves
  afterwards restarts the feed (`SquadsApp::restart_realtime`) rather than
  mutating a running collector, mirroring the existing token-change behaviour
  (B-008).
- The real whoami response and the live own-side rendering remain unverified
  here (U-008); the mapping and the `mark_own` passes are covered offline.

---

## ADR-035 — Inline media: `text::image_urls` + a deterministic `MediaCache` + `egui_extras` image loading

**Status:** accepted (M13)

**Context.** Teams message HTML carries images as `<img src="…">`; the M10
timeline rendered only `Message::content` (HTML-stripped text), so images and
GIFs never appeared. `ost::api::media` (`fetch_media_data` / `needs_auth` /
`auth_headers`) already knows how to fetch them — Microsoft media hosts
(`*.msg.teams.microsoft.com`, `*.asm.skype.com`, …) authenticate with the Skype
token; public hosts must fetch **without** it — but nothing in `squads-core`
used it, and the vendored `fetch_media_data` refuses non-`https` URLs, which
makes it untestable against a local stub. Two questions: how URLs are extracted
from the raw body, and how bytes are fetched/cached and rendered.

**Decision.**

1. **Extraction is pure and shared.** `text::image_urls(raw) -> Vec<String>`
   pulls `<img src=…>` (any quoting/attribute order, case-insensitive) plus
   obvious image links (an `http(s)://…` ending in a known image extension).
   HTML entities in the `src` are decoded, percent-encoding is preserved, and
   the function is total and de-duplicating. Object-store `src`s with **no**
   extension (`…/views/imgo`) are surfaced via their `<img>` tag, never guessed
   from a link.
2. **A small cache owns the fetch.** `media::MediaCache { dirs, token, client }`
   writes bytes under `AppDirs::media_dir()` at a **deterministic** path
   (`sha256(url)[..16]` + an image extension, `bin` when none) and returns
   `CachedMedia { url, path, mime, from_cache }`. A second `fetch` for the same
   URL returns the file with **no request**, so repeated renders never refetch.
   The token is attached only when `media_needs_auth(url)` is true, using the
   vendored `auth_headers` scheme, so it never leaks to third parties. The HTTP
   client is injectable (`with_client`), which is what makes the auth-header
   rule and the cache-hit property testable against a local stub. Writes are
   atomic and capped at `MAX_BYTES`.
3. **Rendering rides the existing bridge.** `squads-app` adds `egui_extras`
   (feature `image`) and installs its loaders in `theme::install`; the
   conversation view renders each extracted URL as an `egui::Image` from the
   cached `file://` path (bounded to 320×320) or a labelled placeholder, and
   requests the fetch with `Action::LoadMedia` — the backend answers with
   `AppEvent::MediaLoaded` / `MediaFailed`, so nothing blocks the frame and a
   repaint happens only when something changed (ADR-027).

**Consequences.**

- Images/GIFs render in the timeline; a URL is fetched once and cached, and the
  token boundary is explicit and tested.
- The cache is keyed by URL only (not by account) and is **unencrypted at rest**
  like the rest of the local cache (B-007). Bytes are buffered in memory once per
  fetch before the write (no streaming).
- The fetch/rendering path is offline-testable (URL extraction, the service
  against a stub, and a headless `egui_kittest` placeholder test); the real
  media hosts, `Content-Type`s and rendered pixels remain unverified here
  (U-008 / B-012).
- `egui_extras`/`image` are pure-Rust decoders (png/jpeg/gif/webp/…), so the
  default build gains no system dependency.

---

## ADR-036 — Message search is local-first (schema-v9 index), with the Graph server path as an optional, expected-to-403 fallback

**Status:** accepted (B1)

**Context.** Backlog item **B1** adds message search; before it the app could
only filter chat *names*. The vendored `ost::api::search` wraps Graph
`POST /search/query` (entity `chatMessage`), but that route needs the
`Chat.Read` delegated scope, which the Teams first-party sign-in token does
**not** carry — the vendored crate's own notes flag the lane as unproven, and it
is expected to **403** live (the same class of gap as Graph presence, ADR-021,
and join-by-id, ADR-033). The store, however, already holds the cached history
(`messages`), so a search that never touches the network is both possible and
the better default.

**Decision.**

1. **Local-first is the primary path.** `Store::search_messages(profile, query,
   chat_id, limit)` runs a bounded `LIKE` scan over `messages.content`, matching
   case-insensitively (SQLite's default `LIKE`, i.e. ASCII case folding), with
   the query's `LIKE` metacharacters escaped so user input matches literally.
   It is ordered `timestamp DESC, rowid DESC` (newest first, ties stable), takes
   an optional `chat_id` filter, and returns **no rows** for an
   empty/whitespace query (never "everything").
2. **Schema v9** adds the index `messages(profile, timestamp)` (forward-only,
   `CREATE INDEX IF NOT EXISTS`, re-entrant; the "newer `user_version` is a hard
   error" rule is unchanged). It lets SQLite filter by profile and walk rows in
   recency order without a temp sort. It is a **scan-support** index, not a
   lookup one: a leading-wildcard `LIKE` cannot use an index for the match
   itself, so the caller supplies the result `limit`. **FTS5 is deliberately not
   used** even though bundled SQLite ships it: `LIKE` gives the substring and
   literal-wildcard semantics the UI wants with no tokenizer, no external-content
   table and no write-path triggers to keep in sync, and the store is small.
3. **`Search` service** (`squads_core::search`) holds the shared `Arc<Store>`
   and an **optional** `Arc<ApiClient>`. `Search::local` is synchronous and
   offline (resolves each hit's chat name and builds a snippet); `Search::server`
   is the async Graph path; `Search::search` is local-first and only falls back
   to the server when the local store had **no** hits, logging and swallowing a
   server error so the feature never depends on it. The pure helpers
   `escape_like` / `like_pattern` / `snippet` live here (unit-tested).
4. **`model::SearchHit`** (`chat_id`, `chat_name`, `message_id`, `sender`,
   `snippet`, `timestamp`, `is_own`) is the shared shape the local rows and the
   mapped server hits both produce. `ApiClient::search_messages` wraps the
   vendored call. `Session::search()` is the entry point and, unlike the other
   services, **never fails**: a signed-out session gets a local-only handle.
5. **App.** A `Page::Search` with a nav-rail entry and `Ctrl/Cmd+Shift+F`. The
   M11 `Ctrl/Cmd+F` keeps its meaning (filter the open conversation); the global
   search gets the `Shift` variant so the two are distinct. Typing runs the
   synchronous local query (queued as `Action::SearchMessages`) and renders rows
   (snippet, chat name, sender, time); `Up`/`Down`/`j`/`k` move the selection and
   Enter/click queues `Action::OpenSearchHit`, which opens the chat and
   highlights the message. If the message is outside the loaded window the app
   pulls older pages until it appears or history is exhausted, and then says so
   rather than silently doing nothing. Repaints happen only when something
   changed (ADR-027); the local query is cheap.

**Consequences.**

- Search works **offline over cached history** and never depends on the server.
  The Graph path is a best-effort secondary that is expected to 403 live (see
  U-009); in practice only the local path runs.
- The v9 index is scan-support, so a search is linear in the profile's message
  count (bounded in *results*, not in *scan*). For very large histories an FTS5
  index is the documented future option; the `Store` API would not change.
- Case-insensitivity is **ASCII** (SQLite `LIKE`); a non-ASCII letter matches
  exactly. This is documented rather than silently different from what a user
  might expect of unicode.
- The jump-to-message highlight pulls one older page per event; a match older
  than the fetched chain is reported (a toast), not missed silently.
- No encryption change: a search exposes cached content in-process only (B-007).
- `crates/squads-protocol` is untouched; the search surface was already
  re-exported by `ost::api`.

---

## ADR-037 — File/people search wrappers, a schema-free `People` service, and the people-picker contract B2a consumes

**Status:** accepted (B3)

**Context.** Backlog item **B3** adds the two `api::filesearch` lanes — Graph
**drive file search** (`GET /me/drive/root/search(q='…')`) and **directory
people search** (`GET /users?$search="displayName:…"`, which needs
`ConsistencyLevel: eventual` or Graph answers `400`) — and, from the people lane,
a **reusable picker** that **B2a**'s "new chat" flow will consume. Two questions
had to be settled: how the searches are shaped (paging, limits), and what
contract the picker exposes to a caller that wants to create a chat.

The vendored helpers (`search_files_data` / `search_people_data`) are explicit
about their bounds: both are **single-window** — no paging cursor, one `$top`,
capped at `FIND_MAX_LIMIT = 25` by the pure `clamp_limit`/`*_search_path`
builders. They return wire rows (`SharedFile` / `TeamMemberInfo`), which the
store and the UI must not touch (the M3 model rule).

**Decision.**

1. **Wrappers, single window.** `ApiClient::search_files` / `search_people` are
   thin, instrumented (redacting `Debug`, request counter, `tracing` spans)
   wrappers over the vendored calls. They do **not** invent paging: a limit is
   clamped to Graph's `1..=25` window by the vendored path builder, and there is
   no `load_more`. The picker and the Files page re-search as the query changes
   instead of paging. The pure helpers (`drive_search_path`,
   `people_search_path`, `parse_*_search_response`, `FIND_MAX_LIMIT`,
   `clamp_limit`) are re-exported so their shapes are pinned without a network.
2. **Owned models.** `model::Person { id, display_name, email, job_title }`
   (`Person::from_member` / `From<&TeamMemberInfo>`), mirroring the owned-model
   rule. The directory projection selects `id,displayName,mail,userPrincipalName`,
   so `email` is `mail` when present else `userPrincipalName`; `job_title` is
   modeled for forward-compatibility and is **always `None` from the wire**
   (the projection does not select it) — documented, not guessed. File hits
   reuse the existing `FileEntry` (they are driveItems).
3. **A `People` service, not a store cache.** `People { profile,
   Arc<ApiClient> }` mirrors the other services' shape but keeps **no store**:
   directory people are not cached, so **no schema bump** and no new table. It
   exposes `search(query, limit) -> Vec<Person>`; `Session::people()` builds it
   (offline; `NotSignedIn` without tokens). `Files::search_files` *does* cache
   its driveItem hits **chatlessly** (the existing additive `upsert_files` with
   `chat_id = None`), so a searched file can be downloaded, without ever
   stealing a chat association (`COALESCE` protects it).
4. **The people-picker contract (for B2a).** The picker is a self-contained
   component whose *state* lives on `SquadsApp`, whose *intents* are `Action`s,
   and whose *result* is one accessor:
   - state: `people_query`, `people_results`, `people_selected`,
     `people_loading`, `people_picker_open`;
   - actions: `SearchPeople(String)` (typing), `TogglePersonSelected(Person)`
     (add/remove one), `ClearPeopleSelection`, `TogglePeoplePicker` (open/close);
   - async delivery: `AppEvent::PeopleResults { query, people }` (stale responses
     for a changed query are dropped);
   - **result:** `SquadsApp::selected_people() -> &[Person]`.
   B2a therefore opens the picker, lets the user pick, reads
   `selected_people()` (one person → a 1:1, several → a group chat), then clears
   with `ClearPeopleSelection`. `Esc` (through `CloseOverlays`) closes the picker
   but **keeps** the selection, so the caller can still read it. Until B2a wires
   it into the new-chat flow it is reachable from the **Search** page's "Find
   people" button, which is what exercises it now.

**Consequences.**

- File search is available on the Files page (a non-empty query switches to
  global results, reusing the `FileEntry` row + Download action; the download
  resolves the row from the in-memory results, so a chat-less hit still works).
- Both searches are fully testable offline: the drive and `/users` request
  shapes (path, `$top`, the `ConsistencyLevel: eventual` header, the Graph
  bearer) and the parsing are pinned by local TCP stubs.
- **The live acceptance is unverified** (U-010): the real response shapes and
  whether the Teams first-party Graph token carries the delegated
  `Files.Read`/`User.ReadBasic.All` scopes for these routes are not proven here;
  if a route 403s, the wrapper surfaces the vendored error unchanged.
- No schema change and no encryption change: directory people are never
  persisted; a search hit is in-process only (B-007 still covers the files cache).
- B2a's dependency is a **one-accessor contract**, so the picker can be reused
  (e.g. an "add member" flow) without touching `squads-core`.

---

## ADR-038 — Chat creation/leave via the chat service, the server-vs-local mute reconciliation, and schema v10

**Status:** accepted (B2a)

**Context.** Backlog item **B2a** adds chat management: start a 1:1 or group
chat, leave one, write the per-user **server** mute/hide state, and read a chat's
roster. Three questions had to be settled: **where creation/leave write**, how
the new **server mute/hide** relates to the M8 **local** mute, and where the
state lives.

Teams splits these writes and the native surfaces differ from the obvious Graph
ones:

- **Creation.** Graph `POST /me/chats` answers **405** (not a create endpoint)
  and Graph `POST /chats` needs `Chat.Create`, which the Teams first-party token
  lacks (**403** live — the vendored crate documents both). The vendored
  `create_one_to_one_chat_data` / `create_group_chat_data` therefore ride the
  **chat service**: a 1:1 is the derived `19:<lo>_<hi>@unq.gbl.spaces` thread
  (OstMac §106), probed and created with `uniquerosterthread`/`fixedRoster`
  properties; a group is a `POST /v1/threads` whose id comes from the response
  `Location`.
- **Leave.** The vendored `leave_chat_with_client` deletes the signed-in user's
  own MRI from the thread roster (`DELETE /v1/threads/{id}/members/{mri}`).
- **Mute/hide.** Mute is the per-user conversation property `alerts`
  (`"false"` = muted); hide is the `unpinnedTime` (now in ms) +
  `historyHiddenTime` pair, with unhide writing a `null` `unpinnedTime`. Graph
  `hideForUser`/`unhideForUser` need `Chat.ReadWrite`, which the token lacks.
- **Local mute already exists.** M8 persists `chats.muted` and
  [`route`](../crates/squads-core/src/notifications.rs) honours it — it is the
  **notification-routing source of truth** (ADR-021), and B2a must not break it.

**Decision.**

1. **Wrap the vendored chat-service writes.** `ApiClient` gains
   `create_one_to_one_chat`, `create_group_chat`, `leave_chat`,
   `set_chat_muted`, `set_chat_hidden` and `chat_members` (over
   `create_one_to_one_chat_data` / `create_group_chat_data` /
   `leave_chat_with_client` / `set_chat_muted_with_client` /
   `set_chat_hidden_with_client` / `list_chat_members_data`). The pure
   write-shape helpers are re-exported so the paths/bodies are pinned offline.
2. **A dedicated `Chats` service**, not an extension of `Messaging`. Creation and
   leave mutate the **chat list** (upsert/delete), which is not `Messaging`'s
   job; `Chats { profile, Arc<Store>, Arc<ApiClient> }` mirrors the
   `People`/`Files` shape and keeps the store effect in one place. `new_one_to_one`
   / `new_group` map the created [`ChatInfo`] into the owned `Chat` and upsert
   it; `leave` calls the server and then `delete_chat` (dropping its messages);
   `members` maps the roster into a new [`ChatMember`] model.
3. **A `ChatMember` model, not `Person`.** A roster entry is keyed by the
   chat-service **MRI** (`8:orgid:<guid>` — the leave target and the message
   `from` identity), carries **roles**/owner, and often has no name; the
   directory `Person` (Graph user id, email, job title) is a different thing.
   `ChatMember` gets its own type with a documented reason.
4. **The server-vs-local mute reconciliation.** `Chats::set_muted` writes the
   **local** flag first (`chats.muted`, immediate, offline — routing stays
   correct) and then attempts the **server** `alerts` write. On success it records
   the server state in a new `chats.server_muted` column; on failure it logs and
   **keeps the local flag** (the server write is best-effort). `set_hidden`
   mirrors this with `chats.hidden`. Both return `Ok(())` once the local state is
   written, so an **offline toggle still takes effect locally** — the documented
   trade-off. The app writes the local flag synchronously (the routing truth) and
   runs the server write on the backend, publishing `AppEvent::ChatFlagsChanged`.
5. **Schema v10.** `chats.hidden` (`INTEGER NOT NULL DEFAULT 0`) and
   `chats.server_muted` (**nullable** `INTEGER`; `NULL` = unknown, distinct from
   a confirmed `0`) via guarded `ALTER TABLE`s. `SCHEMA_VERSION = 10`;
   forward-only, re-entrant, a newer `user_version` is still a hard error.
6. **The app's entry points.** A **New chat** button opens B3's people picker in
   "new chat" mode (one person → 1:1; several → a topic prompt + group); a chat
   **context menu** ("...") offers Mute/Unmute, Hide and Leave (Leave confirms
   first). Muted rows badge; hidden rows collapse into a "Show hidden (N)"
   section. B3's Search-page "Find people" entry point is **kept** (browse); the
   picker now has two callers, which is exactly the contract ADR-037 designed.

**Consequences.**

- Creating/leaving/roster-writing is fully offline-testable: the exact
  method/path/auth/body of every shape is pinned by local TCP stubs (the 1:1
  resolve+probe(+create), the group `POST /v1/threads` + `Location`, the leave
  `DELETE .../members/{mri}`, the `alerts` and `unpinnedTime`/`historyHiddenTime`
  PUTs, the roster `GET`), and the store round-trips plus the **v9→v10**
  migration are tested.
- **M8 routing is unchanged**: `chats.muted` is still the only flag `route`
  reads; the server state is a separate, best-effort mirror. A mute set in
  another client is still not observed here (the vendored chat list's
  `properties.alerts` is not yet read back into `chats.muted`) — a documented
  gap.
- **The live acceptance is unverified** (U-011): whether the live chat service
  accepts the 1:1/group create bodies, the leave DELETE, the mute/hide property
  writes and the roster shape is assumed from the vendored crate + stubs; a
  protocol change upstream is inherited because nothing is reimplemented.
- No encryption change: the two flags join the chat row in the plaintext store;
  the roster is in-process only (B-007 still applies).
- B2b (folders, pinned messages) is a separate, additive change to the same
  `api`/`store`/app surfaces.

---

## ADR-039 — Folders & pins: `chats.folder_id` + a pin-membership table (schema v11), and the Graph-vs-thread pin sources

**Status:** accepted (B2b)

**Context.** Backlog item **B2b** adds the last chat-management surface: **chat
folders** (read the folder list, file a chat into one, and filter the chat list
by folder) and **pinned messages** (list a chat's server-side pins, jump to one,
unpin). Four questions had to be settled: where the folder read/move write, how
folder membership is modelled, where pins come from, and where the state lives.

The vendored surfaces:

- **Folders.** `conversation_folders_data(bearer)` GETs a **fixed CSA URL**
  (`https://teams.microsoft.com/api/csa/api/v1/teams/users/me/conversationFolders?…`)
  through the process-wide `shared_http()` client — it does **not** take the
  `TeamsClient`, so it is **not redirectable** to a local stub (unlike every
  other lane). `conversation_folder_move_with_client(client, bearer, chat, target)`
  does a hierarchy GET, one POST of the pure `folder_move_actions`/
  `folder_move_body` edits, then a re-read. The parser **drops** the
  server-generated views (`SYSTEM_FOLDER_TYPES`), so what comes back is
  Favorites + user folders — the folders a chat can be moved into.
- **Pins.** `chat_pinned_messages_data(client, chat)` reads the **chat-service**
  thread properties (`GET /v1/threads/{id}?view=msnp24Equivalent`, skypetoken)
  and parses tolerantly (`parse_thread_pins`), filling a bare pin's
  sender/preview/time from its per-message GET. The Graph
  `pinnedMessages?$expand=message` fallback needs `Chat.Read`, which the Teams
  token lacks (expected 403 live), so `pick_pins` prefers the chat-service pins.
  **Unpinning** is Graph `DELETE /chats/{id}/pinnedMessages/{pinId}`, which needs
  `ChatMessage.ReadWrite`/`Chat.ReadWrite` — also not granted — so
  `chat_unpin_message_with_client` **refuses before any network**.

**Decision.**

1. **Wrappers.** `ApiClient` gains `conversation_folders()`,
   `move_chat_to_folder(chat_id, target)`, `pinned_messages(chat_id)` and
   `unpin_message(chat_id, message_id)`, in the existing instrumented (redacting
   `Debug`, request counter, `tracing` spans) style. The folder lane needs the
   bearer the vendored call takes as a parameter, so `ApiClient` now **retains
   the skype token** it was built with (the same host/token pair `csa_get`
   uses for the chat list) and refuses with `NotSignedIn` when there is none.
   The B2b pure helpers/types are re-exported.
2. **Models.** `model::ChatFolder` (id/name/folder_type, `is_system` computed
   from `SYSTEM_FOLDER_TYPES`, `item_ids`) and `model::PinnedMessage`
   (chat_id/message_id/sender/preview/time/pinned_at/by/graph_pin_id, plus an
   `Option<Message>` resolved from the store on read — **not** persisted).
3. **Services.** A small `Folders { profile, Arc<Store>, Arc<ApiClient> }` and
   `Pins { … }`, mirroring `Chats`/`People`. `Folders::folders` fetches + caches;
   `cached_folders` is local-only; `move_to_folder` is **local-first** (the
   membership is written immediately so the tabs update offline; the server move
   is best-effort and re-caches the server's state on success — the folder lane
   cannot be stubbed and, offline, always fails, exactly like B2a's server mute).
   `Pins::pinned` fetches + caches + resolves messages; `cached_pinned` is
   local-only; `unpin` drops the local pin and treats the (always-refusing)
   server call as best-effort. `Session::folders()` / `Session::pins()`.
4. **Store (schema v11).** `chat_folders(id, profile, name, folder_type,
   is_system, position)` holds the folder list; **membership lives on a nullable
   `chats.folder_id`** rather than a join table, because a chat is in **at most
   one** movable folder (`folder_move_actions` removes it from every other one
   when it adds it to the target) — a join table would model an impossible
   state. `Store::chat_folders` rebuilds each folder's `item_ids` from that
   column, so the two can never disagree. `pinned_messages(chat_id, message_id,
   profile)` holds the pin **membership metadata** only; the body is resolved
   from `messages`. The migration is `CREATE TABLE/INDEX IF NOT EXISTS` + a
   guarded `ALTER TABLE chats ADD COLUMN folder_id`; `SCHEMA_VERSION = 11`,
   forward-only, re-entrant, a newer `user_version` still a hard error.
   `chats.muted`/`server_muted`/`hidden` are untouched.
5. **App.** The chat list gets **folder tabs** ("All" + one per folder, with
   counts) filtering the list, and a **Move to folder** row in the B2a context
   menu; the conversation info pane gets a **pinned-messages panel** (sender,
   preview, time) whose **Go** reuses B1's jump/highlight and whose **Unpin**
   removes the pin. All of it rides the existing `AppRuntime`/`AppEvent` bridge
   and repaints only when something changed (ADR-027).

**Consequences.**

- The pin lane is fully offline-testable (the chat-service thread GET is
  stubbable): the request line, the tolerant parse of both the thread and Graph
  shapes, the fill-from-message pass, the store round-trip and unpin are pinned
  by tests.
- **The folder lane cannot be stubbed offline** (the vendored GET/POST use a
  fixed CSA URL and `shared_http()`); its pure shapes (`conversation_folders_url`,
  `folder_move_actions`, `folder_move_body`, `parse_conversation_folders`) are
  pinned, and the service's **local-first** behaviour is tested, but the live
  request is unverified — recorded as **U-012**.
- **Server unpin is unavailable with the Teams sign-in** (Graph `Chat.ReadWrite`
  not granted); `Pins::unpin` therefore removes the pin **locally** and logs the
  refused server call. A live tenant with a suitably-scoped token changes only
  that one branch.
- The Graph-vs-thread pin-source split is explicit: the chat-service thread
  properties are authoritative today; `pick_pins`/`PinSource` keep the Graph
  fallback ready for a token that can read it.
- No encryption change: folder names and pin previews join chat content in the
  plaintext store (B-007).

---

## ADR-040 — Public-team join + member admin (Graph), and the dedicated Graph channel-reaction lanes

**Status:** accepted (B4)

**Context.** Backlog item **B4** finishes the `api::teams` surface M6 left out:
**public-team discovery + join**, **member add/remove**, **team settings**, and
**channel message reactions** — the last of which M6 explicitly deferred
(ADR-018/U-003). Three things had to be settled: which host serves each call, how
the channel-reaction lane differs from the chat-reaction lane, and whether any
new state needs persisting.

The vendored surfaces:

- **Public-team search** is Graph and **two-lane**: `GET /groups` `$search`
  (word match, `ConsistencyLevel: eventual`, `Group.Read.All`-class) with a
  fallback to `GET /teams` `startswith(displayName)` (`Team.ReadBasic.All`) when
  `/groups` is refused. Both are **single-window** (no cursor; capped at
  `PUBLIC_TEAMS_MAX = 25`), and the parser keeps only `visibility == public`
  rows. **Join** is Graph `POST /teams/{id}/members` for self (the own user id
  resolved through `/me`); join-by-invite-code is not Graph and is out of scope.
- **Member admin** is Graph: `POST /teams/{id}/members` (owner role optional)
  and `DELETE /teams/{team}/members/{membership}` (the **membership** id, not the
  user id). **Team settings** is two Graph GETs (`GET /teams/{id}` for
  `memberSettings`, `GET /teams/{id}/primaryChannel` for the General channel),
  each independently failing to `None`.
- **Channel reactions** are the problem child. The **chat-service** reaction
  endpoint (`/messages/{id}/reactions`) **404s** on channel threads, so the
  channel counterpart is the **Graph** `POST /teams/{team}/channels/{channel}/
  messages/{id}/(set|unset)Reaction` lane (`reactionType` sent as the picker
  unicode), with a `…/replies/{reply}/(set|unset)Reaction` variant for thread
  replies. The vendored crate flags this lane **NOT verified live** (auth was
  down when it landed).

**Decision.**

1. **Wrap the vendored calls, Graph throughout.** `ApiClient` gains
   `search_public_teams`, `join_team`, `add_team_member`, `remove_team_member`,
   `team_settings`, `set_channel_reaction` and `unset_channel_reaction`, in the
   existing instrumented (redacting `Debug`, request counter, `tracing` spans)
   style, and re-exports the B4 pure helpers/types so the request/response shapes
   are pinned offline.
2. **Owned models, additive.** `model::PublicTeam` mirrors `PublicTeamInfo` plus
   a **local** `is_joined` (the wire carries no membership; `From` defaults it
   `false` and the service marks it against the joined cache).
   `model::TeamSettings` mirrors the two flags + the General channel id, each
   `None` when unknown. `TeamMember` gains a **non-persisted** `user_id` (the
   vendored projection carries it) so the app can recognise the signed-in owner;
   the store does not persist it. `model::Reaction` is **reused unchanged** —
   channel reactions adjust the same stored representation as chat reactions.
3. **The channel-reaction lane needs the team id.** The Graph path is keyed by
   `(team, channel, message, reply)`, but a conversation id does not name its
   team. `Teams::react_in_channel(conversation_id, message_id, emoji, remove,
   is_reply)` resolves the team from the **cached channel**
   (`Store::team_for_channel`, a schema-free reverse lookup over the existing
   `channels` table) and derives the **root** for a reply from the derived thread
   conversation `<channel>;messageid=<root>` (falling back to the stored
   `reply_to`); `split_thread_conversation` is the inverse of the existing
   `thread_conversation`. The call then adjusts the stored message's reactions
   (the same coarse ±1 as `Messaging::react`; canonical counts arrive with the
   next history refresh).
4. **No new persisted state.** Public-team search results are transient (like
   the B3 people search); `TeamSettings` is cheap to re-read and per-team, so it
   is **not cached**; the roster already lives in `team_members`. The only store
   additions are two schema-free helpers (`team_for_channel`,
   `delete_team_member`), so **there is no schema v12**.
5. **The app.** A **Browse public teams** search on the Teams page
   (`Action::SearchPublicTeams` → `AppEvent::PublicTeamsLoaded`, a Join button
   per hit → `Action::JoinTeam`), an **owner-gated member roster**
   (`is_owner_of` = a fetched member whose `user_id` matches our oid carries the
   `owner` role; **Add member** opens B3's people picker in "add member" mode
   with an owner-role toggle; **Remove** confirms first), and a **channel
   reaction affordance** in the conversation view (`Action::ReactInChannel` →
   `AppEvent::ReactionsChanged`). All ride the existing `AppRuntime`/`AppEvent`
   bridge and repaint only when something changed (ADR-027).

**Consequences.**

- Every request shape is pinned **offline** against a local TCP stub (the
  `/groups`/`/teams` search paths, the join/member-add bodies, the member
  `DELETE`, the two settings GETs, and the channel `setReaction`/`unsetReaction`
  paths for the root **and** the reply variant, with the Graph bearer), and the
  service effects (membership marking, roster write-through, the store reaction
  update) are covered.
- **The channel-reaction lane is now wired but not live-verified** (U-003
  updated): the vendored crate's own note that the unicode-vs-named
  `reactionType`, the scope grant and the Graph-vs-chat-service message id
  mapping need a signed-in probe still stands. The **public-team search/join,
  member admin and team-settings** shapes are likewise unproven live.
- Channel **read receipts** stay on hold (Graph exposes no channel receipt API
  and the chat-service consumption horizon 403s on `tacv2` threads); chat
  receipts keep the M5 path.
- `create_channel` still rides Graph (a vendor-vs-brief discrepancy noted in
  U-003); routing it through the middle tier remains a contained change in
  `ApiClient::create_channel`.
- No encryption change: public-team names/descriptions, member names/emails and
  channel reaction state join chat content in the plaintext store (B-007).

---

## ADR-041 — Recordings & transcripts service, a schema-v12 `(id, profile)` cache, and reuse of the M7 download path

**Status:** accepted (B5)

**Context.** Backlog item **B5** exposes the two lanes M9's call layer left
unused — `api::recordings` and `api::transcripts` — so meeting recordings and
transcripts can be **listed and opened**. Both are plain Graph **driveItems**: a
non-channel meeting's recording/transcript lives in the signed-in user's OneDrive
`Recordings` folder and a channel meeting's in the channel's SharePoint
`filesFolder/Recordings`. Three things had to be settled: how the vendored
(global) lane maps onto a conversation, where the rows are cached, and how the
bytes are transferred.

The vendored surfaces:

- `list_recordings_data(client, limit)` / `list_transcripts_data(client, limit)`
  are **global**: one call fans out over OneDrive **and** every channel drive (a
  bounded `list_teams_data` scan plus per-channel `filesFolder`/children
  lookups), merges the results and sorts them newest-first. There is **no**
  per-chat/per-channel variant and **no** paging cursor; a limit is clamped by
  the pure path builder (`RECORDINGS_MAX_LIMIT = 50`, `TRANSCRIPTS_MAX_LIMIT =
  100`). The transcripts lane additionally falls back to a bounded full-drive
  search when the folder scan finds zero rows.
- The wire projection (`RecordingInfo` / `TranscriptInfo`) carries
  `id`/`name`/`size`/`mime`/`web_url`/`drive_id`/`created`/`modified` and a
  **source** (`OneDrive` or `Channel { team, channel }`, named by **display
  name**). It carries **no** chat/meeting association and **no** team/channel
  id; a recording carries a `video` facet (duration) and a pre-authenticated
  `download_url`, a transcript carries neither.
- Downloads reuse the M7 files stack: `/drives/{d}/items/{i}/content` (a
  transcript's only path; a recording may instead stream its `download_url`).

**Decision.**

1. **Wrap the vendored calls, global.** `ApiClient` gains `list_recordings`,
   `search_recordings`, `list_transcripts` and `search_transcripts`, in the
   existing instrumented (redacting `Debug`, request counter, `tracing` spans)
   style, and re-exports the B5 pure helpers/types. The wrappers do **not**
   invent a source parameter the vendored functions do not take.
2. **Owned models.** `model::Recording` and `model::Transcript` mirror the wire
   projections (with an owned `RecordingSource` — `TranscriptSource` is the same
   shape under its own name — carrying `is_channel`/`label`/`team_name`/
   `channel_name`). `Recording::is_video` is computed with the vendored
   predicate (always `true` from the wire: the parser drops non-videos).
   `as_file_entry()` projects either type onto a `FileEntry` so the M7 download
   path can transfer it unchanged. The absent chat/meeting association and the
   name-only channel identity are **documented on the types** rather than
   guessed.
3. **A `Recordings` service that slices the global lane by source.**
   `Recordings { profile, Arc<Store>, Arc<ApiClient>, Files }`:
   - `recordings_for_chat` keeps the **OneDrive** rows (a chat/meeting recording
     lives in the organizer's OneDrive) and caches them under the chat id;
   - `recordings_for_channel` keeps the **channel** rows, matched to the
     requested channel by its **cached display name** (the wire names no id;
     when the name cannot be resolved every channel row is kept — documented),
     and caches them under the channel id;
   - `search_recordings`/`search_transcripts` are the global drive-search (all
     sources), cached chatlessly;
   - `transcripts_for_chat`/`transcripts_for_channel` mirror the recording split;
   - the local reads `cached_recordings`/`cached_transcripts`.
   `Session::recordings()` mirrors the other services.
4. **Store (schema v12).** `recordings` and `transcripts` are keyed
   **`(id, profile)`** — the driveItem id matches the store's convention and the
   M7 **ADR-019** rationale: `drive_id` is optional and SQLite treats `NULL`
   keys as distinct, so keying on it would break the `Option` round-trip. A
   nullable non-key `scope` holds the owning chat/channel id (a global listing
   leaves it `NULL`), mirroring `files.chat_id`; the source is persisted as
   `source_kind`/`source_team`/`source_channel`. Upserts are idempotent and
   **additive** (`COALESCE` on the optional columns), so a later global listing
   cannot wipe a conversation's scope or a cached drive id/URL. The migration is
   `CREATE TABLE/INDEX IF NOT EXISTS`; `SCHEMA_VERSION = 12`, forward-only,
   re-entrant, a newer `user_version` still a hard error.
5. **Download reuses the M7 path.** `Recordings::download` /
   `download_transcript` project the entry onto a `FileEntry` and call
   [`Files::download`](../crates/squads-core/src/files.rs), so the buffered
   transfer and the coarse `(0, size)`/`(written, size)` progress reporting are
   exactly the M7 ones (ADR-020); a drive-less item is rejected before any
   network with the same guard.
6. **The app.** A **Recordings & transcripts** panel in the conversation info
   pane: each row shows its name, source label, size, modified date and
   (recordings) duration, with a **Download** action and a **Refresh
   recordings** affordance that resolves the chat-vs-channel slice (a channel
   resolves its team from the cached channel). All of it rides the existing
   `AppRuntime`/`AppEvent` bridge (`AppEvent::RecordingsLoaded`) and repaints
   only when something changed (ADR-027).

**Consequences.**

- The lane is fully offline-testable: the OneDrive `Recordings` children path,
  the `joinedTeams` scan, the channel `filesFolder` and folder-children paths,
  the drive-search, the source split, the store round-trip and the **v11→v12**
  migration are all pinned by stubs/tests.
- **The vendored lane is global and unscopable**, so `recordings_for_channel`
  matches by cached display name and falls back to every channel row when the
  name is unknown; a chat listing still performs the full channel fan-out
  (there is no OneDrive-only vendored function). Both are documented on the
  service, not hidden.
- **Live acceptance is unverified** (U-013): whether the Teams first-party Graph
  token carries the delegated `Files.Read` scope for these reads, whether a real
  `Recordings` folder/`video` facet/`.vtt` item matches the projection, and the
  real fan-out cost are assumed from the vendored crate + stubs. A 403 surfaces
  as the vendored error unchanged.
- No encryption change: recording/transcript names and URLs join chat content in
  the plaintext store (B-007).
