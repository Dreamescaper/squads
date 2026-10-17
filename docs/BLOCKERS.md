# Blockers, Constraints & Deferred Work

This file tracks anything that prevents or limits progress, plus work consciously
deferred. Items are moved/closed as they resolve.

## Environment constraints (standing)

### B-001 — Headless build environment (no display server)

`$DISPLAY` is unset and there is no Wayland compositor. GUI applications need a display
to *run*, so the window cannot be launched or visually verified here.

**Impact:** UI work is verified by compilation, static review and `egui_kittest` headless
tests only; screenshots/manual QA are impossible in this environment.

**Mitigation:** keep UI logic separated from rendering so it can be unit-tested; use
`egui_kittest` (AccessKit-driven, no display needed) for view/interaction tests; lean on
`squads-cli` for end-to-end protocol verification; document how a developer runs the GUI
locally.

### B-002 — No live Microsoft tenant / credentials

Real authentication, chat, files and calls require a work/school Microsoft account and
enrolling an (unofficial) client. We must not authenticate automated agents against
production Microsoft services.

**Impact:** network paths cannot be exercised end-to-end here.

**Mitigation:** the protocol core already supports a fake transport for tests; build the
`Session`/store layers against fakes; provide a documented manual verification path.

### B-003 — Reverse-engineered, volatile protocol

Teams endpoints are undocumented and change without notice. Region routing
(`regionGtms`), fallback strategies and client-version headers are required.

**Impact:** features can break on Microsoft's side independent of our code.

**Mitigation:** reuse the protocol core's existing fallbacks; keep protocol-touching code
behind `squads-core`; record any known-fragile areas in this file.

### B-004 — Browser PKCE flow has no loopback listener yet (M1)

M1 landed the PKCE primitives (verifier/challenge/`state`, authorize URL,
redirect parsing) but not the local loopback HTTP listener that receives the
browser redirect and completes the authorization-code exchange. A loopback
listener plus a real browser and tenant cannot be exercised in this headless,
credential-free environment.

**Impact:** browser-based sign-in is not available; the supported sign-in path is
device code (which is fully implemented and tested).

**Mitigation:** the PKCE pieces are unit-tested and ready to wire up; add the
loopback listener and code redemption as an M10 login-screen follow-up (see
PROGRESS "M1 — Authentication").

### B-005 — `exchange_skype_token` is not unit-testable (M1)

The vendored `ost::auth::skype::exchange_skype_token` always uses the protocol
crate's private shared HTTP client and hard-coded `authsvc` URLs (work + personal
hosts), so it cannot be redirected to a local stub without modifying
`crates/squads-protocol` (which is off-limits).

**Impact:** the Skype-ish exchange step is covered only by the fake transport in
tests; the real HTTP call is exercised only against a live tenant.

**Mitigation:** `HttpTransport::exchange_skype` is a thin adapter; all flow logic
around it (ordering, `regionGtms` capture, refresh dependency on the Spaces
token) is covered offline. If the vendored crate ever exposes an injectable
client, add a local stub test.

### B-006 — 1:1 mate-name resolution and the vendored mate cache are not exercised offline (M2)

`ost::api::list_chats_data` names 1:1 chats without a topic through a second
pass: a `whoami` call, a per-thread roster read and a history read, memoised in
a process-global cache keyed by `ost::config::active_profile()` (which defaults
to `"default"`). `squads-core`'s `ApiClient` does not (yet) set that vendored
active profile, and the extra requests would need several stub endpoints.

**Impact:** the offline `list_chats` tests give every chat a topic, so this
branch is skipped; only the topic/last-sender/system-label naming is covered
here. The mate-name path is exercised only against a live tenant.

**Mitigation:** the code path otherwise passes through untouched; M3/M5 should
decide whether `squads-core` owns mate-name resolution (using its own store)
rather than relying on the vendored global profile. Once that lands, extend the
stub to serve `whoami` + roster + history and cover the branch offline.

### B-007 — Local store keeps chat content unencrypted at rest (M3)

`crates/squads-core`'s SQLite store (`data/store.sqlite`) is **plain
`rusqlite`/`bundled`** — no encryption (ADR-011). Accounts, chats and messages
are written in the clear, including display names, sender MRIs, raw message HTML
and reaction reactor names. The file is protected only by filesystem/OS-account
permissions (and WAL leaves `store.sqlite-wal`/`-shm` siblings with the same
exposure).

**Impact:** anyone with read access to the user's data directory can read cached
Teams content. This is not acceptable for a general release and must be
revisited (encryption at rest, e.g. SQLCipher, or an encrypted blob column).

**Mitigation:** the storage layer is behind `squads_core::store::Store`, so an
encrypted engine can replace it without touching callers. A future ADR must also
settle the key source and plaintext→encrypted migration; see ADR-011.

### B-008 — Realtime (M4): call frames filtered, global hub, reconnect token staleness

Three properties of the wrapped vendored Trouter path (ADR-013) are worth being
explicit about:

1. **Call notifications cannot be surfaced from `event_hub`.** The vendored
   `trouter::data_frame_event` *deliberately* filters call frames
   (`NGCallManagerWin` / `SkypeSpacesWeb`) out of the hub, so our
   `RealtimeEvent` has no call variant and `squads-core` never sees ringing/
   invitation events. Calls still take the vendored path (auto-answer unless
   `TEAMS_MANUAL_CALLS` is set).

   **Impact:** incoming/ongoing call UI is impossible through the M4 bus.

   **Mitigation/deferred (M9 status):** real call signalling landed in **M9**
   (`squads-core::calls`): the incoming-call parser (`calls::incoming::parse_incoming`)
   and the `CallService::on_notification` seam exist and are tested, and calls
   can be placed/accepted/ended via the vendored epconv path. **The producer is
   still missing**: no `RealtimeEvent` variant can carry a call frame, so
   `on_notification` has nothing feeding it. The recommended fix is a small
   `squads-protocol` change — stop filtering call frames in
   `trouter::data_frame_event`, add a call event (or a dedicated call hub), and
   have the M4 collector hand the raw JSON to `on_notification` — plus a
   `TEAMS_MANUAL_CALLS`-style opt-out so the M4 feed and M9 handling do not fight
   over the same frames. Tracked as a follow-up for M10/UI wiring.

2. **`ost::event_hub` is process-global and bounded (1024, oldest dropped).**
   Every squads-core account (and any other embedder in-process) shares one FIFO.
   A full queue silently drops the oldest events, and two collectors would steal
   each other's events.

   **Impact:** with a single account and a single collector (today) this is
   fine; multi-account would cross-feed. Tests that publish to the hub must
   serialize (the M4 tests take a module lock, use unique ids and drain first).

   **Mitigation:** the collector drains the hub fully each pass; if multi-account
   lands, either give the service its own hub (a vendored change) or tag events
   with a profile upstream. The bounded drop is partially covered by the
   server-side `message_loss` → `Resync` signal.

3. **Reconnect token staleness.** `connect_and_run_with_config` reuses the same
   in-memory `Config` on every reconnect, so a reconnect after the access tokens
   expire re-negotiates with stale credentials.

   **Impact:** without intervention the socket can spin on auth failures during
   backoff (1s…64s).

   **Mitigation:** `squads-core` stops the feed whenever its own token set
   changes (`restore`, sign-in completion, `ensure_fresh_tokens`, `sign_out`) and
   the caller must call `Session::start_realtime()` again; the vendored loop
   itself is not restarted with fresh tokens mid-flight. A future M5/M10 wiring
   should restart realtime after every refresh.

### U-001 — M3 store aspects not exercised offline

- **Cross-account chat-id collision:** `messages` is keyed `(id, chat_id)` with
  `profile` a non-key column, and `messages`/`messages_before` query by `chat_id`
  only (per the M3 API). Two profiles that somehow share a chat id would see each
  other's rows. In practice chat ids are account-unique, so this is untested and
  assumed safe.
- **Boundary paging:** `messages_before` compares the `timestamp` string with
  `<`; messages that share the boundary timestamp are skipped. Wire timestamps
  are millisecond-resolution in practice; the assumption is not exercised
  against a live tenant.
- **Crash/atomicity:** atomic writes for settings and transactional migrations
  are tested for correctness, but an actual power-loss/`SIGKILL` mid-write cannot
  be reproduced headlessly.
- **Concurrency:** `Store` is asserted `Send + Sync` and exercised single-threaded;
  real multi-threaded lock contention (UI + M4 realtime) is not benchmarked here.

### U-002 — M5 messaging aspects not exercised offline

M5's chat calls are thin wrappers over the vendored crate and are tested against
local TCP stubs (request shape + store effects). The following cannot be
exercised here (B-002 — no live tenant):

- **Live send/edit/delete/reply/react/mark-read.** The exact server acceptance of
  the POST/PUT/DELETE bodies — in particular that the real chat service returns a
  `Location` (or `OriginalArrivalTime`) we can parse as the new message id, and
  that a re-used `clientmessageid` is genuinely de-duplicated server-side — is
  only verifiable against a real tenant. The vendored crate's own docs already
  flag the reactions lane as *best-effort, not yet verified live*, and the
  `?behavior=softDelete` delete shape as "exactly what the Teams web client
  sends".
- **Channel-vs-chat differences.** The vendored reactions/receipts lane targets
  the native chat service with `skypetoken`; channel conversations
  (`is_channel_conversation_id`, `@thread.tacv2`/`@thread.skype`) route through
  different endpoints (thread replies, middle tier for some writes) that M5 does
  **not** special-case — `Messaging` treats every id uniformly. M6 (teams &
  channels) must add channel-aware send/reply/reaction/receipt routing. The
  read-receipt `consumptionhorizons` endpoint in particular is a thread
  (`/v1/threads/{id}/…`) call whose behaviour on channels is unverified here.
- **Pending→sent re-key races.** `send_text` replaces the placeholder row with
  the server-id row; if the realtime `NewMessage` for the same message lands in
  between, a duplicate row can be created until a resync. Offline tests cover the
  sequential paths only.
- **Reaction reconciliation.** The local `react` adjustment is a coarse ±1 with no
  reactor identity (we do not know our own MRI in `Messaging`); canonical counts
  arrive with the next realtime update/history refresh. Not verified live.

**Impact:** the wire-level correctness of the write calls is assumed from the
vendored crate + stubs, not proven; channel messaging needs M6 routing.

**Mitigation:** keep the write calls behind `ApiClient`/`Messaging` so M6 can
slot in channel routing; M8 consumes `read_state`; the deterministic stub tests
pin the exact request shapes so a regression in formatting is caught.

### U-003 — M6 teams/channels aspects not exercised offline

M6's teams/channel calls are thin wrappers over the vendored crate, tested
offline against local TCP stubs (method/path/auth/body + store effects). The
following cannot be exercised here (B-002 — no live tenant):

- **Channel reactions and read receipts are best-effort and not wired.** The
  chat-service reaction endpoint (`/messages/{id}/reactions`) **404s** on channel
  threads and the consumption-horizon call (`/v1/threads/{id}/consumptionhorizons`)
  **403s** (`AclCheckFailed`); the alternative Graph `setReaction`/`unsetReaction`
  paths the vendor added are **NOT verified live** (auth was down when they
  landed). M6 therefore exposes **no** channel react/mark-read: chats keep the M5
  [`Messaging`] paths. Wiring channel reactions/receipts needs a signed-in probe
  to settle the Graph-vs-chat-service id mapping and the scope grant.
- **`create_channel` rides Graph, not the middle tier.** The vendored
  `create_channel_data` posts to Graph (`POST /teams/{id}/channels`, body
  `displayName` + optional `description`); like the channel edit/delete routes it
  is *not* live-verified and may surface a 403 (the Teams first-party token's
  `Channel.Create` grant is unconfirmed). The milestone brief listed it among the
  middle-tier writes, but the vendored helper uses Graph; `ApiClient::create_channel`
  wraps the helper as-is. If Graph refuses it live, routing it through the middle
  tier is a contained change (see ADR-018).
- **Write acceptance for channel/team writes.** The exact server acceptance of
  the Graph create, the middle-tier `PATCH`/`DELETE …/beta/teams/{thread}/channels/{channel}`
  bodies and `POST …/beta/teams/create` — in particular that the middle tier keys
  the delete on the URL and that the create answer carries `value.siteInfo.groupId` —
  is assumed from the vendored crate + stubs, not proven. The stub tests pin the
  request shapes so a regression is caught.
- **Thread-reply reconciliation.** `reply_in_thread` stores the reply under the
  derived conversation `<channel>;messageid=<root>`; the realtime `NewMessage`
  for a channel reply would presumably arrive on that conversation, but the
  live `conversationLink`/id mapping is unverified. A pending→sent realtime race
  can still create a duplicate row until a resync (same shape as U-002).
- **Roster/owner semantics.** `TeamMember.id` is the **membership** id, not the
  user id; the owner flag is derived from `roles` containing `"owner"`. Both come
  straight from the vendored parser and are not validated against a live roster.

**Impact:** channel write correctness and the channel reaction/receipt story are
assumed from the vendored crate + stubs.

**Mitigation:** keep the writes behind `ApiClient`/`Teams`; the deterministic stub
tests pin the exact paths/bodies/auth so a regression is caught; wire channel
reactions/receipts in a live-verified follow-up (candidate for M8, alongside
presence/notifications).

### U-004 — M7 files aspects not exercised offline

M7's file calls are thin wrappers over the vendored Graph code, tested offline
against local TCP stubs (method/path/auth/body + store effects). The following
cannot be exercised here (B-002 — no live tenant):

- **Real SharePoint/OneDrive acceptance.** The exact server behaviour of the
  Graph list endpoints (chat-service `properties.files` → `/shares/{u!…}/driveItem`
  resolution, channel `filesFolder` + `/drives/{d}/items/{i}/children`,
  `/me/drive/recent`), the simple-PUT upload response (does it carry a
  `parentReference.driveId` and an eTag whose GUID is the attachment id?), the
  resumable-session handshake (does Graph answer `202` for every non-final
  fragment and `200` with the driveItem for the last?) and the
  `createLink`/`versions`/`restoreVersion`/manage bodies are assumed from the
  vendored crate + stubs, not proven. The stub tests pin the request shapes so a
  regression is caught.
- **The chat-service `reference` file post.** The upload works by mining the
  uploaded item's eTag for the attachment GUID and posting a
  `properties.files` (JSON string) message; whether the real chat service
  accepts it (and whether a realtime `NewMessage` for the file post reconciles
  with the cached row) is unverified. An unresolved/invalid eTag makes the
  vendored post fail — there is no live example to confirm the eTag always
  carries a GUID for chat-folder uploads.
- **Chat-vs-channel upload routing.** `upload_file_data_idem` sends chat-shaped
  ids straight to the sender's OneDrive chat-folder and channel-shaped ids to
  the channel `filesFolder` (after a joinedTeams + per-team channels scan); only
  the chat path is covered offline here (the channel path needs the team scan
  stubs).
- **Coarse download progress.** The vendored downloader buffers the whole body,
  so `Files::download` reports only `(0,size)`/`(written,size)`; there is no way
  to exercise (or observe) intermediate progress offline.
- **Drive-item id collisions.** See ADR-019: the store is keyed `(id, profile)`
  and a driveItem id collision across drives in one profile would have the
  second listing replace the first. Not observable without two colliding drives.

**Impact:** the wire-level correctness of the file calls is assumed from the
vendored crate + stubs, not proven; only the chat upload path is exercised.

**Mitigation:** keep the calls behind `ApiClient`/`Files` so a live fix is
contained; the deterministic stub tests pin the exact paths/bodies/headers
(including the `Content-Range` fragments and the `u!<base64url-nopad>` share
id) so a regression is caught; streaming download/upload is a future follow-up.

### B-009 — Unified presence token/audience is unverified (M8)

The Graph presence routes (`GET /me/presence`, `POST
/me/presence/setUserPreferredPresence`) **403 live**: the Teams first-party
token does not carry the `Presence.Read*` scopes. M8 therefore defaults to
Teams' **unified** presence service (`presence.teams.microsoft.com`), but its
**token audience is not confirmed**: `better-teams` minted a token for the
`https://presence.teams.microsoft.com` resource, whereas we only hold the M1
`spaces`/`skype`/`graph`/`ic3`/`recorder` tokens. The unified backend sends the
**skype** token as a Bearer header — a documented, **unverified** choice.

**Impact:** presence read/write may be rejected (401/403) against a live tenant
until the correct audience/header is confirmed. The unified request/response
**shapes** are likewise unverified (see U-005).

**Mitigation:** the token is a parameter of `UnifiedBackend` (and
`Session::presence`); if the skype token is wrong, only that choice (or a
token-audience mint in M1) changes, not the call sites. The stub tests pin the
paths/bodies/headers.

### B-010 — Desktop notifications and quiet hours (M8)

Desktop notifications go through `notify-rust`, which needs a freedesktop
notification daemon over DBus. This environment has no display and no session
bus, so the path cannot be exercised here — the same constraint as the OS
keyring (ADR-008). The notifier is behind the **default-off**
`desktop-notifications` cargo feature; the default build never compiles
`notify-rust`.

**Impact:** notification delivery is verified only by the `Notifier` seam
(`RecordingNotifier`/`LogNotifier`); the real toast cannot be observed here.
**Quiet hours are not implemented** (no clock in the router, no settled
settings), so notifications fire at all hours.

**Mitigation:** routing is a pure, tested function and delivery is one trait
method; a settings screen can add quiet hours by threading a window through
`NotificationContext` without touching the notifier.

### U-005 — M8 presence/notification aspects not exercised offline

M8's presence calls are checked against local TCP stubs (method/path/body/auth)
and its routing/unread logic is pure. The following cannot be exercised here
(B-002 — no live tenant):

- **Live presence acceptance.** Whether the unified service accepts the skype
  token (B-009), whether `getpresence` returns the shapes we parse, whether
  `forceavailability` needs a different body/expiration, and whether a batch
  read of arbitrary MRIs is permitted are all unproven. The mapping tolerates
  several wrappers/keys to soften a mismatch.
- **Reply detection** relies on `Message::reply_to` (the vendored
  `split_reply_quote`) and a caller-supplied set of our own message ids; the
  realtime `NewMessage` for a reply may not always carry the parent link in the
  same shape.
- **Mention detection** is heuristic: an index-only `<at id="0">` tag yields no
  MRI (we do not parse the message's `mentions` property array), and a plain
  `@name` token is matched by display name only. A message that mentions us
  solely through an index-only tag + the `mentions` array would be missed.
- **Read-horizon edge cases.** `unread_count`/`unread_messages` compare
  timestamps against the horizon message's timestamp; a horizon message that is
  no longer cached reads the whole chat as unread (documented), and messages
  sharing a timestamp boundary may be miscounted (millisecond resolution in
  practice).
- **Reaction / missed-call notifications** are placeholders: the M4 bus carries
  no reaction event and filters call frames out (B-008), so no producer exists
  yet.
- **Mute** is a local flag (schema v5); syncing it with the server's `alerts`
  property (`set_chat_muted_with_client`) is not wired, so a mute set in another
  client is not observed here.

**Impact:** presence correctness and the mention/reply/unread derivations are
assumed from stubs + pure logic, not proven against Teams.

**Mitigation:** keep the transport-injectable service and pure router; the
deterministic stub tests pin the request shapes so a regression is caught; the
unwired reaction/call producers are M9/UI follow-ups.

## Deferred work

### D-001 — Third-party Teams app hosting (TeamsJS/web frames)

Hosted apps / tabs require a webview and the TeamsJS token broker. Deferred;
read-only tab listing will ship first (M6).

### D-002 — Linux hardware video (VA-API/NVENC)

macOS uses AVFoundation/VideoToolbox. On Linux we start with signalling-only calls and
openh264 software codecs; hardware acceleration is deferred (M9).

### D-003 — Mobile

Out of scope.

### B-011 — Calls: live signalling, the Trouter surl, and real media (M9)

M9's call layer is fully offline-testable, but three things cannot be exercised in
this environment (no tenant, no devices, no live recorder service):

1. **The real `trouter_surl` is not in `region_gtms`.** It is the callback path
   prefix the negotiated Trouter session returns (`SessionResponse::surl`), which
   `squads-core` does not own. `CallEndpoints::from_region_gtms` therefore reads
   `region_gtms.trouterSurl` (our convention) and falls back to the empty string;
   a caller that needs the real value must `CallSignaller::set_endpoints` after
   negotiating the session. The signalling request shapes are pinned by stub
   tests regardless.
2. **Live call acceptance.** Whether the epconv/acceptance/media-answer/end
   shapes are accepted by the live service, and how the remote answer surfaces,
   is unproven (see U-006). The remote answer is **not observable** over the M4
   bus (B-008), so `CallService::place`/`accept` stop at `Connecting` and the
   caller drives `mark_connected` once media is up.
3. **Real media devices.** Audio capture/playback (`cpal`/ALSA) and video
   capture/display (V4L2 + `openh264` + SDL2) cannot be opened here; SDL2 dev
   libraries are not installed, so the `video-capture` feature must not be
   enabled in CI. Real recording additionally needs a **live recorder service**
   (the recorder endpoint/session id are assigned per call and arrive over
   Trouter), so only the socket-free recording steps (`add_bot`, `stop`) are
   tested — the orchestrator `CallRecorder::start` is live-only.

**Impact:** the media path and live signalling are verified only through the
offline loopback (`macav::call_dry_run`), the SDP/SRTP/RTP primitive tests and the
stub request-shape tests; real calls and recording are not.

**Mitigation:** keep the `MediaSession` seam and the `CallSignaller`/`CallRecorder`
wrappers transport-injectable so a live fix is contained; the stub tests pin the
exact paths/headers/bodies so a regression is caught; wire real devices and a live
recording flow as a follow-up.

### U-006 — M9 call aspects not exercised offline

M9's signalling and recording calls are thin wrappers over the vendored crate,
tested offline against local TCP stubs (method/path/headers/body), and its media
primitives (SDP/SRTP/RTP) and loopback are pure/deterministic. The following
cannot be exercised here (B-002/B-011):

- **Live acceptance of the signalling shapes.** That the live epconv service
  returns a usable `conversationController` and accepts our join body, that the
  acceptance/media-answer bodies are accepted (and that the SDP answer is
  well-formed enough for the media controller), and that the `end` link tears the
  call down, are assumed from the vendored crate + stubs.
- **The remote-answer transition.** With call frames filtered out of the M4 bus
  (B-008) there is no event to move `Connecting` → `Connected`; M9 exposes
  `mark_connected` for the caller. A real implementation needs the B-008 fix (or
  the vendored signalling callbacks) to drive it from the wire.
- **The actual media path.** SRTP/RTP are tested as primitives and looped back,
  but sending/receiving against a real media controller (ICE connectivity,
  renegotiation, RTCP feedback, video) is unverified; the generated SDP uses
  placeholder ports/candidates.
- **Recording.** Only `add_bot` and `stop` request shapes are pinned; the full
  flow (Trouter `addParticipantSuccess` → recorder discovery → transcription →
  recording) needs a live recorder service and is unverified. The recorder-base
  region fallback is the vendored code's.
- **Call history fidelity.** `call_log.thread_id` is empty for an incoming call
  whose invitation does not carry the thread id, and `started_at` is our local
  clock (not the wire's); missed-vs-declined classification is a local heuristic.

**Impact:** live call correctness is assumed from the vendored crate + stubs, not
proven; the media path is exercised only offline.

**Mitigation:** keep signalling/recording behind `CallSignaller`/`CallRecorder`
and media behind `MediaSession`; the deterministic stub tests pin the request
shapes so a regression is caught; fix B-008 to drive the state machine from the
wire and add a live-device media implementation.

### B-012 — UI verification is wiring-only headlessly (M10)

The M10 UI cannot be shown here (B-001), so it is verified by compilation, static
review and `egui_kittest` **headless** tests (ADR-028). Those tests drive the real
`eframe::App` through AccessKit and assert rendered labels and the app's applied
`Action` log, but they do **not** render pixels — layout/overlap/colour/spacing
regressions and visual polish are not caught.

Two UI paths are also deliberately incomplete:

- **Attachment picker is a stub.** The composer's **Attach** button queues
  `Action::Attach`, which currently only surfaces a toast; the real file dialog
  (and the `Files::upload` call behind it) is a follow-up. The upload *service*
  is complete and tested (M7); only the picker UI is missing.
- **Calls are signalling-only in the UI.** The call screen drives the real
  `CallService` state machine, but there is no media device in this build
  (B-011), so it says so and exposes a manual "Mark connected" step; the remote
  answer still cannot arrive over the M4 bus (B-008).

**Impact:** the UI's *behaviour* is covered; its *appearance* and the picker/live
media are not.

**Mitigation:** a developer on a machine with a display should run
`cargo run -p squads-app` and walk the checklist in `docs/PROGRESS.md` ("M10 —
egui UI → How to run the GUI locally"); wire the picker and real media as
follow-ups beyond the planned milestones (see B-013).

### B-013 — M11 polish limits: shortcut placeholders, notification settings, and unbuilt bundles

M11 landed keyboard shortcuts, expanded settings, theming and packaging *docs*,
but three things are deliberately bounded:

1. **Two shortcuts are placeholders.** `Ctrl/Cmd+Shift+M` (mute) has no media
   device in the default build, so it toggles an app-level flag and shows a
   toast; `Ctrl/Cmd+Enter` (send) is a no-op with no conversation open. The
   mapping and dispatch exist and are tested; the underlying features are
   B-011/B-012.
2. **The notification settings gate *routing*, not *delivery*.** The
   `notifications_enabled` / `notify_only_mentions` switches feed the pure
   [`route`](../crates/squads-core/src/notifications.rs) and are honoured by the
   app's realtime path, but the default notifier is still a `LogNotifier`: real
   desktop toasts need the default-off `desktop-notifications` feature and a
   session bus (B-010). Quiet hours remain unimplemented.
3. **Archives are built by CI; installers and signing are not.**
   `.github/workflows/release.yml` now builds a per-platform archive on native
   runners (Linux `.tar.gz`, macOS `.zip` with an ad-hoc-signed `Squads.app`,
   Windows `.zip`) and attaches them to a GitHub Release on `v*` tags. What is
   still *not* produced or verified: AppImage/Flatpak/MSI, and any real
   signing/notarization (needs Developer ID / Authenticode secrets). The macOS
   `.app` is only ad-hoc signed, so it runs locally but will not pass Gatekeeper
   for distribution. See `docs/PACKAGING.md` §6. The `strip = "symbols"` release
   profile and the CI `cargo build --release` are verified here.

**Impact:** the M11 *surface* (mapping, settings, persistence, theming, docs,
CI) is verified offline; the *features behind the placeholders* and the
*installers* are not.

**Mitigation:** the shortcut mapping is pure and additive (add an arm in
`resolve`/`apply_shortcut` once a feature exists); the notification settings are
already threaded to the router; the packaging instructions are concrete enough
for a contributor to run and correct on real hardware.

### U-007 — M12 calendar aspects not exercised offline

M12's calendar calls are thin wrappers over the vendored Graph calendar lanes,
tested offline against local TCP stubs (path/params/body + store effects), and
its join/lobby logic is pure. The following cannot be exercised here (B-002 — no
live tenant):

- **Live Graph calendar acceptance.** The exact server behaviour of the
  `calendarView` read (does the returned `dateTime` always match the `$select`
  shape we order on?), the `POST /me/calendar/events` create with
  `isOnlineMeeting`/`teamsForBusiness` (does Graph provision `onlineMeeting
  .joinUrl` every time?), and the `DELETE` cancel are assumed from the vendored
  crate + stubs, not proven. The stub tests pin the request shapes so a
  regression is caught.
- **Join-by-id is unavailable with the Teams sign-in.** The vendored
  `graph_get` refuses `GET /me/onlineMeetings` (`graph_denied`:
  `OnlineMeetings.Read` is not granted to the Teams web client), so
  `Calendar::resolve_join` for a typed **numeric** meeting id always falls back
  to the browser `/meet/<id>?p=` link (kind `meeting-id`). The Graph lookup
  wrapper exists and is unit-tested only for its refusal; a live tenant would
  confirm whether a suitably-scoped token changes this. Recorded in ADR-033.
- **The lobby machine has no realtime producer.** `lobby_step`/`Lobby` wrap the
  vendored pure `lobby_next` and are fully tested, but call frames are filtered
  out of the M4 bus (B-008), so nothing drives `idle → joining → lobby →
  admitted` from the wire; the UI must feed the transitions from the
  call-signalling callbacks once that producer exists.
- **Time-zone fidelity.** `Meeting::start`/`end` keep the Graph `dateTime`
  string verbatim (no timezone; the projection drops `timeZone`), so the store's
  `from_secs` filter and ordering assume the strings share the wire format and
  are effectively UTC-ish. A meeting in a non-UTC zone may be filtered at the
  boundary by up to the zone offset; the assumption is not exercised live.

**Impact:** calendar correctness is assumed from the vendored crate + stubs, not
proven; the join-by-id path is a browser fallback in practice, and the lobby has
no live driver.

**Mitigation:** keep the calls behind `ApiClient`/`Calendar` so a live fix is
contained; the deterministic stub tests pin the exact paths/params/bodies so a
regression is caught; the join classification and lobby machine are pure and
fully unit-tested.
