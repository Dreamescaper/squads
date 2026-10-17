# Squads — engineering docs

An alternative, cross-platform Microsoft Teams client in **Rust + egui**.

| Doc | What it covers |
|---|---|
| [STATUS.md](STATUS.md) | One-screen project state: what works, the ADR list, open blockers, how to verify by hand. |
| [PLAN.md](PLAN.md) | The milestone roadmap (M0–M12) and working agreement. |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Crate graph, responsibilities, data flow, protocol notes. |
| [DECISIONS.md](DECISIONS.md) | Architecture decision records (ADRs). |
| [PROGRESS.md](PROGRESS.md) | Live status per milestone and what actually landed. |
| [BLOCKERS.md](BLOCKERS.md) | Constraints, blockers, deferred work. |
| [PACKAGING.md](PACKAGING.md) | Build, install and cross-platform packaging instructions. |
| [reference/](reference/) | Live end-to-end shell probes kept for manual verification. |

## Quick start

```sh
export PATH="$HOME/.cargo/bin:$PATH"   # if cargo is not on PATH

# Build everything (default features: no device/network needed)
cargo build

# Run the GUI (needs a display server)
cargo run -p squads-app

# Headless protocol CLI (login / api testing)
cargo run -p squads-protocol --bin squads-cli -- --help

# Tests
cargo test --workspace
```

See [`STATUS.md`](STATUS.md) for the current state and the hand-verification
checklist, and [`PACKAGING.md`](PACKAGING.md) for release/packaging.
