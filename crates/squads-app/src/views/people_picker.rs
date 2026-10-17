//! The reusable **people picker** overlay (backlog item **B3**).
//!
//! A self-contained directory search + multi-select component. It is written so
//! that **B2a**'s "new chat" flow can consume it unchanged:
//!
//! * the *state* lives on [`SquadsApp`](crate::app::SquadsApp)
//!   ([`people_query`](crate::app::SquadsApp::people_query),
//!   [`people_results`](crate::app::SquadsApp::people_results),
//!   [`people_selected`](crate::app::SquadsApp::people_selected));
//! * the *intents* are [`Action`]s (`SearchPeople`, `TogglePersonSelected`,
//!   `ClearPeopleSelection`, `TogglePeoplePicker`);
//! * the *result* a caller reads is
//!   [`SquadsApp::selected_people`](crate::app::SquadsApp::selected_people).
//!
//! B2a therefore: opens the picker (queue [`Action::TogglePeoplePicker`] or a
//! dedicated "new chat" button), lets the user search/pick, then reads
//! `app.selected_people()` — one person → a 1:1 chat, several → a group chat —
//! and clears with [`Action::ClearPeopleSelection`]. Nothing here talks to the
//! network: typing queues [`Action::SearchPeople`] and the results arrive as
//! [`AppEvent::PeopleResults`](crate::runtime::AppEvent::PeopleResults).
//!
//! The search is **Graph** and single-window (Graph caps at 25 people), so the
//! list is a single bounded page rather than an infinite scroll.
//!
//! The overlay is a no-op while closed, so it is safe to call every frame. It is
//! reachable now from the **Search** page's "Find people" button (and via
//! [`Action::TogglePeoplePicker`]), which is what exercises it until B2a wires
//! it into the new-chat flow.

use egui::{Align2, Context, RichText, TextEdit, Window};

use crate::app::{Action, SquadsApp};

/// Render the overlay (no-op when closed).
pub fn show(app: &mut SquadsApp, ctx: &Context) {
    if !app.people_picker_open() {
        return;
    }

    let mut toggle: Option<squads_core::model::Person> = None;
    let mut clear = false;
    let mut close = false;

    Window::new("People picker")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, [0.0, 96.0])
        .default_width(420.0)
        .show(ctx, |ui| {
            ui.label(
                RichText::new("Find people to start a chat with.")
                    .weak()
                    .small(),
            );

            let label = ui.label("Search people");
            let response = ui.add(
                TextEdit::singleline(app.people_query_mut())
                    .hint_text("Search by name…")
                    .desired_width(f32::INFINITY),
            );
            let response = response.labelled_by(label.id);
            if app.take_people_focus() {
                response.request_focus();
            }
            if response.changed() {
                let query = app.people_query().to_string();
                app.push_action(Action::SearchPeople(query));
            }

            // The picked people, each removable.
            let selected = app.selected_people().to_vec();
            if !selected.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new("Selected").weak().small());
                ui.horizontal_wrapped(|ui| {
                    for person in &selected {
                        if ui
                            .button(format!("Remove {}", person.display_name))
                            .clicked()
                        {
                            toggle = Some(person.clone());
                        }
                    }
                });
            }

            ui.add_space(4.0);
            if app.people_loading() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Searching the directory…").weak());
                });
            }

            let results = app.people_results().to_vec();
            if results.is_empty() {
                if !app.people_loading() && !app.people_query().trim().is_empty() {
                    ui.label(RichText::new("No people match.").weak());
                }
            } else {
                ui.separator();
                for person in &results {
                    let picked = app.is_person_selected(&person.id);
                    let text = person.label();
                    if ui.selectable_label(picked, text).clicked() {
                        toggle = Some(person.clone());
                    }
                }
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Clear").clicked() {
                    clear = true;
                }
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        });

    if let Some(person) = toggle {
        app.push_action(Action::TogglePersonSelected(person));
    }
    if clear {
        app.push_action(Action::ClearPeopleSelection);
    }
    if close {
        app.push_action(Action::CloseOverlays);
    }
}
