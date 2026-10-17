# Progress

Live status of the roadmap in [`PLAN.md`](PLAN.md). Updated as work lands.

Legend: ✅ done · 🟡 in progress · ⬜ not started · ⛔ blocked (see [BLOCKERS.md](BLOCKERS.md))

| # | Milestone | Status | Notes |
|---|---|---|---|
| M0 | Workspace & scaffolding | 🟡 | Workspace + 3 crates; vendored protocol core; docs. |
| M1 | Authentication | ⬜ | |
| M2 | Protocol clients & session core | ⬜ | |
| M3 | Local persistence & offline cache | ⬜ | |
| M4 | Realtime (Trouter) | ⬜ | |
| M5 | Messaging | ⬜ | |
| M6 | Teams & channels | ⬜ | |
| M7 | Files | ⬜ | |
| M8 | Presence & notifications | ⬜ | |
| M9 | Audio/video calls | ⬜ | |
| M10 | egui UI | ⬜ | |
| M11 | Polish & packaging | ⬜ | |

---

## M0 — Workspace & scaffolding

**Goal.** Turn the single-crate egui hello-world into a workspace with a vendored protocol
core, a UI-free core crate, and the egui app; document the architecture.

**Landed.**

- Root `Cargo.toml` converted to a workspace (`crates/squads-protocol`, `crates/squads-core`,
  `crates/squads-app`), with shared dependency versions and the egui-oriented
  dev/release profiles.
- Vendored `ost` (from the better-teams fork of `eisbaw/ost`) as `crates/squads-protocol`
  (package `squads-protocol`, library name `ost`), keeping its CLI as `squads-cli` for
  headless protocol testing. `LICENSE` preserved; attribution added in `NOTICE`.
- `crates/squads-core`: `AppDirs` (platform paths), `CoreError`/`Result`, and a
  `Session`/`SessionState` skeleton, with unit tests.
- `crates/squads-app`: the `squads` egui binary now depends on `squads-core` and renders
  the resolved session state, proving the workspace wiring.
- Docs: `PLAN.md`, `ARCHITECTURE.md`, `DECISIONS.md`, `PROGRESS.md`, `BLOCKERS.md`,
  `reference/` (live e2e probes), root `README.md`, `NOTICE`.

**Decisions.** ADR-001 (reuse `ost`), ADR-002 (workspace), ADR-006 (egui over GPUI).

**Blockers.** B-001 (headless: GUI cannot run), B-002 (no live tenant).
