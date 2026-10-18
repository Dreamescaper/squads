//! The calendar tab (milestone **M12**).
//!
//! An **agenda** of upcoming meetings (subject, time, organizer, online flag)
//! with a **week view** toggle, a **Join** button per meeting, a free-form join
//! box, and a small **New meeting** form (subject / start / end / timezone) with
//! a Schedule action. Cancelling a meeting is available on each row.
//!
//! Joining resolves the target through
//! [`Calendar::resolve_join`](teamtrim_core::Calendar::resolve_join): a native
//! `thread` target is handed off to the existing call path
//! ([`Action::PlaceCall`]); anything else (a browser link) is shown as the
//! resolved target. Everything is read from the store's cache and refreshed
//! through the service; the view only queues [`Action`]s.

use egui::{RichText, Ui};

use crate::app::{Action, TeamTrimApp, short_date, short_time};

/// Render the calendar page.
pub fn show(app: &mut TeamTrimApp, ui: &mut Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.heading("Meetings");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Refresh").clicked() {
                    app.push_action(Action::RefreshCalendar);
                }
            });
        });

        // Agenda / week toggle.
        ui.horizontal(|ui| {
            let agenda = !app.calendar_week();
            if ui.selectable_label(agenda, "Agenda").clicked() {
                app.push_action(Action::RefreshCalendar);
            }
            if ui.selectable_label(app.calendar_week(), "Week").clicked() {
                app.push_action(Action::RefreshCalendarWeek);
            }
        });
        ui.separator();

        show_join_box(app, ui);

        // The last resolved join target (shown when it could not be dialled).
        if let Some(target) = app.join_target() {
            let url = target.url.clone();
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Resolved:").weak());
                ui.label(RichText::new(format!("[{}]", target.kind)).monospace());
                ui.label(url);
            });
        }

        ui.add_space(8.0);
        let heading = if app.calendar_week() {
            "This week"
        } else {
            "Upcoming"
        };
        ui.heading(heading);
        if app.calendar_loading() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Loading meetings…").weak());
            });
        }

        let meetings = app.meetings().to_vec();
        if meetings.is_empty() {
            ui.label(RichText::new("No meetings cached. Press Refresh.").weak());
        } else {
            egui::ScrollArea::vertical()
                .max_height(280.0)
                .show(ui, |ui| {
                    for meeting in &meetings {
                        show_meeting_row(app, ui, meeting);
                    }
                });
        }

        ui.add_space(12.0);
        ui.separator();
        show_new_meeting_form(app, ui);
    });
}

/// The free-form "join by link or meeting id" box.
fn show_join_box(app: &mut TeamTrimApp, ui: &mut Ui) {
    ui.horizontal(|ui| {
        let label = ui.label("Join by link or ID");
        let response = ui.add(
            egui::TextEdit::singleline(app.join_input_mut())
                .hint_text("Paste a Teams link or type a meeting id")
                .desired_width(300.0),
        );
        response.labelled_by(label.id);
        let pass_label = ui.label("Passcode");
        ui.add(
            egui::TextEdit::singleline(app.join_passcode_mut())
                .hint_text("optional")
                .desired_width(120.0),
        )
        .labelled_by(pass_label.id);

        let target = app.join_input().trim().to_string();
        if ui
            .add_enabled(!target.is_empty(), egui::Button::new("Join meeting"))
            .clicked()
        {
            let passcode = app.join_passcode().trim().to_string();
            app.push_action(Action::JoinMeeting { target, passcode });
        }
    });
}

/// One meeting row: subject, time, organizer, online flag, Join and Cancel.
fn show_meeting_row(app: &mut TeamTrimApp, ui: &mut Ui, meeting: &teamtrim_core::Meeting) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(meeting.subject.as_str()).strong());
        let when = meeting_time(meeting);
        if !when.is_empty() {
            ui.label(RichText::new(when).weak().small());
        }
        if let Some(organizer) = &meeting.organizer {
            ui.label(RichText::new(organizer).weak().small());
        }
        if meeting.is_online {
            ui.label(RichText::new("[online]").weak().small());
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Cancel").clicked() {
                app.push_action(Action::CancelMeeting {
                    event_id: meeting.id.clone(),
                });
            }
            let target = meeting
                .join_url
                .clone()
                .or_else(|| meeting.join_meeting_id.clone());
            let can_join = target.is_some();
            if ui
                .add_enabled(can_join, egui::Button::new("Join"))
                .clicked()
                && let Some(target) = target
            {
                app.push_action(Action::JoinMeeting {
                    target,
                    passcode: meeting.passcode.clone().unwrap_or_default(),
                });
            }
        });
    });
    ui.separator();
}

/// The "new meeting" form.
fn show_new_meeting_form(app: &mut TeamTrimApp, ui: &mut Ui) {
    ui.heading("New meeting");
    ui.horizontal(|ui| {
        let subject = ui.label("Subject");
        ui.add(
            egui::TextEdit::singleline(app.new_meeting_subject_mut())
                .hint_text("Meeting subject")
                .desired_width(220.0),
        )
        .labelled_by(subject.id);

        let tz = ui.label("Time zone");
        ui.add(
            egui::TextEdit::singleline(app.new_meeting_timezone_mut())
                .hint_text("UTC")
                .desired_width(140.0),
        )
        .labelled_by(tz.id);
    });
    ui.horizontal(|ui| {
        let start = ui.label("Start");
        ui.add(
            egui::TextEdit::singleline(app.new_meeting_start_mut())
                .hint_text("2026-09-29T10:00:00")
                .desired_width(200.0),
        )
        .labelled_by(start.id);

        let end = ui.label("End");
        ui.add(
            egui::TextEdit::singleline(app.new_meeting_end_mut())
                .hint_text("2026-09-29T10:30:00")
                .desired_width(200.0),
        )
        .labelled_by(end.id);

        let ready = !app.new_meeting_subject().trim().is_empty()
            && !app.new_meeting_start().trim().is_empty()
            && !app.new_meeting_end().trim().is_empty()
            && !app.new_meeting_timezone().trim().is_empty();
        if ui
            .add_enabled(ready, egui::Button::new("Schedule"))
            .clicked()
        {
            app.push_action(Action::ScheduleMeeting {
                subject: app.new_meeting_subject().trim().to_string(),
                start: app.new_meeting_start().trim().to_string(),
                end: app.new_meeting_end().trim().to_string(),
                time_zone: app.new_meeting_timezone().trim().to_string(),
            });
        }
    });
    ui.label(
        RichText::new(
            "Datetimes are Graph `YYYY-MM-DDTHH:MM:SS` values in the given time zone; \
             a Teams meeting is requested so Graph provisions the join link.",
        )
        .weak()
        .small(),
    );
}

/// `YYYY-MM-DD HH:MM` from the meeting's wire start (falls back to the raw
/// string, or empty when there is no start).
fn meeting_time(meeting: &teamtrim_core::Meeting) -> String {
    match meeting.start.as_deref() {
        None => String::new(),
        Some(start) => format!("{} {}", short_date(start), short_time(start)),
    }
}
