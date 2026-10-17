//! The chat list column (milestone **M10**).
//!
//! Reads chats from the local store via the app's cache ([`SquadsApp::chats`]),
//! shows the name, preview, last-activity time and unread badge, and flags a
//! conversation whose newest cached messages include a pending or failed send.
//! Selecting a row queues [`Action::SelectChat`].

use egui::{RichText, Ui};

use crate::app::{Action, SquadsApp, short_time};

/// Render the chat list (used by the Chats and Files pages).
pub fn show(app: &mut SquadsApp, ui: &mut Ui) {
    egui::Panel::left("chat_list")
        .default_size(280.0)
        .min_size(180.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let label = ui.label("Search chats");
                let mut search = app.search().to_string();
                let response = ui.add(
                    egui::TextEdit::singleline(&mut search)
                        .hint_text("Search")
                        .desired_width(f32::INFINITY),
                );
                let response = response.labelled_by(label.id);
                if response.changed() {
                    app.set_search(search);
                }
            });
            if ui.button("Refresh chats").clicked() {
                app.push_action(Action::RefreshChats);
            }
            ui.separator();

            let filter = app.search().to_lowercase();
            let chats: Vec<_> = app
                .chats()
                .iter()
                .filter(|chat| filter.is_empty() || chat.name.to_lowercase().contains(&filter))
                .cloned()
                .collect();

            if chats.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new("No chats cached yet.").weak());
                return;
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                for chat in chats {
                    row(app, ui, &chat);
                }
            });
        });
}

fn row(app: &mut SquadsApp, ui: &mut Ui, chat: &squads_core::Chat) {
    let selected = app.selected_chat() == Some(chat.id.as_str());
    let unread = app.unread_count(&chat.id);
    let pending = app.chat_pending(&chat.id);
    let failed = app.chat_failed(&chat.id);
    let preview = app.chat_preview(chat);
    let time = chat
        .last_message_time
        .as_deref()
        .map(short_time)
        .unwrap_or_default();

    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            let mut name = RichText::new(&chat.name);
            if unread > 0 {
                name = name.strong();
            }
            let response = ui.selectable_label(selected, name);
            if response.clicked() {
                app.push_action(Action::SelectChat(chat.id.clone()));
            }
            if !preview.is_empty() {
                ui.label(RichText::new(preview).weak().small());
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            if failed {
                ui.colored_label(app.palette().failed, "!");
            } else if pending {
                ui.colored_label(app.palette().pending, "…");
            }
            if unread > 0 {
                ui.label(
                    RichText::new(unread.to_string())
                        .strong()
                        .color(app.palette().accent),
                );
            }
            if !time.is_empty() {
                ui.label(RichText::new(time).weak().small());
            }
        });
    });
    ui.separator();
}
