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

## Deferred work

### D-001 — Third-party Teams app hosting (TeamsJS/web frames)

Hosted apps / tabs require a webview and the TeamsJS token broker. Deferred;
read-only tab listing will ship first (M6).

### D-002 — Linux hardware video (VA-API/NVENC)

macOS uses AVFoundation/VideoToolbox. On Linux we start with signalling-only calls and
openh264 software codecs; hardware acceleration is deferred (M9).

### D-003 — Mobile

Out of scope.
