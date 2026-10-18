//! The global message-search page (backlog item **B1**).
//!
//! A **local-first** search over the cached history: typing a query runs a
//! synchronous, offline `LIKE` scan through
//! [`Search::local`](teamtrim_core::Search::local) (queued as
//! [`Action::SearchMessages`]) and the results render immediately. When the
//! local store has nothing and the session is signed in, the app additionally
//! asks the server (Graph `/search/query`) as an **optional** fallback — that
//! path needs `Chat.Read`, which the Teams token does not carry, so it is
//! expected to 403 live (see ADR-036).
//!
//! Each row shows the snippet (the match is centred), the chat name, the sender
//! and the time. Clicking a row (or pressing Enter) queues
//! [`Action::OpenSearchHit`], which opens the chat and highlights the message;
//! `Up`/`Down` (or `j`/`k`) move the selection. The view only records intent —
//! [`crate::app::TeamTrimApp::apply_actions`] runs after the frame.

use egui::{Key, RichText, Ui};

use crate::app::{Action, TeamTrimApp, short_time};
use teamtrim_core::model::SearchHit;

/// Render the global message-search page.
pub fn show(app: &mut TeamTrimApp, ui: &mut Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.heading("Message search");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // The reusable people picker (B3) is reachable from here until
                // B2a wires it into the new-chat flow.
                if ui.button("Find people").clicked() {
                    app.push_action(Action::TogglePeoplePicker);
                }
            });
        });
        ui.label(
            RichText::new("Search your cached messages. Results open the conversation and highlight the match.")
                .weak()
                .small(),
        );
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            let label = ui.label("Search messages");
            let response = ui.add(
                egui::TextEdit::singleline(app.search_query_mut())
                    .hint_text("Search all messages…")
                    .desired_width((ui.available_width() - 96.0).max(120.0)),
            );
            let response = response.labelled_by(label.id);
            if app.take_search_focus() {
                response.request_focus();
            }
            if response.changed() {
                let query = app.search_query().to_string();
                app.push_action(Action::SearchMessages(query));
            }
            // Enter in the box opens the selected result (a single-line field
            // gives up focus on Enter).
            if response.lost_focus()
                && ui.input(|input| input.key_pressed(Key::Enter))
                && let Some(hit) = app.search_results().get(app.search_selected()).cloned()
            {
                app.push_action(Action::OpenSearchHit {
                    chat_id: hit.chat_id,
                    message_id: hit.message_id,
                });
            }
        });

        ui.add_space(8.0);

        if app.search_query().trim().is_empty() {
            ui.label(RichText::new("Type a query to search your cached messages.").weak());
            return;
        }
        if app.search_loading() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Searching the server…").weak());
            });
        }

        let results = app.search_results().to_vec();
        if results.is_empty() {
            if !app.search_loading() {
                ui.label(RichText::new("No messages match.").weak());
            }
            return;
        }

        ui.label(
            RichText::new(format!("{} result(s)", results.len()))
                .weak()
                .small(),
        );
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (index, hit) in results.iter().enumerate() {
                result_row(app, ui, hit, index);
            }
        });
    });
}

/// One result row: the snippet (clickable, labelled by the snippet so it is
/// findable by AccessKit), then the chat name and sender, with the time on the
/// right. The selected row is rendered selected for keyboard navigation.
fn result_row(app: &mut TeamTrimApp, ui: &mut Ui, hit: &SearchHit, index: usize) {
    let selected = app.search_selected() == index;
    ui.push_id(("search_hit", index), |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                let response = ui.selectable_label(selected, &hit.snippet);
                if response.clicked() {
                    app.push_action(Action::OpenSearchHit {
                        chat_id: hit.chat_id.clone(),
                        message_id: hit.message_id.clone(),
                    });
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&hit.chat_name).weak().small());
                    ui.label(RichText::new("·").weak().small());
                    ui.label(RichText::new(&hit.sender).weak().small());
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                ui.label(RichText::new(short_time(&hit.timestamp)).weak().small());
            });
        });
        ui.separator();
    });
}
