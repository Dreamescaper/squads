# Project status — one screen

> Snapshot for the **M13 (Self identity + inline media)** milestone plus backlog
> items **B1 (Message search)**, **B3 (File & people search)**, **B2a (New
> chat, leave, mute/hide)**, **B2b (Folders and pinned messages)**, **B4
> (Team administration)** and **B5 (Recordings & transcripts)**. The
> detailed per-milestone history is in [`PROGRESS.md`](PROGRESS.md); constraints
> are in [`BLOCKERS.md`](BLOCKERS.md).

## Where the project is

All fourteen planned milestones (**M0–M13**) are implemented, plus backlog items
**B1 (message search)**, **B3 (file & people search)**, **B2a (new chat, leave,
mute/hide)**, **B2b (folders and pinned messages)**, **B4 (team
administration)** and **B5 (recordings & transcripts)** — the backlog is
complete apart from **B6**, an explicit *not planned* list. The workspace builds
device- and network-free with default features, and the test suite passes
offline:

| Crate | Role | Tests |
|---|---|---|
| `teamtrim-protocol` (vendored `ost`, lib `ost`) | protocol core (auth, api, trouter, calling) | 377 (unchanged) |
| `teamtrim-core` | session, store, settings, realtime, services | 442 |
| `teamtrim-app` (`teamtrim` binary) | egui UI | 40 |

`cargo build --workspace` and `cargo build --workspace --release` both succeed.

**Release artifacts:** [`.github/workflows/release.yml`](../.github/workflows/release.yml)
builds a per-platform archive on native runners — Linux `.tar.gz`, macOS `.zip`
(`TeamTrim.app`, ad-hoc signed, Intel + Apple Silicon) and Windows `.zip` — and
attaches them to a GitHub Release on `v*` tags. Installers (AppImage/Flatpak/MSI)
and real signing/notarization are still manual (B-013, `PACKAGING.md` §6).

**What works (verified here, offline):** workspace + CI config; the full auth
token model and device-code flow; the session/client facade and region routing;
the SQLite store with migrations v1→v12; realtime decode/fan-out/write-through;
messaging (optimistic sends, paging, edit/delete/react/receipts); teams &
channels; files (Graph list/upload/download/links/versions); presence and
notification routing; call signalling/media primitives/loopback/call log; the
calendar agenda/week view, scheduling/cancelling and the join hand-off; the
signed-in identity (whoami → `mark_own` on server history and realtime) and the
inline-media cache with timeline image rendering; **local-first message search**
(schema-v9 index, `Search` service, the **Search** page and jump-to-message —
B1); **Graph file & people search** (single-window drive search on the Files
page, a `People` service, and a reusable multi-select **people picker** —
B3); **chat management** (new 1:1/group chat via the picker, leave with
confirmation, server mute/hide reconciled with the local routing flag, a chat
context menu and muted/hidden rendering — B2a); **folders & pins** (folder tabs
with counts filtering the chat list, a move-to-folder action, and a
pinned-messages panel with jump-to-message and unpin, schema v11 — B2b); **team
administration** (a public-team search + join, an owner-gated member roster with
add-via-picker and remove-with-confirmation, team settings, and a channel
reaction affordance routed through the dedicated Graph lane — B4); **recordings
& transcripts** (a global OneDrive/SharePoint recording/transcript list + search
with a chat-vs-channel source split, a `Recordings` service that reuses the M7
download path, and a **Recordings & transcripts** info-pane panel, schema v12 —
B5); the egui shell and every screen; M11 keyboard shortcuts, expanded settings,
light theme + accents, and headless UI tests.

**What is unverified (needs a live tenant / devices / a display):** every
network write shape (U-002…U-006), the calendar's live Graph acceptance and the
browser-only join-by-id fallback (U-007), the live whoami and media-host
acceptance (U-008), the optional Graph message-search fallback (expected to 403
— U-009), the live Graph drive/people search (U-010), the live chat-service
create/leave/mute/hide/roster acceptance (U-011), the live folder read/move and
pin read (the folder lane cannot be stubbed offline; server unpin is
unavailable — U-012), the live public-team search/join, member admin,
team-settings and channel-reaction acceptance (U-003), the live
recordings/transcripts Graph acceptance, the global channel fan-out and the
name-only channel match (U-013), the unified presence token
audience (B-009), desktop notifications and quiet hours (B-010), real call media
(B-011), and the UI's actual pixels (B-012). See [`BLOCKERS.md`](BLOCKERS.md).

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
- **ADR-032** — Cross-platform release artifacts on native runners.
- **ADR-033** — Calendar service, schema-v7 `meetings`, and the join hand-off to calls.
- **ADR-034** — Self identity: whoami → cached identity, schema-v8 `accounts`, and where `mark_own` is applied.
- **ADR-035** — Inline media: `text::image_urls` + a deterministic `MediaCache` + `egui_extras` image loading.
- **ADR-036** — Message search is local-first (schema-v9 index), with the Graph server path as an optional, expected-to-403 fallback.
- **ADR-037** — File/people search wrappers, a schema-free `People` service, and the people-picker contract B2a consumes.
- **ADR-038** — Chat creation/leave via the chat service, the server-vs-local mute reconciliation, and schema v10.
- **ADR-039** — Folders & pins: `chats.folder_id` + a pin-membership table (schema v11), and the Graph-vs-thread pin sources.
- **ADR-040** — Public-team join + member admin (Graph), and the dedicated Graph channel-reaction lanes.
- **ADR-041** — Recordings & transcripts service, a schema-v12 `(id, profile)` cache, and reuse of the M7 download path.

## Open blockers (summary)

Standing environment limits: **B-001** (headless — no display), **B-002** (no
live tenant), **B-003** (volatile reverse-engineered protocol).

Open / deferred work: **B-004** (PKCE loopback), **B-005** (Skype exchange not
stubbable), **B-006** (mate-name path untested offline), **B-007** (store and
media cache unencrypted), **B-008** (call frames filtered from the hub; global
hub; reconnect token staleness), **B-009** (unified presence token unverified),
**B-010** (desktop notifications / quiet hours), **B-011** (live calls + real
media), **B-012** (UI verified by wiring, not pixels; attachment picker stub),
and unverified aspects **U-001…U-013** (U-003: live public-team search/join,
member admin, team-settings and channel-reaction acceptance; U-007: live calendar
acceptance and the browser-only join-by-id fallback; U-008: live whoami and
media-host acceptance; U-009: the optional Graph message-search fallback,
expected to 403; U-010: live Graph drive/people search acceptance and the
delegated scopes; U-011: live chat-service create/leave/mute/hide/roster
acceptance; U-012: the live folder read/move and pin read, with the folder lane
unstubbable offline and server unpin unavailable; U-013: live
recordings/transcripts acceptance, the global channel fan-out and the name-only
channel match).

## How to verify by hand

### Offline (runs anywhere)

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --workspace                 # default features, no device/network
cargo test -p teamtrim-core               # 442
cargo test -p teamtrim-app                # 40 (headless egui_kittest)
cargo test -p teamtrim-protocol           # 377
cargo fmt -p teamtrim-core -p teamtrim-app -- --check
cargo clippy -p teamtrim-core -p teamtrim-app --all-targets
cargo build --workspace --release
```

### With a display (manual UI QA)

1. `cargo run -p teamtrim-app` (binary `teamtrim`); the login screen appears.
2. Sign in (device code) and confirm the shell, chat list and timeline render.
3. Exercise the M11 shortcuts: `Ctrl/Cmd+K` (quick switcher), `Ctrl/Cmd+F`
   (in-conversation search), `Ctrl/Cmd+Shift+F` (search all messages — B1),
   `Esc` (close/clear), `Ctrl/Cmd+,` (settings),
   `Ctrl/Cmd+Shift+M` (mute placeholder), `Ctrl/Cmd+1`…`5` (jump to Chats /
   Calendar / Teams / Files / Calls), `j`/`k` (chat navigation),
   `Ctrl/Cmd+Enter` (send).
4. In **Settings**, switch the theme to Light and pick an accent; toggle compact
   mode and the notification switches; set a download folder. Restart and confirm
   the choices persist (`~/.config/teamtrim/settings.json`).
5. On **Files**, type a filename into the search box and confirm the global
   OneDrive results render and download; clear it to return to the chat's files.
   On **Search**, press **Find people**, search a colleague's name and select one
   or several (B3).
6. Press **New chat**, search a colleague and select one (a 1:1 opens) or two or
   more (type a **Group topic** and Create). Open a chat's **"..."** menu and
   **Mute** it (a badge appears), **Hide** it (it collapses into the "Show
   hidden" section and can be unhidden), and **Leave** it (confirm) — B2a.
7. Press **Refresh folders** and confirm the folder tabs appear with counts;
   pick a folder and confirm the chat list filters; move a chat into a folder
   from its **"..."** menu (the count changes). Open a chat's **Info** pane and
   confirm the **pinned messages** list; **Go** jumps to and highlights the
   message, **Unpin** removes it — B2b.
8. On **Teams**, type a team name into **Browse public teams** and press **Join**
   on a result; expand a team you own to see its **Members** and add/remove a
   member (add opens the people picker). Open a channel and react to a message
   (and a thread reply) — B4.
9. Open a chat you recorded a meeting in, open its **Info** pane and confirm the
   **Recordings & transcripts** list (name/source/date/size/duration); press
   **Download** on a row and confirm the file lands in the download directory.
   On a channel, press **Refresh recordings** — B5.
10. Follow the per-milestone "How to verify manually" steps in
    [`PROGRESS.md`](PROGRESS.md) for the network features (needs a real tenant).

### Release/packaging

See [`PACKAGING.md`](PACKAGING.md) for the Linux install, macOS `.app` layout and
Windows notes.
