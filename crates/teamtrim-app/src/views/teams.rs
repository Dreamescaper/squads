//! The teams & channels browser (milestone **M10**) plus team administration
//! (backlog item **B4**).
//!
//! Lists the joined teams (from the store's cache, refreshed through
//! [`Teams::list_teams`](teamtrim_core::Teams::list_teams)); a team expands to its
//! channels and clicking a channel opens its conversation (channel ids go
//! through the same chat-service message paths — ADR-018).
//!
//! **B4** adds a **Browse public teams** search (query → joinable results with a
//! Join button) and, for teams the signed-in user **owns**, a member roster with
//! **Add member** (via B3's people picker) and **Remove** (with confirmation).
//! Everything is a queued [`Action`]; the network runs on the app's runtime
//! bridge.

use egui::{RichText, TextEdit, Ui};

use crate::app::{Action, TeamTrimApp};

/// Render the teams & channels page.
pub fn show(app: &mut TeamTrimApp, ui: &mut Ui) {
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

        public_teams(app, ui);
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
                    if app.is_owner_of(&team.id) {
                        ui.label(RichText::new("owner").weak().small());
                    }
                });
                if expanded {
                    channels(app, ui, &team.id);
                    members(app, ui, &team.id);
                }
                ui.separator();
            }
        });
    });
}

/// The public-team search section (B4): a query box, async results, and a
/// Join/Joined affordance per hit.
fn public_teams(app: &mut TeamTrimApp, ui: &mut Ui) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Browse public teams").strong());
        if app.public_loading() {
            ui.spinner();
        }
    });
    let label = ui.label("Search public teams");
    let response = ui.add(
        TextEdit::singleline(app.public_query_mut())
            .hint_text("Search by team name…")
            .desired_width(320.0),
    );
    let response = response.labelled_by(label.id);
    if response.changed() {
        let query = app.public_query().to_string();
        app.push_action(Action::SearchPublicTeams(query));
    }

    let results = app.public_results().to_vec();
    if results.is_empty() {
        if !app.public_query().trim().is_empty() && !app.public_loading() {
            ui.label(RichText::new("No public teams match.").weak().small());
        }
        return;
    }
    ui.add_space(4.0);
    for team in results {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&team.display_name).strong());
            ui.label(RichText::new(&team.visibility).weak().small());
            if let Some(description) = &team.description {
                ui.label(RichText::new(description).weak().small());
            }
            if team.is_joined {
                ui.label(RichText::new("Joined").weak().small());
            } else if ui.button("Join").clicked() {
                app.push_action(Action::JoinTeam(team.id.clone()));
            }
        });
    }
}

/// A team's channel list (M6/M10).
fn channels(app: &mut TeamTrimApp, ui: &mut Ui, team_id: &str) {
    let channels = app.channels_for(team_id).to_vec();
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

/// A team's member roster (B4): the list plus, for owners, add/remove controls.
fn members(app: &mut TeamTrimApp, ui: &mut Ui, team_id: &str) {
    let owner = app.is_owner_of(team_id);
    ui.horizontal(|ui| {
        ui.add_space(20.0);
        ui.label(RichText::new("Members").strong());
        if ui.small_button("Refresh members").clicked() {
            app.push_action(Action::RefreshTeamMembers(team_id.to_string()));
        }
        if owner && ui.small_button("Add member").clicked() {
            app.push_action(Action::OpenAddTeamMember(team_id.to_string()));
        }
    });

    let roster: Vec<(String, String, Option<String>, bool)> = app
        .members_for(team_id)
        .iter()
        .map(|member| {
            (
                member.id.clone(),
                member.display_name.clone(),
                member.email.clone(),
                member.is_owner(),
            )
        })
        .collect();

    if roster.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(20.0);
            ui.label(RichText::new("No members cached.").weak().small());
        });
        return;
    }

    for (membership_id, name, email, is_owner) in roster {
        ui.horizontal(|ui| {
            ui.add_space(20.0);
            ui.label(RichText::new(&name).small());
            if is_owner {
                ui.label(RichText::new("owner").weak().small());
            }
            if let Some(email) = &email {
                ui.label(RichText::new(email).weak().small());
            }
            if !owner {
                return;
            }
            let confirming = app.remove_member_confirm() == Some((team_id, membership_id.as_str()));
            if confirming {
                if ui.small_button("Confirm remove").clicked() {
                    app.push_action(Action::ConfirmRemoveTeamMember {
                        team_id: team_id.to_string(),
                        membership_id: membership_id.clone(),
                    });
                }
                if ui.small_button("Cancel").clicked() {
                    app.push_action(Action::CancelRemoveTeamMember);
                }
            } else if ui.small_button(format!("Remove {name}")).clicked() {
                app.push_action(Action::RequestRemoveTeamMember {
                    team_id: team_id.to_string(),
                    membership_id: membership_id.clone(),
                });
            }
        });
    }
    if !owner {
        ui.horizontal(|ui| {
            ui.add_space(20.0);
            ui.label(
                RichText::new("Member management is available to team owners.")
                    .weak()
                    .small(),
            );
        });
    }
}
