# Squads — engineering docs

An alternative, cross-platform Microsoft Teams client in **Rust + egui**.

| Doc | What it covers |
|---|---|
| [PLAN.md](PLAN.md) | The milestone roadmap (M0–M11) and working agreement. |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Crate graph, responsibilities, data flow, protocol notes. |
| [DECISIONS.md](DECISIONS.md) | Architecture decision records (ADRs). |
| [PROGRESS.md](PROGRESS.md) | Live status per milestone and what actually landed. |
| [BLOCKERS.md](BLOCKERS.md) | Constraints, blockers, deferred work. |
| [reference/](reference/) | Live end-to-end shell probes kept for manual verification. |

## Quick start

```sh
# Build everything
cargo build

# Run the GUI (needs a display server)
cargo run -p squads-app

# Headless protocol CLI (login / api testing)
cargo run -p squads-protocol --bin squads-cli -- --help

# Tests
cargo test --workspace
```
