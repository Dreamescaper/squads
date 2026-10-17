//! The conversation: virtualized message timeline + composer (milestone **M10**).
//!
//! The timeline is rendered with [`egui::ScrollArea::show_rows`], so only the
//! visible rows are laid out — the "virtualized list" the brief asks for. A
//! "load older" affordance is shown while the service reports a backward page.
//!
//! Message bubbles render the sender, time, edited flag, reply context,
//! reactions and the optimistic [`SendState`] styling (pending / failed with a
//! retry button).

use egui::{RichText, Ui};
use squads_core::SendState;
use squads_core::model::Message;

use crate::app::{Action, SquadsApp, short_time};
use crate::views::composer;

/// Fixed row height for the virtualized list. Message bubbles are single-line
/// in the default font at this size; longer bodies wrap within the row.
const ROW_HEIGHT: f32 = 78.0;

/// Render the conversation for the selected chat/channel.
pub fn show(app: &mut SquadsApp, ui: &mut Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        let Some(chat_id) = app.selected_chat().map(str::to_string) else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Select a chat to start.").weak());
            });
            return;
        };

        let name = app
            .chats()
            .iter()
            .find(|chat| chat.id == chat_id)
            .map(|chat| chat.name.clone())
            .unwrap_or_else(|| chat_id.clone());

        // Header, then the composer pinned to the bottom; the timeline fills
        // what is left. Panels must be added before the non-panel content.
        egui::Panel::top("conversation_header").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(name);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let open = app.show_info();
                    if ui.selectable_label(open, "Info").clicked() {
                        app.set_show_info(!open);
                    }
                });
            });
        });
        egui::Panel::bottom("composer_bar").show(ui, |ui| {
            ui.separator();
            composer::show(app, ui);
        });
        if app.show_info() {
            egui::Panel::right("conversation_info")
                .default_size(220.0)
                .show(ui, |ui| {
                    ui.heading("Details");
                    ui.separator();
                    if let Some(chat) = app.chats().iter().find(|chat| chat.id == chat_id) {
                        ui.label(format!("Name: {}", chat.name));
                        ui.label(format!("Group: {}", chat.is_group));
                        if let Some(last) = &chat.last_message_time {
                            ui.label(format!("Last activity: {last}"));
                        }
                    }
                    ui.label(format!("Chat id: {chat_id}"));
                    ui.label(format!("Unread: {}", app.unread_count(&chat_id)));
                });
        }

        if app.has_older() {
            if ui.button("Load older messages").clicked() {
                app.push_action(Action::LoadOlder(chat_id.clone()));
            }
        }

        let timeline = app.timeline().to_vec();
        if timeline.is_empty() {
            ui.add_space(12.0);
            ui.label(RichText::new("No messages cached yet.").weak());
        }
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show_rows(ui, ROW_HEIGHT, timeline.len(), |ui, range| {
                for index in range {
                    bubble(app, ui, &timeline[index], index);
                }
            });
    });
}

fn bubble(app: &mut SquadsApp, ui: &mut Ui, message: &Message, index: usize) {
    let own = message.is_own;
    let fill = if own {
        app.palette().own_bubble
    } else {
        app.palette().other_bubble
    };

    ui.push_id(index, |ui| {
        ui.horizontal(|ui| {
            if own {
                ui.add_space(48.0);
            }
            egui::Frame::group(ui.style()).fill(fill).show(ui, |ui| {
                ui.set_min_width(220.0);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.strong(&message.sender_name);
                        ui.label(RichText::new(short_time(&message.timestamp)).weak().small());
                        if message.edited {
                            ui.label(RichText::new("(edited)").weak().small());
                        }
                        match message.send_state {
                            SendState::Pending => {
                                ui.label(
                                    RichText::new("sending…")
                                        .small()
                                        .color(app.palette().pending),
                                );
                            }
                            SendState::Failed => {
                                ui.label(
                                    RichText::new("failed").small().color(app.palette().failed),
                                );
                                if ui.small_button("Retry").clicked() {
                                    let client_id = message
                                        .client_message_id
                                        .clone()
                                        .unwrap_or_else(|| message.id.clone());
                                    app.push_action(Action::RetryMessage {
                                        chat_id: message.chat_id.clone(),
                                        client_message_id: client_id,
                                    });
                                }
                            }
                            SendState::Sent => {}
                        }
                    });
                    if let Some(parent) = &message.reply_to {
                        ui.label(RichText::new(format!("reply to {parent}")).weak().small());
                    }
                    if !message.content.is_empty() {
                        ui.label(&message.content);
                    }
                    if !message.reactions.is_empty() {
                        ui.horizontal(|ui| {
                            for reaction in &message.reactions {
                                ui.label(
                                    RichText::new(format!("{} {}", reaction.emoji, reaction.count))
                                        .small(),
                                );
                            }
                        });
                    }
                });
            });
        });
    });
}
