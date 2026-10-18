//! The message composer (milestone **M10**, Ctrl/Cmd+Enter in **M11**).
//!
//! A multiline input where **Enter sends** (and **Shift+Enter inserts a
//! newline**; **Ctrl/Cmd+Enter** also sends, matching the M11 shortcut), a
//! **Send** button (disabled while the draft is empty) and an
//! **Attach** button. Sending queues [`Action::SendMessage`], which the app
//! turns into an optimistic [`Messaging::send_text`](teamtrim_core::Messaging::send_text)
//! (ADR-014): the pending bubble appears immediately and is re-keyed to the
//! server id when the POST confirms.

use egui::Ui;

use crate::app::{Action, TeamTrimApp};

/// Render the composer for the selected chat/channel.
pub fn show(app: &mut TeamTrimApp, ui: &mut Ui) {
    let Some(chat_id) = app.selected_chat().map(str::to_string) else {
        return;
    };

    let mut attach = false;
    let mut send: Option<String> = None;

    ui.horizontal(|ui| {
        if ui.button("Attach").clicked() {
            attach = true;
        }
        let label = ui.label("Message");
        let empty = app.draft(&chat_id).trim().is_empty();
        let draft = app.draft_mut(&chat_id);
        let response = ui.add_sized(
            [ui.available_width() - 72.0, 48.0],
            egui::TextEdit::multiline(draft)
                .desired_rows(2)
                .hint_text("Type a message…"),
        );
        let response = response.labelled_by(label.id);

        // Enter sends; Shift+Enter inserts a newline (Ctrl/Cmd+Enter also
        // sends — the M11 shortcut; the composer owns it while focused so the
        // global handler does not double-send). egui's multiline editor
        // consumes Enter to insert `\n`; we detect the key press afterwards and
        // strip the trailing newline before sending.
        let enter = response.has_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
        let clicked = ui.add_enabled(!empty, egui::Button::new("Send")).clicked();

        if enter || clicked {
            while draft.ends_with('\n') {
                draft.pop();
            }
            let text = draft.trim().to_string();
            if !text.is_empty() {
                send = Some(text);
            }
        }
    });

    if attach {
        app.push_action(Action::Attach {
            chat_id: chat_id.clone(),
        });
    }
    if let Some(text) = send {
        app.push_action(Action::SendMessage { chat_id, text });
    }
}
