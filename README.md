# Squads

An alternative, cross-platform **Microsoft Teams** client built with **Rust** and
[**egui**](https://github.com/emilk/egui) (immediate-mode, event-driven, glow/OpenGL).

> Status: early. The protocol core is in place; the UI and the session/store layers are
> being built milestone by milestone. See [`docs/PLAN.md`](docs/PLAN.md).

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

## Building

```sh
cargo build            # workspace
cargo run -p squads-app  # GUI (requires a display server)
cargo test --workspace
```

Linux build prerequisites: a C toolchain, `libssl-dev`, `libasound2-dev` (audio),
`libxkbcommon-dev`, `libwayland-dev`/X11, and `libgl1-mesa-dev`.

## License

MIT. Vendored third-party code retains its own licenses and attribution; see
[`NOTICE`](NOTICE) and [`crates/squads-protocol/LICENSE`](crates/squads-protocol/LICENSE).
