//! The call screen (milestone **M10**).
//!
//! Drives the [`CallService`](squads_core::CallService) state machine: place a
//! call in the selected conversation, accept/decline a ringing incoming call,
//! hang up, and browse the local call history. Call *media* is not available in
//! this build (B-011) — the screen says so explicitly and the service stops at
//! `Connecting`/`Connected` until the caller drives `mark_connected`.

use egui::{RichText, Ui};
use squads_core::CallState;

use crate::app::{Action, SquadsApp};

/// Render the calls page.
pub fn show(app: &mut SquadsApp, ui: &mut Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.heading("Call");
        ui.separator();

        let state = app.call_state().clone();
        ui.horizontal(|ui| {
            ui.label("State:");
            ui.strong(state.label());
            if let Some(reason) = state.end_reason() {
                ui.label(RichText::new(format!("({})", reason.as_str())).weak());
            }
        });
        if let Some(incoming) = app.incoming_call() {
            ui.label(format!("Incoming call from {}", incoming.caller_label()));
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| match &state {
            CallState::Idle | CallState::Ended { .. } => {
                let selected = app.selected_chat().map(str::to_string);
                let can_call = selected.is_some();
                if ui
                    .add_enabled(can_call, egui::Button::new("Call"))
                    .clicked()
                    && let Some(thread_id) = selected
                {
                    app.push_action(Action::PlaceCall {
                        thread_id,
                        callee: None,
                        video: false,
                    });
                }
                if state.is_terminal() && ui.button("Dismiss").clicked() {
                    app.push_action(Action::ResetCall);
                }
            }
            CallState::Ringing => {
                if ui.button("Accept").clicked() {
                    app.push_action(Action::AcceptCall);
                }
                if ui.button("Decline").clicked() {
                    app.push_action(Action::DeclineCall);
                }
            }
            CallState::Placing | CallState::Connecting | CallState::Connected => {
                if ui.button("Hang up").clicked() {
                    app.push_action(Action::EndCall);
                }
                if state == CallState::Connecting && ui.button("Mark connected").clicked() {
                    // Media is not wired in this build; expose the transition so
                    // the state machine can be exercised by hand (B-011).
                    app.push_action(Action::MarkCallConnected);
                }
            }
        });

        ui.add_space(12.0);
        ui.separator();
        ui.label(
            RichText::new(
                "Audio/video capture is not available in this build; calls are \
                 signalling-only (see docs/BLOCKERS.md B-011).",
            )
            .weak()
            .small(),
        );

        ui.add_space(12.0);
        ui.separator();
        ui.heading("Recents");
        let log = app.call_log().to_vec();
        if log.is_empty() {
            ui.label(RichText::new("No call history yet.").weak());
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for entry in log {
                ui.horizontal(|ui| {
                    ui.label(entry.direction.as_str());
                    let who = if entry.participants.is_empty() {
                        "(unknown)".to_string()
                    } else {
                        entry.participants.join(", ")
                    };
                    ui.label(who);
                    ui.label(RichText::new(entry.outcome.as_str()).weak().small());
                    if let Some(duration) = entry.duration_secs {
                        ui.label(RichText::new(format!("{duration}s")).weak().small());
                    }
                });
                ui.separator();
            }
        });
    });
}
