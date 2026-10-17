# Squads

An alternative, cross-platform **Microsoft Teams** client built with **Rust** and
[**egui**](https://github.com/emilk/egui) (immediate-mode, event-driven, glow/OpenGL).

> Status: the planned milestones (**M0–M13**) are implemented. The workspace
> builds and tests **offline, with default features**; the network paths need a
> live tenant and the UI needs a display, neither of which exist in the build
> environment. See [`docs/STATUS.md`](docs/STATUS.md) for the one-screen state and
> [`docs/PLAN.md`](docs/PLAN.md) for the roadmap.

## Why

The official Teams client is heavy and the notable open-source alternative,
[better-teams](https://github.com/mrowlinson/better-teams), is macOS-only. Squads pairs a
portable Rust protocol core with a native egui UI so it can run on Linux, macOS and
Windows, while staying easy on RAM and CPU.

## Why egui (not a GPU-only toolkit)

GPU-only frameworks always create a GPU context and, on machines without a working Vulkan
driver, silently fall back to software rendering (high CPU). egui renders via OpenGL with
input-driven repaints, so an idle window costs ~0% CPU and the RAM baseline stays low.
See [`docs/DECISIONS.md`](docs/DECISIONS.md) (ADR-006).

## How it is built

Teams has no public chat/call API. Chat uses Microsoft's proprietary `ng.msg` service
(authenticated with a `skypetoken`), realtime uses Microsoft **Trouter** (socket.io v1 over
WebSocket), and calls use epconv SDP signalling with ICE/TURN/SRTP/RTP. Rather than
reimplement this fragile, reverse-engineered surface, Squads vendors the MIT-licensed
Rust protocol core from [`eisbaw/ost`](https://github.com/eisbaw/ost) (via the
[better-teams](https://github.com/mrowlinson/better-teams) fork) and builds a new
cross-platform UI on top. See [`docs/DECISIONS.md`](docs/DECISIONS.md).

```
crates/
  squads-protocol/   vendored protocol core (lib `ost`): auth, api, trouter, calling
  squads-core/       session lifecycle, event bus, local store, config
  squads-app/        the egui application
```

## Feature status

| Area | Works (verified offline) | Unverified (needs a live tenant / device / display) | Deferred |
|---|---|---|---|
| Auth | Device-code flow, five-token model, 0600 token store, refresh | Real AAD/`authsvc` exchange | Browser PKCE loopback (B-004) |
| Chat | Offline-first store + paging, optimistic send, edit/delete/reply/react/receipts; own messages rendered on the own side from the whoami identity | Server acceptance of the write shapes (U-002); live whoami (U-008) | — |
| Chat management | **New chat** (1:1 + group via the people picker), leave (with confirmation), server mute/hide reconciled with the local routing flag, a chat context menu, muted/hidden rendering; schema v10 | Live chat-service acceptance of create/leave/mute/hide/roster (U-011) | Folders + pinned messages (B2b) |
| Search | Local-first message search over the cached store (case-insensitive `LIKE`, wildcard-escaped, newest-first), a **Search** page + `Ctrl/Cmd+Shift+F`, click/Enter-to-jump with the match highlighted | The optional Graph `/search/query` fallback (expected to 403 with the Teams sign-in — U-009) | FTS5 index for very large histories |
| File & people search | Graph drive search + directory `/users` `$search` (single-window, 25); a global search box on the Files page; a **reusable people picker** (async query, multi-select, `selected_people()` for B2a's new chat) | Live Graph acceptance and the delegated `Files.Read`/`User.ReadBasic.All` scopes (U-010) | Paging (single-window only) |
| Inline media | `<img>`/GIF extraction, one-fetch-per-URL `MediaCache` (Skype token only on Microsoft hosts), images rendered in the timeline | Live media hosts/`Content-Type`s and rendered pixels (U-008/B-012) | Streaming; per-account cache |
| Realtime | Trouter wrap, typed event bus, write-through | Live push; call frames are filtered upstream (B-008) | — |
| Teams/channels | Metadata cache, channel history, thread replies, middle-tier writes | Write acceptance; channel reactions/receipts (U-003) | — |
| Calendar | Graph `calendarView` agenda + week view, schedule/cancel, offline `meetings` cache, join hand-off to calls | Live Graph calendar acceptance; join-by-id is a browser fallback (U-007) | — |
| Files | Graph list/upload/download/links/versions, coarse progress | Real SharePoint/OneDrive acceptance (U-004) | Streaming I/O |
| Presence | Two backends, unified by default; pure mapping | Unified token audience/shapes (B-009/U-005) | Quiet hours (B-010) |
| Notifications | Pure routing, unread/activity, settings switches | Desktop toasts (need DBus, B-010) | Quiet hours |
| Calls | State machine, epconv signalling, SDP/SRTP/RTP, loopback, call log | Live calls + real media devices (B-011/U-006) | Hardware video (D-002) |
| UI | egui shell + all screens, keyboard shortcuts, light/dark + accents, compact mode | Actual pixels/layout (B-012) | Attachment picker (B-012) |
| Packaging | `cargo build --release`, `.desktop`+icon, docs; CI builds Linux/macOS/Windows archives and attaches them to `v*` releases | No installer (AppImage/Flatpak/MSI) and no real signing/notarization (B-013) | Signed/notarized bundles |

The full blocker list is in [`docs/BLOCKERS.md`](docs/BLOCKERS.md).

## Building & running

```sh
export PATH="$HOME/.cargo/bin:$PATH"   # if cargo is not on PATH

cargo build                 # workspace, default features (device/network-free)
cargo run -p squads-app     # GUI (binary `squads`; requires a display server)
cargo test --workspace      # offline test suite
cargo build --release       # shipped configuration
```

Linux build prerequisites: a C toolchain, `pkg-config`, `libssl-dev`,
`libxkbcommon-dev` (+ `libxkbcommon-x11-dev`), `libwayland-dev`/X11,
`libgl1-mesa-dev`. (`libasound2-dev` and SDL2 are only needed for the
default-off `audio`/`video-capture` features.) See
[`docs/PACKAGING.md`](docs/PACKAGING.md) for the full, per-platform instructions
and for AppImage/Flatpak/macOS/Windows packaging notes.

### Keyboard shortcuts

`Ctrl/Cmd+K` quick chat switcher · `Ctrl/Cmd+F` search in conversation ·
`Ctrl/Cmd+Shift+F` search all messages · `Esc`
close overlays / clear search · `Ctrl/Cmd+,` settings · `Ctrl/Cmd+Shift+M`
mute (placeholder) · `Ctrl/Cmd+1`…`5` jump to Chats / Calendar / Teams / Files /
Calls · `j`/`k` or `↓`/`↑` chat navigation · `Ctrl/Cmd+Enter` send.

## Configuration & data

Resolved by `squads_core::AppDirs` (`directories`, project id
`dev.squads.squads`):

| | Linux | macOS | Windows |
|---|---|---|---|
| Config | `~/.config/squads` | `~/Library/Application Support/dev.squads.squads` | `%APPDATA%\squads\squads\config` |
| Data | `~/.local/share/squads` | `~/Library/Application Support/dev.squads.squads` | `%APPDATA%\squads\squads\data` |
| Cache | `~/.cache/squads` | `~/Library/Caches/dev.squads.squads` | `%LOCALAPPDATA%\squads\squads\cache` |

- `settings.json` — preferences (theme, accent, font scale, compact mode,
  notifications, download directory).
- `tokens.json` / `tokens-<profile>.json` — the token set, mode `0600` (secret).
- `store.sqlite` (+ `-wal`/`-shm`) — the offline cache; **unencrypted at rest**
  (B-007).
- `media/` — downloads when no download directory is set, and the inline-media
  cache (one hashed file per fetched image URL).

## Troubleshooting

- **The window never opens / "no display".** Squads is a GUI app and needs an X11
  or Wayland session. In a headless shell (CI, containers, SSH) `cargo run -p
  squads-app` cannot show a window; use the tests and the
  [`squads-cli`](crates/squads-protocol) binary for protocol work instead. This
  is why the UI is verified by compilation and headless `egui_kittest` tests,
  not screenshots (B-001/B-012).
- **`libEGL`/`libGL`/`libwayland` errors at start.** Install the graphics and
  windowing development libraries listed above; a software GL driver (Mesa
  `llvmpipe`) is sufficient.
- **No chat/call data.** Everything network-facing needs a signed-in Microsoft
  work/school account and a live tenant; nothing is exercised against production
  here (B-002).
- **Settings look wrong after a hand edit.** A malformed `settings.json` is a
  hard error by design (so a typo is noticed); delete the file to fall back to
  defaults. New fields are forward-compatible — an old file keeps loading.

## License

MIT. Vendored third-party code retains its own licenses and attribution; see
[`NOTICE`](NOTICE) and [`crates/squads-protocol/LICENSE`](crates/squads-protocol/LICENSE).
