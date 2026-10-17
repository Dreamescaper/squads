//! The settings page (milestone **M10**, expanded in **M11**).
//!
//! Theme and accent selection, UI font scale, compact density, notification
//! switches and the download directory — all persisted through
//! [`Settings`](squads_core::Settings) — plus the account/profile display and
//! **Sign out**.

use egui::{Checkbox, RichText, Slider, TextEdit, Ui};
use squads_core::settings::{FONT_SCALE_MAX, FONT_SCALE_MIN};
use squads_core::{Accent, Theme};

use crate::app::{Action, SquadsApp};

/// Render the settings page.
pub fn show(app: &mut SquadsApp, ui: &mut Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
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

            ui.add_space(8.0);
            ui.label("Accent");
            let accent = app.accent();
            ui.horizontal(|ui| {
                for choice in [Accent::Blue, Accent::Teal, Accent::Purple, Accent::Orange] {
                    if ui
                        .selectable_label(accent == choice, accent_label(choice))
                        .clicked()
                    {
                        app.push_action(Action::SetAccent(choice));
                    }
                }
            });

            ui.add_space(8.0);
            let mut scale = app.font_scale();
            if ui
                .add(
                    Slider::new(&mut scale, FONT_SCALE_MIN..=FONT_SCALE_MAX)
                        .text("Font scale")
                        .fixed_decimals(2),
                )
                .changed()
            {
                app.push_action(Action::SetFontScale(scale));
            }

            let mut compact = app.compact_mode();
            if ui.checkbox(&mut compact, "Compact mode").changed() {
                app.push_action(Action::SetCompactMode(compact));
            }

            ui.add_space(12.0);
            ui.separator();
            ui.heading("Notifications");
            let mut enabled = app.notifications_enabled();
            if ui.checkbox(&mut enabled, "Enable notifications").changed() {
                app.push_action(Action::SetNotificationsEnabled(enabled));
            }
            let mut mentions = app.notify_only_mentions();
            if ui
                .add_enabled(
                    enabled,
                    Checkbox::new(&mut mentions, "Notify only for mentions"),
                )
                .changed()
            {
                app.push_action(Action::SetNotifyOnlyMentions(mentions));
            }
            ui.label(
                RichText::new(
                    "Delivery uses a log notifier in this build; desktop toasts need the \
                     `desktop-notifications` feature (B-010).",
                )
                .weak()
                .small(),
            );

            ui.add_space(12.0);
            ui.separator();
            ui.heading("Files");
            let label = ui.label("Download folder");
            let mut dir = app.download_dir();
            let response = ui.add(
                TextEdit::singleline(&mut dir)
                    .hint_text("Default (app media cache)")
                    .desired_width(420.0),
            );
            let response = response.labelled_by(label.id);
            if response.changed() {
                app.push_action(Action::SetDownloadDir(dir));
            }

            ui.add_space(12.0);
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
    });
}

fn theme_label(theme: Theme) -> &'static str {
    match theme {
        Theme::System => "System",
        Theme::Light => "Light",
        Theme::Dark => "Dark",
    }
}

fn accent_label(accent: Accent) -> &'static str {
    match accent {
        Accent::Blue => "Blue",
        Accent::Teal => "Teal",
        Accent::Purple => "Purple",
        Accent::Orange => "Orange",
    }
}
