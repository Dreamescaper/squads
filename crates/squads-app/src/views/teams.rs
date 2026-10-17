//! The teams & channels browser (milestone **M10**).
//!
//! Lists the joined teams (from the store's cache, refreshed through
//! [`Teams::list_teams`](squads_core::Teams::list_teams)); a team expands to its
//! channels and clicking a channel opens its conversation (channel ids go
//! through the same chat-service message paths — ADR-018).

use egui::{RichText, Ui};

use crate::app::{Action, SquadsApp};

/// Render the teams & channels page.
pub fn show(app: &mut SquadsApp, ui: &mut Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.heading("Teams & channels");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Refresh").clicked() {
                    app.push_action(Action::RefreshTeams);
                }
            });
        });
        ui.separator();

        let teams = app.teams().to_vec();
        if teams.is_empty() {
            ui.label(RichText::new("No teams cached yet. Press Refresh.").weak());
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            for team in teams {
                let expanded = app.team_expanded(&team.id);
                ui.horizontal(|ui| {
                    let arrow = if expanded { "v" } else { ">" };
                    if ui.button(arrow).clicked() {
                        app.push_action(Action::ToggleTeam(team.id.clone()));
                    }
                    if ui.button(team.name.as_str()).clicked() {
                        app.push_action(Action::ToggleTeam(team.id.clone()));
                    }
                });
                if expanded {
                    let channels = app.channels_for(&team.id).to_vec();
                    if channels.is_empty() {
                        ui.horizontal(|ui| {
                            ui.add_space(20.0);
                            ui.label(RichText::new("No channels cached.").weak());
                        });
                    }
                    for channel in channels {
                        ui.horizontal(|ui| {
                            ui.add_space(20.0);
                            ui.label("#");
                            if ui.button(channel.name.as_str()).clicked() {
                                app.push_action(Action::SelectChat(channel.id.clone()));
                            }
                        });
                    }
                }
                ui.separator();
            }
        });
    });
}
