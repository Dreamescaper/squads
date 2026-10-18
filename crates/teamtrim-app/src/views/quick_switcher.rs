//! The quick chat switcher overlay (milestone **M11**).
//!
//! Opened with `Ctrl/Cmd+K` (or the [`crate::app::Action::ToggleQuickSwitcher`]
//! action), it shows a single filter box and the matching cached chats; clicking
//! a row (or pressing Enter for the first match) selects it and closes the
//! overlay. `Esc` closes it through [`crate::app::Action::CloseOverlays`].
//!
//! It is a no-op while closed, so it is safe to call every frame.

use egui::{Align2, Context, Key, RichText, TextEdit, Window};

use crate::app::{Action, TeamTrimApp};

/// How many matches the switcher lists.
const MAX_MATCHES: usize = 8;

/// Render the overlay (no-op when closed).
pub fn show(app: &mut TeamTrimApp, ctx: &Context) {
    if !app.quick_switcher_open() {
        return;
    }

    let mut query = app.quick_switcher_query().to_string();
    let mut chosen: Option<String> = None;
    let mut close = false;

    Window::new("Quick switcher")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, [0.0, 96.0])
        .default_width(360.0)
        .show(ctx, |ui| {
            let label = ui.label("Quick switcher");
            let response = ui.add(
                TextEdit::singleline(&mut query)
                    .hint_text("Jump to a chat…")
                    .desired_width(f32::INFINITY),
            );
            let response = response.labelled_by(label.id);
            if app.take_quick_switcher_focus() {
                response.request_focus();
            }

            let filter = query.to_lowercase();
            let matches: Vec<(String, String)> = app
                .chats()
                .iter()
                .filter(|chat| filter.is_empty() || chat.name.to_lowercase().contains(&filter))
                .take(MAX_MATCHES)
                .map(|chat| (chat.id.clone(), chat.name.clone()))
                .collect();

            let enter = ui.input(|i| i.key_pressed(Key::Enter));

            ui.separator();
            if matches.is_empty() {
                ui.label(RichText::new("No matching chats.").weak());
            }
            for (id, name) in &matches {
                if ui.selectable_label(false, name).clicked() {
                    chosen = Some(id.clone());
                }
            }
            if enter && !matches.is_empty() {
                chosen = Some(matches[0].0.clone());
            }

            ui.add_space(4.0);
            if ui.button("Close").clicked() {
                close = true;
            }
        });

    if query != app.quick_switcher_query() {
        app.set_quick_switcher_query(query);
    }
    if let Some(id) = chosen {
        app.push_action(Action::SelectChat(id));
        app.push_action(Action::CloseOverlays);
    } else if close {
        app.push_action(Action::CloseOverlays);
    }
}
