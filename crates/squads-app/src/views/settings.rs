//! The settings page (milestone **M10**).
//!
//! Theme selection (persisted through [`Settings`](squads_core::Settings)),
//! the account/profile display and **Sign out**.

use egui::{RichText, Ui};
use squads_core::Theme;

use crate::app::{Action, SquadsApp};

/// Render the settings page.
pub fn show(app: &mut SquadsApp, ui: &mut Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.heading("Preferences");
        ui.separator();

        ui.label("Theme");
        let current = app.theme();
        ui.horizontal(|ui| {
            for theme in [Theme::System, Theme::Light, Theme::Dark] {
                if ui
                    .selectable_label(current == theme, theme_label(theme))
                    .clicked()
                {
                    app.push_action(Action::SetTheme(theme));
                }
            }
        });

        ui.add_space(16.0);
        ui.separator();
        ui.heading("Account");
        ui.label(format!("Profile: {}", app.profile()));
        if app.is_signed_in() {
            ui.label("Signed in.");
        } else {
            ui.label("Signed out.");
        }
        ui.add_space(8.0);
        if ui.button("Sign out").clicked() {
            app.push_action(Action::SignOut);
        }

        ui.add_space(16.0);
        ui.separator();
        ui.label(RichText::new(format!("Squads {}", env!("CARGO_PKG_VERSION"))).weak());
    });
}

fn theme_label(theme: Theme) -> &'static str {
    match theme {
        Theme::System => "System",
        Theme::Light => "Light",
        Theme::Dark => "Dark",
    }
}
