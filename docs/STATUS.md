# Project status — one screen

> Snapshot for the **M11 (Polish & packaging)** milestone. The detailed
> per-milestone history is in [`PROGRESS.md`](PROGRESS.md); constraints are in
> [`BLOCKERS.md`](BLOCKERS.md).

## Where the project is

All twelve planned milestones (**M0–M11**) are implemented. The workspace
builds device- and network-free with default features, and the test suite passes
offline:

| Crate | Role | Tests |
|---|---|---|
| `squads-protocol` (vendored `ost`, lib `ost`) | protocol core (auth, api, trouter, calling) | 377 (unchanged) |
| `squads-core` | session, store, settings, realtime, services | 285 |
| `squads-app` (`squads` binary) | egui UI | 18 |

`cargo build --workspace` and `cargo build --workspace --release` both succeed.

**What works (verified here, offline):** workspace + CI config; the full auth
token model and device-code flow; the session/client facade and region routing;
the SQLite store with migrations v1→v6; realtime decode/fan-out/write-through;
messaging (optimistic sends, paging, edit/delete/react/receipts); teams &
channels; files (Graph list/upload/download/links/versions); presence and
notification routing; call signalling/media primitives/loopback/call log; the
egui shell and every screen; M11 keyboard shortcuts, expanded settings, light
theme + accents, and headless UI tests.

**What is unverified (needs a live tenant / devices / a display):** every
network write shape (U-002…U-006), the unified presence token audience (B-009),
desktop notifications and quiet hours (B-010), real call media (B-011), and the
UI's actual pixels (B-012). See [`BLOCKERS.md`](BLOCKERS.md).

**Deferred:** encryption at rest (B-007), the realtime call-frame producer
(B-008), the attachment picker UI (B-012), the browser PKCE loopback listener
(B-004), and hardware video (D-002).

## Architecture decisions (ADRs)

Newest last; full text in [`DECISIONS.md`](DECISIONS.md).

- ADR-001 — Reuse the MIT `ost` core rather than reimplementing.
- ADR-002 — Three-crate Cargo workspace.
- ADR-003 — GPUI for the UI *(superseded by ADR-006)*.
- ADR-004 — Dedicated tokio runtime bridged to the UI by channels.
- ADR-005 — Local-first SQLite store.
- ADR-006 — egui/eframe over GPUI (footprint priority).
- ADR-007 — Async `AuthTransport` trait for offline auth tests.
- ADR-008 — File token store by default; OS keyring behind a feature.
- ADR-009 — Device-code first; browser PKCE helpers only.
- ADR-010 — Vendored `ost` embedding patch (`from_config`, runtime base URLs).
- ADR-011 — Plain bundled SQLite, not SQLCipher (for now).
- ADR-012 — `Store` is `Send + Sync` via `Mutex<Connection>`.
- ADR-013 — Wrap the vendored Trouter loop; poll `event_hub` into a typed bus.
- ADR-014 — Optimistic sends with `SendState` and schema-v2 `send_state`.
- ADR-015 — One shared text/HTML module; outgoing HTML stays vendored.
- ADR-016 — Backward-paging cursor lives in memory, per `Messaging` handle.
- ADR-017 — Teams/channel metadata in a schema-v3 side cache.
- ADR-018 — Channel routing: chat service / Graph / middle tier.
- ADR-019 — Files metadata in a schema-v4 cache keyed `(id, profile)`.
- ADR-020 — Upload by size (≤4 MiB simple, >4 MiB resumable); coarse download.
- ADR-021 — Two presence backends, unified by default; unverified audience.
- ADR-022 — `Notifier` trait; desktop notifications behind a default-off feature.
- ADR-023 — Call signalling wraps epconv with an injectable client/endpoints.
- ADR-024 — Media behind default-off features; offline loopback backs the tests.
- ADR-025 — Local call history in a schema-v6 `call_log` table.
- ADR-026 — App architecture: runtime bridge + UI → Action → apply.
- ADR-027 — `request_repaint` discipline: repaint only on an applied event.
- ADR-028 — Headless UI testing with `egui_kittest` + `accesskit`.
- **ADR-029** — Keyboard shortcuts: a pure mapping, app-owned dispatch.
- **ADR-030** — Settings forward-compatibility: container-level serde defaults.
- **ADR-031** — CI scope (our crates only) and the release profile.

## Open blockers (summary)

Standing environment limits: **B-001** (headless — no display), **B-002** (no
live tenant), **B-003** (volatile reverse-engineered protocol).

Open / deferred work: **B-004** (PKCE loopback), **B-005** (Skype exchange not
stubbable), **B-006** (mate-name path untested offline), **B-007** (store is
unencrypted), **B-008** (call frames filtered from the hub; global hub;
reconnect token staleness), **B-009** (unified presence token unverified),
**B-010** (desktop notifications / quiet hours), **B-011** (live calls + real
media), **B-012** (UI verified by wiring, not pixels; attachment picker stub),
and unverified aspects **U-001…U-006**.

## How to verify by hand

### Offline (runs anywhere)

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --workspace                 # default features, no device/network
cargo test -p squads-core               # 285
cargo test -p squads-app                # 18 (headless egui_kittest)
cargo test -p squads-protocol           # 377
cargo fmt -p squads-core -p squads-app -- --check
cargo clippy -p squads-core -p squads-app --all-targets
cargo build --workspace --release
```

### With a display (manual UI QA)

1. `cargo run -p squads-app` (binary `squads`); the login screen appears.
2. Sign in (device code) and confirm the shell, chat list and timeline render.
3. Exercise the M11 shortcuts: `Ctrl/Cmd+K` (quick switcher), `Ctrl/Cmd+F`
   (in-conversation search), `Esc` (close/clear), `Ctrl/Cmd+,` (settings),
   `Ctrl/Cmd+Shift+M` (mute placeholder), `j`/`k` (chat navigation),
   `Ctrl/Cmd+Enter` (send).
4. In **Settings**, switch the theme to Light and pick an accent; toggle compact
   mode and the notification switches; set a download folder. Restart and confirm
   the choices persist (`~/.config/squads/settings.json`).
5. Follow the per-milestone "How to verify manually" steps in
   [`PROGRESS.md`](PROGRESS.md) for the network features (needs a real tenant).

### Release/packaging

See [`PACKAGING.md`](PACKAGING.md) for the Linux install, macOS `.app` layout and
Windows notes.
