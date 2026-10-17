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




