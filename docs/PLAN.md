# TeamTrim — High-Level Plan of Work

> An alternative, cross-platform Microsoft Teams client written in **Rust + egui**.

## 1. Product goal

Build a fast, native, cross-platform (Linux/macOS/Windows) Teams client:

- **Chat & messaging** — 1:1, group chats, channel conversations; send/edit/delete, replies, reactions, read receipts, typing.
- **Teams & channels** — browse teams/channels, threads, channel messages.
- **Files** — browse/upload/download files shared in chats and channels (OneDrive/SharePoint via Microsoft Graph).
- **Calls** — 1:1 and group audio/video calls, screen share, call recording/transcripts (as far as the protocol allows).
- **Presence, notifications, search.**
- **Native egui UI** — chat-rail + list + timeline + composer, theming, keyboard-driven.

Non-goals (for now): full Teams admin features, third-party app hosting (TeamsJS frames), mobile.

## 2. Reference architecture (what we learned)

| Source | What it gives us |
|---|---|
| [`eisbaw/ost`](https://github.com/eisbaw/ost) (MIT) | Reverse-engineered **Rust** client for the proprietary Teams protocol: OAuth, native chat service, Trouter realtime, calling. |
| [`mrowlinson/better-teams`](https://github.com/mrowlinson/better-teams) (MIT + MS exclusion) | Extends `ost` into a full protocol core (files, presence, teams/channels, CSA/middle-tier) and pairs it with a **macOS-only Swift UI**. Proves the "portable Rust core + native UI" split. |
| [`crmne/zapfast`](https://github.com/crmne/zapfast) | A Rust chat client (egui, not GPUI): useful **architecture** — UI→Action→apply, backend-per-account on its own tokio runtime, `Command`/`Event` channels, encrypted SQLite archive, virtualized message list. |

**Core insight:** Teams has no usable public API for chat/calls. Chat bypasses Graph
(the first-party token lacks `Chat.Read`) and uses the native `ng.msg` chat service with a
`skypetoken`; realtime is Microsoft's Trouter (socket.io v1 over WebSocket); calls use
epconv SDP signalling with ICE/TURN/SRTP/RTP. All of this is already solved in Rust by
`ost`/`better-teams`. The missing piece is a **cross-platform native UI** — which is what we build.

### Decision: reuse the Rust protocol core, build our own UI + session/orchestration layer

We vendor the `ost` protocol core (MIT, from the better-teams fork, which has files/presence/
teams patches) as an in-workspace crate, and build two new crates on top. See `docs/DECISIONS.md`.

```
teamtrim/
├── Cargo.toml                # workspace
├── crates/
│   ├── teamtrim-protocol/      # vendored `ost` core (auth, api, trouter, calling, models)
│   ├── teamtrim-core/          # our session layer: client lifecycle, event bus, store, settings
│   └── teamtrim-app/           # egui application binary
└── docs/                     # this plan + decisions/progress/blockers
```

## 3. Milestones

Each milestone is an independent work item for the implementation loop: **implement → review → commit → push**.

### M0 — Workspace & scaffolding
- Convert repo to a Cargo workspace; add `teamtrim-core` and `teamtrim-app` crates; vendor `teamtrim-protocol`.
- Error types, logging/tracing, config paths, `docs/` skeleton.
- CI task list (fmt, clippy, test, build) documented; `cargo check` green on Linux.
- **Exit:** workspace builds; egui window stub launches (headless CI cannot display it).

### M1 — Authentication
- Device-code (RFC 8628) flow **and** browser PKCE flow.
- Five-token model: AAD/Skype-Spaces → `authsvc` exchange (`skypetoken` + `regionGtms`) → Graph / IC3 / Recorder refresh grants.
- Secure token storage (OS keyring + 0600 file fallback), refresh-with-skew, multi-account.
- **Exit:** login state machine + token store are unit-tested; network calls are mockable.

### M2 — Protocol clients & session core
- Wrap chat service, Graph, presence, CSA and middle-tier clients from the protocol crate.
- `Session`/`Client` lifecycle, region routing from `regionGtms`, request instrumentation.
- **Exit:** a `teamtrim-core::Session` can list chats against a fake transport in tests.

### M3 — Local persistence & offline cache
- SQLite (rusqlite) schema for accounts, chats, messages, files, settings; migrations.
- Offline-first reads; write-through on realtime events.
- **Exit:** store round-trips chats/messages; schema migration test.

### M4 — Realtime (Trouter)
- Trouter socket.io v1 client (negotiate → session → WSS → registrar), `3:::` ack contract, `message_loss` resync.
- `EventBus` fan-out into `teamtrim-core` domain events (message, edit, typing, read-state, thread-update, call).
- **Exit:** frame decoder unit-tested; reconnect/backoff implemented.

### M5 — Messaging
- Chat list (3-strategy fallback), message history + backward paging.
- Send / edit / delete / reply; idempotent sends (`clientmessageid`); reactions; read receipts; typing.
- Message model + HTML/RichText handling.
- **Exit:** message encode/decode + send/optimistic-update logic tested.

### M6 — Teams & channels
- Joined teams, channels, channel threads, channel messages; team/channel writes via middle tier where Graph is denied.
- **Exit:** team/channel models + routing tested.

### M7 — Files
- Graph drive: resolve attachments from messages, list folder/recents/search, upload (simple ≤4 MiB + resumable >4 MiB), download with progress, share links, versions.
- **Exit:** upload session chunking + share-id encoding unit-tested.

### M8 — Presence & notifications
- Unified presence read/write; activity/mention feed; desktop notifications; unread badges.
- **Exit:** presence mapping tested; notification routing tested.

### M9 — Audio/video calls
- Signaling (epconv two-phase, invite/accept/end), ICE/STUN/TURN (FlightProxy relay), SRTP/SDES, RTP/RTCP.
- Audio capture/playback (cpal) and PCMU/PCMU+H.264; incoming-call handling; call recording/transcripts.
- **Exit:** SDP parse/generate and SRTP keying tested; media loopback test where possible.

### M10 — egui UI
- App shell: window, theming, layout (nav rail + chat list + timeline + composer + info panel).
- Virtualized message timeline, message bubbles, avatar/image async loading, composer with attachments.
- Login screen, teams/channels browser, files browser, call screen.
- **Exit:** all views compile; logic covered by `egui_kittest` headless tests where feasible.

### M11 — Polish & packaging
- Settings, keyboard shortcuts, themes, cross-platform build/packaging, docs, README.
- **Exit:** release build config + packaging notes.
- **Landed:** pure `shortcuts` mapping + dispatch; expanded, forward-compatible
  `Settings` (accent, font scale, compact, notifications, download dir) with a
  settings screen; light/dark + accent theming; `docs/PACKAGING.md` + the
  `packaging/` assets; `.github/workflows/ci.yml`; release `strip`; and the
  docs final pass (`STATUS.md`, README matrix, index). See `PROGRESS.md` (M11).

### M12 — Calendar
- Graph `calendarView` agenda + explicit week view; schedule (`POST
  /me/calendar/events`) and cancel (`DELETE`); offline-first `Calendar` service
  with a schema-v7 `meetings` cache; join-target resolution and the pure lobby
  helper; the join hand-off to the existing call path; a **Calendar** tab.
- **Exit:** calendar calls tested offline against stubs (paths/params/bodies +
  store effects); join classification and lobby transitions unit-tested;
  headless `egui_kittest` calendar test.
- **Landed:** `model::Meeting`, schema **v7** (`meetings`), `Calendar` +
  `Session::calendar()`, Graph `api` wrappers, the `Page::Calendar` tab (nav
  entry + `Ctrl/Cmd+2`) and `views/calendar.rs`, join hand-off to `CallService`.
  See `PROGRESS.md` (M12) and ADR-033.

### M13 — Self identity + inline media
- Fetch and cache the signed-in identity (Graph `/me` → `Identity`, schema-v8
  `accounts.self_mri`/`oid`) and apply `mark_own` to server-loaded history and
  the realtime collector so our own messages render on the own side.
- Extract inline image URLs from raw HTML (`text::image_urls`), fetch and cache
  them once with an offline-first `MediaCache` (Skype token only on Microsoft
  hosts), and render them in the egui timeline via `egui_extras` image loaders.
- **Exit:** identity mapping, `mark_own`, URL extraction and the media cache are
  tested offline against stubs (fetch + auth-header rule + cache hit); a
  headless `egui_kittest` test shows an own message and an image placeholder.
- **Landed:** `model::Identity`, schema **v8** (`accounts.self_mri`/`oid`),
  `Session::identity()`/`set_identity`/`client()`, `Messaging`/realtime
  `mark_own`, `api::whoami`/`fetch_media`, `text::image_urls`, `MediaCache`,
  `egui_extras` image loading in `views/conversation.rs`. See `PROGRESS.md`
  (M13) and ADR-034/ADR-035.

## 4. Working agreement (the loop)

For each milestone: implement (via subagent where useful) → review the diff → run
`cargo fmt/clippy/test/check` → commit with a scoped message → push to `origin`.

Documentation is updated continuously:

- `docs/PLAN.md` — this plan (checklist kept in sync).
- `docs/PROGRESS.md` — per-milestone status + what actually landed.
- `docs/DECISIONS.md` — architecture/technology decisions with rationale (ADRs).
- `docs/BLOCKERS.md` — blockers, workarounds, and deferred work.

## 5. Known constraints & risks

- **Headless environment:** the GUI window cannot be displayed here; verification is
  compile + unit tests + review. Documented in `docs/BLOCKERS.md`.
- **No live tenant:** real auth/chat/calls cannot be exercised end-to-end without
  Microsoft credentials; network paths are built testable with fake transports.
- **Protocol volatility:** Teams endpoints are reverse-engineered and change; region
  routing and fallbacks are required (the protocol crate already handles much of this).
- **Calls are the heaviest lift:** on Linux we lose macOS AVFoundation/VideoToolbox and
  must use cpal + VA-API/openh264 (or degrade gracefully to signalling-only).
- **Licensing:** vendored `ost` is MIT; the better-teams-derived patches carry an
  MIT + Microsoft-exclusion notice, which we preserve verbatim. See `docs/DECISIONS.md`.

## 6. Milestone checklist

- [x] M0 — Workspace & scaffolding
- [x] M1 — Authentication
- [x] M2 — Protocol clients & session core
- [x] M3 — Local persistence & offline cache
- [x] M4 — Realtime (Trouter)
- [x] M5 — Messaging
- [x] M6 — Teams & channels
- [x] M7 — Files
- [x] M8 — Presence & notifications
- [x] M9 — Audio/video calls
- [x] M10 — egui UI
- [x] M11 — Polish & packaging
- [x] M12 — Calendar
- [x] M13 — Self identity + inline media
