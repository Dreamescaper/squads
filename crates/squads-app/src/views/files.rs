//! The files browser (milestone **M10**, global search **B3**).
//!
//! By default it shows the files shared in the selected chat/channel (from the
//! store's cache, refreshed through
//! [`Files::list_chat_files`](squads_core::Files::list_chat_files)) with name,
//! size and last-modified date, plus a **Download** action wired to
//! [`Files::download`](squads_core::Files::download).
//!
//! A **search box** at the top switches the page to **global drive search**
//! (backlog item **B3**): typing a query queues
//! [`Action::SearchFiles`](crate::app::Action::SearchFiles), which runs
//! [`Files::search_files`](squads_core::Files::search_files) and renders the
//! driveItems as the **same rows** (so a result is downloadable too). Clearing
//! the query returns to the chat's files. The search is single-window (Graph
//! caps at 25 hits), so the list is one bounded page.

use egui::{RichText, Ui};

use crate::app::{Action, SquadsApp, short_date};
use squads_core::FileEntry;

/// Render the files page: the global search results when a query is typed, else
/// the selected chat's shared files.
pub fn show(app: &mut SquadsApp, ui: &mut Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.heading("Shared files");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(chat_id) = app.selected_chat().map(str::to_string)
                    && ui.button("Refresh").clicked()
                {
                    app.push_action(Action::RefreshFiles(chat_id));
                }
            });
        });

        // The global drive search box (B3). Its label is distinct from the
        // nav-rail "Files" so the headless tests can address it unambiguously.
        ui.horizontal(|ui| {
            let label = ui.label("Search files");
            let response = ui.add(
                egui::TextEdit::singleline(app.file_search_query_mut())
                    .hint_text("Search all OneDrive files…")
                    .desired_width((ui.available_width() - 96.0).max(120.0)),
            );
            let response = response.labelled_by(label.id);
            if response.changed() {
                let query = app.file_search_query().to_string();
                app.push_action(Action::SearchFiles(query));
            }
        });
        ui.separator();

        // A typed query switches to the global results, chat or not.
        if !app.file_search_query().trim().is_empty() {
            show_search_results(app, ui);
            return;
        }

        let Some(chat_id) = app.selected_chat().map(str::to_string) else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Select a chat to see its files.").weak());
            });
            return;
        };

        let files = app.files().to_vec();
        if files.is_empty() {
            ui.label(RichText::new("No files cached for this chat.").weak());
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            for file in files {
                file_row(app, ui, &file, &chat_id);
            }
        });
    });
}

/// The global drive search results (or its loading/no-results states).
fn show_search_results(app: &mut SquadsApp, ui: &mut Ui) {
    if app.file_search_loading() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new("Searching OneDrive…").weak());
        });
    }

    let results = app.file_search_results().to_vec();
    if results.is_empty() {
        if !app.file_search_loading() {
            ui.label(RichText::new("No files match.").weak());
        }
        return;
    }

    ui.label(
        RichText::new(format!("{} result(s)", results.len()))
            .weak()
            .small(),
    );
    // A search hit has no owning chat; the download action resolves it from the
    // in-memory result list.
    let chat_id = app.selected_chat().unwrap_or_default().to_string();
    egui::ScrollArea::vertical().show(ui, |ui| {
        for file in results {
            file_row(app, ui, &file, &chat_id);
        }
    });
}

/// One file row (shared by the chat files and the global search results): the
/// kind, name, size and modified date, with a Download action. Folders and
/// drive-less attachments cannot be downloaded.
fn file_row(app: &mut SquadsApp, ui: &mut Ui, file: &FileEntry, chat_id: &str) {
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
                    chat_id: chat_id.to_string(),
                    file_id: file.id.clone(),
                });
            }
        });
    });
    ui.separator();
}

/// Format a byte count for humans (`1.5 MiB`).
pub(crate) fn human_size(bytes: u64) -> String {
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
