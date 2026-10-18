# Architecture

## Crate graph

```
                 ┌───────────────────────┐
                 │      teamtrim-app        │  egui binary (`teamtrim`)
                 │  views, app state,     │
                 │  request_repaint       │
                 └───────────┬───────────┘
                             │  commands / events (channels)
                 ┌───────────▼───────────┐
                 │      teamtrim-core       │  session, event bus, store, config
                 │  (no UI, no egui)      │
                 └───────────┬───────────┘
                             │  in-process async API
                 ┌───────────▼───────────┐
                 │   teamtrim-protocol      │  vendored `ost` (lib name `ost`)
                 │  auth/api/trouter/     │  auth, chat, files, presence,
                 │  calling/models        │  teams, trouter, calling
                 └───────────────────────┘
```

## Responsibilities

### `teamtrim-protocol` (vendored, lib `ost`)

Pure protocol knowledge, no UI. Key modules:

| Module | Purpose |
|---|---|
| `auth` | OAuth2 device-code + PKCE, the five-token exchange, config token storage. |
| `api::client` | `TeamsClient` — regional base URLs, auth headers, request helpers. |
| `api::chat` | Native chat service: list chats, read/send/edit/delete, reactions, receipts. |
| `api::files` | Microsoft Graph drives: resolve, list, upload (simple + resumable), download. |
| `api::teams` | Teams/channels, members, middle-tier writes. |
| `api::presence`, `api::media`, `api::search`, … | Presence, media fetch, search, planner, etc. |
| `trouter` | Trouter socket.io v1 push client + registrar + event hub. |
| `calling` | epconv signalling, SDP, ICE/TURN, SRTP, RTP/RTCP, codecs. |
| `config`, `models`, `event_hub` | Settings, shared data models, push event fan-out. |

The `teamtrim-cli` binary wraps the library for headless login/testing.

### `teamtrim-core` (ours)

Everything the UI needs that is not drawing:

- `paths` — platform directories (`~/.config`, data, cache).
- `session` — session lifecycle state machine (`SignedOut → SigningIn → Loading → Ready`).
- *(M1)* auth driver wrapping `ost::auth`, secure token storage.
- *(M2)* client facade, command/event definitions, tokio runtime.
- *(M3)* SQLite store + repositories.
- *(M4)* event bus fed by Trouter.

### `teamtrim-app` (ours, egui)

- Application state that mirrors `teamtrim-core` and local widget state.
- Views: shell layout (nav rail, chat list, timeline, composer, info panel), login.
- Theming, keyboard handling, and the async bridge: the core emits events over a channel;
  the UI drains them at the top of each frame and calls `ctx.request_repaint()` only when
  something actually changed (see the CPU note in ADR-006).

## Data flow (target, M2+)

```
user action ─▶ teamtrim-app view ─▶ Command { … } ─▶ teamtrim-core backend (tokio)
                                                        │
                              ost::api::* / trouter ────┤
                                                        ▼
                              store write-through ─▶ Event { … } ─▶ teamtrim-app (request_repaint)
```

Views never talk to the network directly; all mutation flows through the core and comes
back as events (the pattern `zapfast` documents as "UI → Action → apply").

## Where the protocol differs from Graph (why we don't just use Graph)

- **Chat** → native `ng.msg` service with `Authentication: skypetoken=…` (no `Chat.Read`).
- **Realtime** → Trouter (socket.io v1 over WSS), not SignalR.
- **Calls** → epconv + raw ICE/SRTP/RTP, not WebRTC.
- **Presence** → `presence.teams.microsoft.com`, not Graph `/me/presence`.
- **Files / teams / calendar** → Microsoft Graph (the token does have these scopes).
- **Team/channel writes** → middle tier when Graph denies the scope.
