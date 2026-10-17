//! Realtime (Trouter) feed (milestone **M4**).
//!
//! The vendored protocol crate already implements Trouter end to end
//! (socket.io v1 over WSS, the registrar, the `3:::` ack contract, gzip/base64
//! unwrapping and exponential-backoff reconnect). We **wrap** it rather than
//! reimplementing (ADR-001/ADR-013):
//!
//! ```text
//!  connect_and_run_with_config(Some(cfg)) ──▶ ost::event_hub (global FIFO)
//!                                                 │  drain_wait(..)
//!                                        collector task (service.rs)
//!                                                 │  decode_all
//!                          ┌──────────────────────┴───────────────────┐
//!                          ▼                                          ▼
//!                    Arc<Store> (write-through)             EventBus (fan-out)
//! ```
//!
//! Three pieces:
//!
//! * [`event`] — the owned [`RealtimeEvent`] domain enum and the pure, total
//!   [`decode`] / [`decode_all`] JSON → event functions.
//! * [`bus`] — a `tokio::sync::broadcast` [`EventBus`] fan-out with a documented
//!   lag/resync story.
//! * [`service`] — [`RealtimeService`], which spawns the connection and
//!   collector tasks on a caller-provided runtime, plus the deterministic
//!   [`collect_once`] core used by tests and [`crate::Session::start_realtime`].
//!
//! Nothing in this module performs its own network I/O and nothing here can
//! panic on wire input; the only network access is inside the vendored
//! connection loop.

pub mod bus;
pub mod event;
pub mod service;

pub use bus::{DEFAULT_BUS_CAPACITY, EventBus, recv};
pub use event::{RealtimeEvent, ThreadChange, decode, decode_all};
pub use service::{RealtimeService, collect_once};
