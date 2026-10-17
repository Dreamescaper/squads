//! The signed-in shell: nav rail, list column and central page (milestone **M10**).
//!
//! Layout:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │ top bar: title, version, sign-out                            │
//! ├────┬───────────────┬─────────────────────────────────────────┤
//! │nav │ list column   │ central page (conversation / teams / …)  │
//! │rail│ (chats/files) │                                          │
//! └────┴───────────────┴─────────────────────────────────────────┘
//! ```
//!
//! The nav rail selects the [`Page`]; the list column and the central page are
//! delegated to the per-screen modules.

use egui::{RichText, Ui};

use crate::app::{Action, Page, SquadsApp};
use crate::views;

/// Render the shell.
pub fn show(app: &mut SquadsApp, ui: &mut Ui) {
    egui::Panel::top("top_bar").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.heading("Squads");
            ui.label(RichText::new(env!("CARGO_PKG_VERSION")).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Sign out").clicked() {
                    app.push_action(Action::SignOut);
                }
                ui.weak(app.profile());
            });
        });
    });

    egui::Panel::left("nav_rail")
        .exact_size(76.0)
        .resizable(false)
        .show(ui, |ui| {
            ui.add_space(6.0);
            ui.vertical_centered(|ui| {
                for page in Page::all() {
                    let selected = app.page() == page;
                    if ui.selectable_label(selected, page.label()).clicked() {
                        app.push_action(Action::SetPage(page));
                    }
                    ui.add_space(4.0);
                }
            });
        });

    match app.page() {
        Page::Chats => {
            views::chat_list::show(app, ui);
            views::conversation::show(app, ui);
        }
        Page::Calendar => views::calendar::show(app, ui),
        Page::Teams => views::teams::show(app, ui),
        Page::Files => {
            views::chat_list::show(app, ui);
            views::files::show(app, ui);
        }
        Page::Calls => views::call::show(app, ui),
        Page::Settings => views::settings::show(app, ui),
    }
}
