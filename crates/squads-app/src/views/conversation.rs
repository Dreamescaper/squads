//! The conversation: virtualized message timeline + composer (milestone **M10**).
//!
//! The timeline is rendered with [`egui::ScrollArea::show_rows`], so only the
//! visible rows are laid out — the "virtualized list" the brief asks for. A
//! "load older" affordance is shown while the service reports a backward page.
//!
//! Message bubbles render the sender, time, edited flag, reply context,
//! reactions and the optimistic [`SendState`] styling (pending / failed with a
//! retry button).

use std::path::Path;

use egui::{RichText, Ui};
use squads_core::SendState;
use squads_core::model::Message;
use squads_core::text::image_urls;

use crate::app::{Action, MediaState, SquadsApp, short_time};
use crate::views::composer;

/// The largest an inline image is drawn, regardless of its natural size.
const MAX_IMAGE_WIDTH: f32 = 320.0;
const MAX_IMAGE_HEIGHT: f32 = 320.0;

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
        if app.timeline_search_open() {
            egui::Panel::top("conversation_search").show(ui, |ui| {
                ui.horizontal(|ui| {
                    let label = ui.label("Search messages");
                    let mut query = app.timeline_filter().to_string();
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut query)
                            .hint_text("Search in conversation")
                            .desired_width(ui.available_width() - 96.0),
                    );
                    let response = response.labelled_by(label.id);
                    if app.take_timeline_search_focus() {
                        response.request_focus();
                    }
                    if response.changed() {
                        app.set_timeline_filter(query);
                    }
                    if ui.button("Close search").clicked() {
                        app.push_action(Action::CloseOverlays);
                    }
                });
            });
        }
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

        if app.has_older() && ui.button("Load older messages").clicked() {
            app.push_action(Action::LoadOlder(chat_id.clone()));
        }

        let filter = app.timeline_filter().to_lowercase();
        let timeline: Vec<Message> = app
            .timeline()
            .iter()
            .filter(|message| filter.is_empty() || message.content.to_lowercase().contains(&filter))
            .cloned()
            .collect();
        if timeline.is_empty() {
            ui.add_space(12.0);
            let text = if filter.is_empty() {
                "No messages cached yet."
            } else {
                "No messages match the search."
            };
            ui.label(RichText::new(text).weak());
        }
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show_rows(ui, app.row_height(), timeline.len(), |ui, range| {
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
                        if app.highlight_message() == Some(message.id.as_str()) {
                            ui.label(
                                RichText::new("match")
                                    .small()
                                    .strong()
                                    .color(app.palette().accent),
                            );
                        }
                    });
                    if let Some(parent) = &message.reply_to {
                        ui.label(RichText::new(format!("reply to {parent}")).weak().small());
                    }
                    if !message.content.is_empty() {
                        ui.label(&message.content);
                    }
                    // Inline images / GIFs (M13): extract from the raw HTML and
                    // render each cached bitmap (or a placeholder while it
                    // loads).
                    for url in image_urls(&message.raw) {
                        inline_image(app, ui, &url);
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

/// Render one inline image URL (M13): the cached bitmap, a labelled
/// placeholder while it is fetched, or a small note when it failed.
///
/// The fetch itself never happens here — an unknown URL queues
/// [`Action::LoadMedia`] and shows the placeholder; `apply_actions` performs
/// the fetch off the frame (ADR-026).
fn inline_image(app: &mut SquadsApp, ui: &mut Ui, url: &str) {
    match app.media_state(url) {
        Some(MediaState::Loaded(path)) => {
            ui.add(
                egui::Image::from_uri(file_uri(&path))
                    .max_width(MAX_IMAGE_WIDTH)
                    .max_height(MAX_IMAGE_HEIGHT),
            );
        }
        Some(MediaState::Failed) => {
            ui.label(RichText::new("Image unavailable").weak().small());
        }
        Some(MediaState::Loading) => {
            ui.label(RichText::new("Loading image...").weak().small());
        }
        None => {
            app.request_media(url);
            ui.label(RichText::new("Loading image...").weak().small());
        }
    }
}

/// A `file://` URI for a local path egui's image loader can open.
///
/// egui's loader expects a leading `/` after the scheme (a POSIX path already
/// has one; a Windows path is normalised to forward slashes and prefixed).
fn file_uri(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('\\', "/");
    if !text.starts_with('/') {
        text.insert(0, '/');
    }
    format!("file://{text}")
}
