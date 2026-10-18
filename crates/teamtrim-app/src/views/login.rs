//! The signed-out login screen (milestone **M10**).
//!
//! Drives [`Session::begin_sign_in`](teamtrim_core::Session::begin_sign_in) (the
//! device-code flow, ADR-009): the user picks work/school or personal, presses
//! **Sign in**, and the screen shows the AAD device code plus the verification
//! URL. The code is delivered by
//! [`Session::poll_sign_in`](teamtrim_core::Session::poll_sign_in), which the app
//! polls from `logic` (see [`crate::app`]).
//!
//! All controls carry AccessKit labels so the headless tests can find them
//! (`get_by_label("Sign in")`, …).

use egui::{RichText, Ui};

use crate::app::{Action, TeamTrimApp};

/// Render the login screen.
pub fn show(app: &mut TeamTrimApp, ui: &mut Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(64.0);
            ui.heading("TeamTrim");
            ui.label("A cross-platform Microsoft Teams client in Rust.");
            ui.add_space(28.0);

            if app.signing_in() {
                signing_in(app, ui);
            } else {
                form(app, ui);
            }

            ui.add_space(40.0);
            ui.separator();
            ui.add_space(8.0);
            ui.label(
                RichText::new("Sign-in uses the Microsoft device-code flow.")
                    .weak()
                    .small(),
            );
        });
    });
}

fn form(app: &mut TeamTrimApp, ui: &mut Ui) {
    let mut personal = app.use_personal();
    if ui
        .checkbox(&mut personal, "Use a personal account")
        .changed()
    {
        app.set_use_personal(personal);
    }
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        if ui
            .button("Sign in")
            .on_hover_text("Shows a code to enter at microsoft.com/devicelogin")
            .clicked()
        {
            app.push_action(Action::BeginSignIn {
                use_personal: personal,
            });
        }
        if ui
            .button("Sign in with browser")
            .on_hover_text("Opens your browser: no code to type (PKCE loopback)")
            .clicked()
        {
            app.push_action(Action::BeginBrowserSignIn {
                use_personal: personal,
            });
        }
    });
    if let Some(error) = app.login().error.clone() {
        ui.add_space(12.0);
        ui.colored_label(app.palette().failed, error);
    }
}

fn signing_in(app: &mut TeamTrimApp, ui: &mut Ui) {
    let hint = app.login().hint.clone();
    if !hint.is_empty() {
        ui.label(&hint);
    }
    ui.add_space(12.0);
    if let Some(code) = app.login().user_code.clone() {
        ui.label("Enter this code at Microsoft:");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(&code)
                        .heading()
                        .monospace()
                        .color(app.palette().accent),
                )
                .selectable(true),
            );
            if ui.button("Copy code").clicked() {
                ui.ctx().copy_text(code.clone());
                app.copied_feedback("Device code");
            }
        });
        if let Some(url) = app.login().verification_uri.clone() {
            ui.add_space(6.0);
            if ui
                .button(format!("Open {url}"))
                .on_hover_text("Opens the Microsoft sign-in page in your browser")
                .clicked()
            {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url.clone()));
            }
            ui.add_space(2.0);
            ui.hyperlink_to(&url, &url);
        }
        let message = app.login().message.clone();
        if !message.is_empty() {
            ui.add_space(6.0);
            ui.label(RichText::new(message).weak());
        }
    } else if let Some(url) = app.login().browser_url.clone() {
        browser_waiting(app, ui, &url);
    } else {
        ui.spinner();
    }
    if let Some(error) = app.login().error.clone() {
        ui.add_space(12.0);
        ui.colored_label(app.palette().failed, error);
    }
}

/// The browser (PKCE loopback) half of the sign-in screen (B-004).
fn browser_waiting(app: &mut TeamTrimApp, ui: &mut Ui, url: &str) {
    ui.label("Finish signing in in your browser; this window updates by itself.");
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.button("Open browser again").clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url.to_string()));
        }
        if ui.button("Copy link").clicked() {
            ui.ctx().copy_text(url.to_string());
            app.copied_feedback("Sign-in link");
        }
    });
    ui.add_space(2.0);
    ui.hyperlink_to(url, url);
}
