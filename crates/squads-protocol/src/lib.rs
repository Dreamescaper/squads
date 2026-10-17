//! Squads protocol crate — vendored Teams protocol core.
//!
//! Library surface over the vendored `ost` sources (from the better-teams fork of
//! `eisbaw/ost`). Exposes auth/api/trouter/calling/models plus an `event_hub` that lets
//! embedders receive Trouter push events instead of only printing them.
//!
//! The crate intentionally exposes a large surface that not every consumer uses, so
//! dead-code warnings are suppressed here (this is vendored code, not Squads code).
#![allow(dead_code)]

pub mod api;
pub mod auth;
pub mod calling;
pub mod config;
pub mod event_hub;
pub mod models;
pub mod trouter;
