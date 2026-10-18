//! The chat list column (milestone **M10**, extended by backlog item **B2a**).
//!
//! Reads chats from the local store via the app's cache ([`TeamTrimApp::chats`]),
//! shows the name, preview, last-activity time and unread badge, and flags a
//! conversation whose newest cached messages include a pending or failed send.
//! Selecting a row queues [`Action::SelectChat`].
//!
//! **B2a** adds the chat-management surface:
//!
//! * a **New chat** button that opens the people picker in "new chat" mode
//!   ([`Action::NewChat`]) — one person creates a 1:1, several a group;
//! * a **"⋯" options** affordance per row ([`Action::ToggleChatMenu`]) that
//!   reveals Mute/Unmute, Hide and Leave (Leave asks for confirmation first);
//! * **muted** rows carry a small badge, **hidden** rows are collapsed out of
//!   the list and can be shown again with the "Show hidden (N)" toggle.
//!
//! **B2b** adds the **folder tabs**: an "All" tab plus one tab per cached
//! folder (with the number of chats it holds), which filters the list
//! ([`Action::SelectFolder`]); and a **Move to folder** row in the context menu
//! ([`Action::MoveChatToFolder`]).

use egui::{RichText, Ui};

use crate::app::{Action, TeamTrimApp, short_time};

/// Render the chat list (used by the Chats and Files pages).
pub fn show(app: &mut TeamTrimApp, ui: &mut Ui) {
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
            ui.horizontal(|ui| {
                if ui.button("New chat").clicked() {
                    app.push_action(Action::NewChat);
                }
                if ui.button("Refresh chats").clicked() {
                    app.push_action(Action::RefreshChats);
                }
                if ui.button("Refresh folders").clicked() {
                    app.push_action(Action::RefreshFolders);
                }
            });
            // Folder tabs (B2b): "All" plus one per cached folder, with counts.
            folder_tabs(app, ui);
            // Hidden chats are collapsed; this re-shows them (B2a).
            if app.hidden_count() > 0 {
                let label = if app.show_hidden() {
                    format!("Hide hidden ({})", app.hidden_count())
                } else {
                    format!("Show hidden ({})", app.hidden_count())
                };
                if ui.button(label).clicked() {
                    app.push_action(Action::ToggleShowHidden);
                }
            }
            ui.separator();

            let filter = app.search().to_lowercase();
            let matches = |chat: &teamtrim_core::Chat| {
                filter.is_empty() || chat.name.to_lowercase().contains(&filter)
            };
            let visible: Vec<_> = app
                .chats()
                .iter()
                .filter(|chat| {
                    !app.is_chat_hidden(&chat.id)
                        && matches(chat)
                        && app.chat_in_selected_folder(&chat.id)
                })
                .cloned()
                .collect();
            // Hidden rows are only listed when the section is expanded.
            let hidden: Vec<_> = if app.show_hidden() {
                app.chats()
                    .iter()
                    .filter(|chat| app.is_chat_hidden(&chat.id) && matches(chat))
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            };

            if visible.is_empty() && hidden.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new("No chats cached yet.").weak());
                return;
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                for chat in &visible {
                    row(app, ui, chat);
                }
                if !hidden.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new("Hidden").weak().small());
                    for chat in &hidden {
                        row(app, ui, chat);
                    }
                }
            });
        });
}

/// The folder tab strip (B2b): "All" plus one tab per cached folder, with the
/// number of chats each holds. Selecting a tab filters the list
/// ([`Action::SelectFolder`]).
fn folder_tabs(app: &mut TeamTrimApp, ui: &mut Ui) {
    let folders: Vec<(String, String, usize)> = app
        .folders()
        .iter()
        .map(|folder| {
            (
                folder.id.clone(),
                folder.name.clone(),
                app.folder_count(&folder.id),
            )
        })
        .collect();
    if folders.is_empty() {
        return;
    }
    let selected = app.selected_folder().map(str::to_string);
    ui.horizontal_wrapped(|ui| {
        let all_selected = selected.is_none();
        if ui.selectable_label(all_selected, "All").clicked() {
            app.push_action(Action::SelectFolder(String::new()));
        }
        for (id, name, count) in folders {
            let is_selected = selected.as_deref() == Some(id.as_str());
            let label = format!("{name} ({count})");
            if ui.selectable_label(is_selected, label).clicked() {
                app.push_action(Action::SelectFolder(id));
            }
        }
    });
    ui.separator();
}

fn row(app: &mut TeamTrimApp, ui: &mut Ui, chat: &teamtrim_core::Chat) {
    let selected = app.selected_chat() == Some(chat.id.as_str());
    let unread = app.unread_count(&chat.id);
    let pending = app.chat_pending(&chat.id);
    let failed = app.chat_failed(&chat.id);
    let muted = app.is_chat_muted(&chat.id);
    let hidden = app.is_chat_hidden(&chat.id);
    let menu_open = app.chat_menu() == Some(chat.id.as_str());
    let confirming = app.leave_confirm() == Some(chat.id.as_str());
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
            if muted {
                ui.label(RichText::new("muted").weak().small());
            }
            if !preview.is_empty() {
                ui.label(RichText::new(preview).weak().small());
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            // The per-row options affordance opens the context menu (B2a).
            if ui.button("...").on_hover_text("Chat options").clicked() {
                app.push_action(Action::ToggleChatMenu(chat.id.clone()));
            }
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

    if menu_open {
        chat_menu(app, ui, chat, muted, hidden);
    }
    if confirming {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Leave this chat?").weak().small());
            if ui.button("Confirm leave").clicked() {
                app.push_action(Action::ConfirmLeaveChat(chat.id.clone()));
            }
            if ui.button("Cancel").clicked() {
                app.push_action(Action::CancelLeaveChat);
            }
        });
    }
    ui.separator();
}

/// The inline context menu for one chat (only rendered while open).
fn chat_menu(
    app: &mut TeamTrimApp,
    ui: &mut Ui,
    chat: &teamtrim_core::Chat,
    muted: bool,
    hidden: bool,
) {
    ui.horizontal(|ui| {
        let mute_label = if muted { "Unmute" } else { "Mute" };
        if ui.button(mute_label).clicked() {
            app.push_action(Action::ToggleChatMuted(chat.id.clone()));
        }
        let hide_label = if hidden { "Unhide" } else { "Hide" };
        if ui.button(hide_label).clicked() {
            app.push_action(Action::ToggleChatHidden(chat.id.clone()));
        }
        if ui.button("Leave").clicked() {
            app.push_action(Action::RequestLeaveChat(chat.id.clone()));
        }
    });
    // Move to folder (B2b): one button per cached folder plus "No folder".
    if !app.folders().is_empty() {
        let folders: Vec<(String, String)> = app
            .folders()
            .iter()
            .map(|folder| (folder.id.clone(), folder.name.clone()))
            .collect();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Move to:").weak().small());
            for (id, name) in folders {
                if ui.button(name).clicked() {
                    app.push_action(Action::MoveChatToFolder {
                        chat_id: chat.id.clone(),
                        folder_id: id,
                    });
                }
            }
            if ui.button("No folder").clicked() {
                app.push_action(Action::MoveChatToFolder {
                    chat_id: chat.id.clone(),
                    folder_id: String::new(),
                });
            }
        });
    }
}
