//! Squads — an alternative Microsoft Teams client built with Rust + egui.
//!
//! Milestone M0 proves out the workspace: the process resolves its directories,
//! creates a [`squads_core::Session`] and opens a window that renders the
//! session state. Real screens (chat list, timeline, composer, …) arrive in M10.
//!
//! ## UI lifecycle (eframe 0.37)
//!
//! `eframe::App` exposes two phases:
//! - `logic(&mut self, ctx, frame)` — runs even when the window is not painting;
//!   use it for timers/polling background work.
//! - `ui(&mut self, ui, frame)` — the paint pass; compose panels and widgets here.
//!
//! Panels are attached to the [`egui::Ui`] handed to `ui` via `Panel::top/left/…`
//! and `CentralPanel`.
//!
//! ## CPU footprint
//!
//! egui is immediate-mode but only repaints on input or when something calls
//! [`egui::Context::request_repaint`]. This app deliberately never requests a
//! repaint on its own, so an idle window costs ~0% CPU. Keep that property:
//! request repaints only while something is actually animating.

use std::sync::Arc;

use eframe::egui;
use squads_core::{AppDirs, Session, SessionState};

fn main() -> eframe::Result<()> {
    init_tracing();

    let dirs = Arc::new(AppDirs::discover().expect("resolve app dirs"));
    dirs.ensure().expect("create app dirs");
    tracing::info!(config = %dirs.config.display(), data = %dirs.data.display(), "starting squads");

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Squads")
            .with_app_id("dev.squads.squads")
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([520.0, 400.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Squads",
        native_options,
        Box::new(move |cc| Ok(Box::new(SquadsApp::new(cc, dirs.clone())?))),
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

/// Root application state. Holds the core [`Session`]; the UI is a pure function
/// of it (plus local widget state) — mutations will flow through the core in M2.
struct SquadsApp {
    session: Session,
    dirs: Arc<AppDirs>,
}

impl SquadsApp {
    fn new(cc: &eframe::CreationContext<'_>, dirs: Arc<AppDirs>) -> anyhow::Result<Self> {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let session = Session::new(dirs.clone())?;
        Ok(Self { session, dirs })
    }

    fn session_state(&self) -> &SessionState {
        self.session.state()
    }

    fn status(&self) -> (&str, egui::Color32) {
        match self.session_state() {
            SessionState::Ready => ("Ready", egui::Color32::from_rgb(0x40, 0xc0, 0x57)),
            SessionState::Failed(_) => ("Error", egui::Color32::from_rgb(0xe0, 0x4f, 0x4f)),
            other => (other.label(), egui::Color32::from_rgb(0x4d, 0xa6, 0xff)),
        }
    }
}

impl eframe::App for SquadsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("titlebar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Squads");
                ui.label(egui::RichText::new(env!("CARGO_PKG_VERSION")).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (text, color) = self.status();
                    ui.colored_label(color, text);
                });
            });
        });

        egui::Panel::bottom("statusbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("config: {}", self.dirs.config.display())).weak(),
                );
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(48.0);
                ui.heading("Welcome to Squads");
                ui.add_space(8.0);
                ui.label("A cross-platform Microsoft Teams client in Rust.");
                ui.add_space(24.0);
                match self.session_state() {
                    SessionState::SigningIn { hint } => {
                        ui.label(format!("Signing in… {hint}"));
                    }
                    other => {
                        ui.label(egui::RichText::new(format!("Session: {}", other.label())).weak());
                    }
                }
                ui.add_space(32.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "M0 scaffolding — authentication (M1) and the UI (M10) land next.",
                    )
                    .weak()
                    .small(),
                );
            });
        });
    }
}
