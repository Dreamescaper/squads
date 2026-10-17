# Backlog

Work items beyond the planned milestones (M0–M13). Derived from the
protocol-coverage audit in the M13 session: these are the `ost` surfaces that
`squads-core`/`squads-app` still do not use and that correspond to real,
user-visible gaps.

Process: **one item at a time** — implement → review → `fmt`/`test`/`build` →
commit → push — updating the status here and in
[`PROGRESS.md`](PROGRESS.md)/[`DECISIONS.md`](DECISIONS.md) as each lands.

Legend: ⬜ not started · 🟡 in progress · ✅ done · 🚫 won't do (documented)

---

## B1 — Message search ⬜

**Gap.** `api::search` is unused; the app can only filter chat *names* (quick
switcher). There is no way to find a message.

**Approach.** Local-first: search the SQLite store (which already holds the
cached history), because the vendored Graph `/search/query` needs `Chat.Read`,
which the Teams sign-in token does **not** carry — server search is expected to
403 live. Wire the server path as an optional secondary and document the limit.

**Scope.**
- `store`: a search query (`LIKE` over lowercased `content` with an index, or
  FTS5 if it is available in bundled SQLite) with chat/sender/time filters,
  bounded and ordered by recency; a tested migration if a new index/column is
  needed.
- `core`: `Search` service (or `Store::search_messages`) returning
  `SearchHit { chat_id, chat_name, message_id, sender, snippet, timestamp }`;
  plus an `api` wrapper over `search_messages_data` for the server fallback.
- `app`: a **Search** page/overlay (nav rail + `Ctrl/Cmd+F`), query box,
  result list (chat, sender, snippet, time), click → open that chat and jump to
  the message; empty/loading/no-results states.

**Acceptance.** Offline tests: store search (match/no-match, ordering, limits,
special characters, case-insensitivity, no panics); the server wrapper against a
stub; a kittest test that typing a query renders matching results and selecting
one opens the chat.

---

## B2 — Chat management ⬜

**Gap.** No way to start a chat, leave one, mute/hide server-side, use folders,
or see pinned messages. (`api::chat` extras + `api::teams` member routes.)

**Split into two items.**

### B2a — New chat, leave, mute/hide ⬜
- `core`: wrappers for `create_one_to_one_chat_data`, `create_group_chat_data`,
  `leave_chat_with_client`, `set_chat_muted_with_client`,
  `set_chat_hidden_with_client`, `alerts_body`/`alerts_muted`, and chat roster
  (`list_chat_members_data`).
- `store`: persist server mute/hidden state (schema bump + migration).
- `app`: a "New chat" flow (pick people via B3's people search → create),
  a chat context menu (mute / hide / leave), and muted/hidden rendering in the
  chat list.

### B2b — Folders and pinned messages ⬜
- `core`: `conversation_folders_data`, `conversation_folder_move_with_client`,
  `chat_pinned_messages_data`, `chat_unpin_message_with_client`,
  `pick_pins`/`fill_pin_from_message`.
- `store`: folders (schema bump + migration).
- `app`: folder tabs in the chat list; a pinned-messages panel in the
  conversation info pane.

**Acceptance.** Offline stub tests for every write shape; store round-trips and
migrations; kittest coverage for the new-chat flow, the context menu and folder
tabs.

---

## B3 — File & people search ⬜

**Gap.** `api::filesearch` is unused.

**Scope.** `core`: wrappers for `search_files_data` (Graph drive search) and
`search_people_data` (directory, `ConsistencyLevel: eventual`), with
`SearchPage`-style paging. `app`: a search box on the Files page; a **people
picker** component reused by B2a's new-chat flow.

**Acceptance.** Offline stub tests for both shapes and paging; a kittest test
that a people query renders results and selection yields an id.

---

## B4 — Team administration ⬜

**Gap.** The rest of `api::teams`: public team discovery + join, member
add/remove, team settings, and channel message reactions (U-003).

**Scope.** `core`: wrappers for `search_public_teams_data`, `join_team_data`,
`add_team_member_data`, `remove_team_member_data`, `team_settings_data`,
`set_channel_reaction_data`/`unset_channel_reaction_data` (+ the pure
`channel_*_reaction_path` helpers). `app`: browse/join public teams from the
Teams page; member management for owners; reactions on channel messages.

**Acceptance.** Offline stub tests for each route (path/verb/body); channel
reactions applied to the store and rendered; kittest coverage for join + member
management.

---

## B5 — Recordings & transcripts ⬜

**Gap.** `api::recordings` and `api::transcripts` are unused, so meeting
recordings/transcripts cannot be listed or opened even though the call layer can
*start* a recording.

**Scope.** `core`: wrappers for `list_recordings_data`/`search_recordings_data`
and `list_transcripts_data`/`search_transcripts_data` (+ the `*_children_path`/
`*_search_path` helpers and `is_video`/`is_transcript` predicates); a service
that resolves OneDrive/SharePoint folders and exposes entries with download
links. `app`: a recordings/transcripts section in the conversation info pane,
with open/download.

**Acceptance.** Offline stub tests for listing, search and path building;
sorting/limit helpers; kittest coverage for rendering a recordings list.

---

## B6 — Explicitly not planned 🚫

Recorded so the audit has a home; revisit only on request:

- `api::apps` / `api::tabs` / `api::team_settings` — hosted third-party Teams
  apps need a webview + TeamsJS token broker (D-001).
- `api::schedule` (Shifts/time-off) — niche.
- `api::planner` — Planner tasks.
- `api::todo` — Microsoft To Do.
- `api::notes` — OneNote.

These are reachable through the vendored crate if a user asks for them, but they
are outside the "Teams chat/calls client" scope.
