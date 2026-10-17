//! The files browser (milestone **M10**).
//!
//! Shows the files shared in the selected chat/channel (from the store's cache,
//! refreshed through [`Files::list_chat_files`](squads_core::Files::list_chat_files))
//! with name, size and last-modified date, plus a **Download** action wired to
//! [`Files::download`](squads_core::Files::download).

use egui::{RichText, Ui};

use crate::app::{Action, SquadsApp, short_date};

/// Render the files page for the selected chat/channel.
pub fn show(app: &mut SquadsApp, ui: &mut Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        let Some(chat_id) = app.selected_chat().map(str::to_string) else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Select a chat to see its files.").weak());
            });
            return;
        };

        ui.horizontal(|ui| {
            ui.heading("Shared files");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Refresh").clicked() {
                    app.push_action(Action::RefreshFiles(chat_id.clone()));
                }
            });
        });
        ui.separator();

        let files = app.files().to_vec();
        if files.is_empty() {
            ui.label(RichText::new("No files cached for this chat.").weak());
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            for file in files {
                ui.horizontal(|ui| {
                    let kind = if file.is_folder { "[dir]" } else { "[file]" };
                    ui.label(RichText::new(kind).weak().monospace());
                    ui.label(file.name.as_str());
                    ui.label(RichText::new(human_size(file.size)).weak().small());
                    if let Some(modified) = &file.modified {
                        ui.label(RichText::new(short_date(modified)).weak().small());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let can_download = !file.is_folder && file.drive_id.is_some();
                        if ui
                            .add_enabled(can_download, egui::Button::new("Download"))
                            .clicked()
                        {
                            app.push_action(Action::DownloadFile {
                                chat_id: chat_id.clone(),
                                file_id: file.id.clone(),
                            });
                        }
                    });
                });
                ui.separator();
            }
        });
    });
}

/// Format a byte count for humans (`1.5 MiB`).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
