//! TeamTrim — an alternative Microsoft Teams client built with Rust + egui.
//!
//! Boot sequence (milestone **M10**):
//!
//! 1. Initialise `tracing`.
//! 2. Resolve the platform directories ([`AppDirs::discover`]) and load
//!    [`Settings`] (theme, window size).
//! 3. Build [`TeamTrimApp`], which restores the session (offline — a stored token
//!    set, if any) and paints the cached store immediately.
//! 4. Open the egui window with [`eframe::run_native`].
//!
//! ## UI lifecycle (eframe 0.37)
//!
//! `eframe::App` exposes two phases:
//! * `logic(&mut self, ctx, frame)` — runs even while the window is hidden; the
//!   app drains backend events here and applies them.
//! * `ui(&mut self, ui, frame)` — the paint pass; the views compose the panels
//!   and queue actions, which are applied after drawing.
//!
//! ## CPU footprint (ADR-006 / ADR-027)
//!
//! egui only repaints on input or when something calls
//! [`egui::Context::request_repaint`]. [`TeamTrimApp`] requests a repaint **only**
//! when a backend event was applied, so an idle window costs ~0% CPU. Keep that
//! property: never request a repaint unconditionally, and do not animate
//! gratuitously.

mod app;
mod icon;
mod runtime;
mod shortcuts;
mod theme;
mod views;
mod webview;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use eframe::egui;
use teamtrim_core::{AppDirs, Settings};

use crate::app::TeamTrimApp;

fn main() -> eframe::Result<()> {
    init_tracing();

    let dirs = Arc::new(AppDirs::discover().expect("resolve app dirs"));
    dirs.ensure().expect("create app dirs");
    tracing::info!(config = %dirs.config.display(), data = %dirs.data.display(), "starting teamtrim");

    let settings = Settings::load(&dirs).unwrap_or_else(|error| {
        tracing::warn!(%error, "could not load settings; using defaults");
        Settings::default()
    });

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("TeamTrim")
        .with_app_id("dev.teamtrim.teamtrim")
        .with_inner_size([settings.window.width, settings.window.height])
        .with_min_inner_size([640.0, 440.0]);
    // Embedded artwork, so the window/taskbar icon exists even when the app was
    // not installed from `packaging/`.
    if let Some(icon) = icon::app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "TeamTrim",
        native_options,
        Box::new(move |cc| Ok(Box::new(TeamTrimApp::new(cc, dirs.clone(), settings)?))),
    )
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}
