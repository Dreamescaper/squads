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

**Status:** proposed (M3)

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

