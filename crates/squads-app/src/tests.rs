//! Headless UI tests for the M10 screens, driven by `egui_kittest` + AccessKit
//! (ADR-028).
//!
//! Every test is **offline and deterministic**: the app is built in
//! [`SquadsApp::headless`] mode over an in-memory [`Store`] seeded directly and
//! a [`Session`] on a [`MemoryTokenStore`], so no service is built, no network
//! task is spawned and realtime never starts. Widgets are found by their
//! AccessKit label and driven with clicks/typing; a queued send is asserted
//! through the app's action log.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use squads_core::auth::MemoryTokenStore;
use squads_core::model::{Channel, FileEntry, Identity, Meeting, Message, SendState};
use squads_core::{AppDirs, Chat, Session, SessionState, Settings, Store, Team};

use crate::app::{Action, Page, SquadsApp};
use crate::shortcuts::Shortcut;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dirs() -> Arc<AppDirs> {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("squads-app-test-{}-{n}", std::process::id()));
    Arc::new(AppDirs::rooted_at(tmp))
}

fn chat(id: &str, name: &str) -> Chat {
    Chat {
        id: id.into(),
        name: name.into(),
        is_group: false,
        last_message_time: Some("2026-01-02T09:30:00Z".into()),
        last_message_sender: Some(name.into()),
        last_message_preview: Some(format!("preview from {name}")),
        unread: false,
        pinned: false,
    }
}

fn message(chat_id: &str, id: &str, sender: &str, content: &str, own: bool) -> Message {
    Message {
        id: id.into(),
        chat_id: chat_id.into(),
        sender_mri: format!("8:orgid:{sender}"),
        sender_name: sender.into(),
        timestamp: "2026-01-02T09:30:00Z".into(),
        content: content.into(),
        raw: format!("<p>{content}</p>"),
        reactions: Vec::new(),
        reply_to: None,
        client_message_id: None,
        edited: false,
        is_own: own,
        send_state: SendState::Sent,
    }
}

/// Build a headless harness over a seeded in-memory store. `setup` runs before
/// the app is constructed; the returned `Arc<Store>` is the same store the app
/// reads.
fn app_harness(setup: impl FnOnce(&Store)) -> (Harness<'static, SquadsApp>, Arc<Store>) {
    app_harness_with_session(setup, |_| {})
}

/// Like [`app_harness`], but also lets the test configure the [`Session`]
/// (e.g. inject a signed-in identity) before the app is built.
fn app_harness_with_session(
    setup: impl FnOnce(&Store),
    configure: impl FnOnce(&mut Session),
) -> (Harness<'static, SquadsApp>, Arc<Store>) {
    let dirs = temp_dirs();
    let store = Arc::new(Store::open_in_memory().expect("in-memory store"));
    setup(&store);

    let tokens = Arc::new(MemoryTokenStore::new());
    let mut session =
        Session::with_token_store_and_db(dirs.clone(), "default", tokens, store.clone())
            .expect("session");
    session.set_state(SessionState::Ready);
    configure(&mut session);

    let store_handle = Arc::clone(&store);
    let harness = Harness::new_eframe(move |cc| {
        SquadsApp::headless(cc.egui_ctx.clone(), dirs, session, Settings::default())
            .expect("build app")
    });
    (harness, store_handle)
}

#[test]
fn login_screen_renders_when_signed_out() {
    let dirs = temp_dirs();
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let tokens = Arc::new(MemoryTokenStore::new());
    let session =
        Session::with_token_store_and_db(dirs.clone(), "default", tokens, store).expect("session");
    // State is `SignedOut` by default.
    let harness = Harness::new_eframe(move |cc| {
        SquadsApp::headless(cc.egui_ctx.clone(), dirs, session, Settings::default())
            .expect("build app")
    });

    // The sign-in control and the personal/work toggle are present.
    harness.get_by_label("Sign in");
    harness.get_by_label("Use a personal account");
}

#[test]
fn shell_renders_seeded_chats_and_selecting_shows_messages() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("19:alice@thread.v2", "Alice")])
            .expect("chats");
        store
            .upsert_messages(
                "default",
                &[message(
                    "19:alice@thread.v2",
                    "m1",
                    "Alice",
                    "hello from alice",
                    false,
                )],
            )
            .expect("messages");
    });
    harness.run();

    // The seeded chat is in the list.
    harness.get_by_label("Alice");

    // Selecting it renders its timeline.
    harness.get_by_label("Alice").click();
    harness.run();
    harness.get_by_label("hello from alice");
}

#[test]
fn composer_enter_queues_a_send() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
        store
            .upsert_messages("default", &[message("c1", "m1", "Alice", "hi", false)])
            .expect("messages");
    });
    harness.run();
    harness.get_by_label("Alice").click();
    harness.run();

    // Focus the composer, type, and press Enter.
    harness.get_by_label("Message").click();
    harness.run();
    harness.get_by_label("Message").type_text("hello there");
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();

    let sent =
        harness.state().applied_actions().iter().any(
            |action| matches!(action, Action::SendMessage { text, .. } if text == "hello there"),
        );
    assert!(
        sent,
        "expected a SendMessage action; got {:?}",
        harness.state().applied_actions()
    );
}

#[test]
fn teams_and_files_views_render_seeded_rows() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
        store
            .upsert_teams(
                "default",
                &[Team {
                    id: "team1".into(),
                    name: "Engineering".into(),
                }],
            )
            .expect("teams");
        store
            .upsert_channels(
                "default",
                &[Channel {
                    id: "ch1".into(),
                    team_id: "team1".into(),
                    name: "General".into(),
                    description: None,
                    membership_type: None,
                    web_url: None,
                    is_channel: true,
                }],
            )
            .expect("channels");
        store
            .upsert_files(
                "default",
                Some("c1"),
                &[FileEntry {
                    id: "f1".into(),
                    drive_id: Some("d1".into()),
                    name: "report.pdf".into(),
                    size: 2048,
                    mime: None,
                    web_url: None,
                    download_url: None,
                    is_folder: false,
                    created: None,
                    modified: Some("2026-01-01T00:00:00Z".into()),
                    sender: None,
                    share_url: None,
                    attachment_id: None,
                }],
            )
            .expect("files");
    });
    harness.run();

    // Teams: the team row and (after expanding) its channel.
    harness.get_by_label("Teams").click();
    harness.run();
    harness.get_by_label("Engineering");
    harness.get_by_label("Engineering").click();
    harness.run();
    harness.get_by_label("General");

    // Files: select a chat, then browse its files.
    harness.get_by_label("Chats").click();
    harness.run();
    harness.get_by_label("Alice").click();
    harness.run();
    harness.get_by_label("Files").click();
    harness.run();
    harness.get_by_label("report.pdf");
}

#[test]
fn switching_sections_updates_the_page() {
    let (mut harness, _store) = app_harness(|_| {});
    harness.run();
    assert_eq!(harness.state().page(), Page::Chats);

    for page in [
        Page::Calendar,
        Page::Teams,
        Page::Files,
        Page::Calls,
        Page::Settings,
        Page::Chats,
    ] {
        harness.get_by_label(page.label()).click();
        harness.run();
        assert_eq!(
            harness.state().page(),
            page,
            "expected the {page:?} page after clicking its nav button"
        );
    }
}

#[test]
fn command_shortcuts_open_and_close_the_overlays() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
    });
    harness.run();

    // Ctrl/Cmd+K opens the quick switcher.
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::K);
    harness.run();
    assert!(
        harness.state().quick_switcher_open(),
        "Ctrl+K should open the quick switcher"
    );

    // Esc closes it.
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(
        !harness.state().quick_switcher_open(),
        "Esc should close the quick switcher"
    );

    // Ctrl/Cmd+F shows the in-conversation search.
    harness.get_by_label("Alice").click();
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::F);
    harness.run();
    assert!(
        harness.state().timeline_search_open(),
        "Ctrl+F should show the timeline search"
    );

    // Esc hides it again.
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(!harness.state().timeline_search_open());
}

#[test]
fn apply_shortcut_dispatches_actions() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
    });
    harness.run();

    // The dispatch half of the shortcut design, driven directly.
    harness.state_mut().apply_shortcut(Shortcut::QuickSwitcher);
    harness.run();
    assert!(harness.state().quick_switcher_open());

    harness.state_mut().apply_shortcut(Shortcut::CloseOverlay);
    harness.run();
    assert!(!harness.state().quick_switcher_open());

    harness.state_mut().apply_shortcut(Shortcut::ToggleMute);
    harness.run();
    assert!(harness.state().muted());

    // Send dispatches the selected conversation's draft through the normal
    // send action.
    harness.get_by_label("Alice").click();
    harness.run();
    harness.state_mut().draft_mut("c1").push_str("hi there");
    harness.state_mut().apply_shortcut(Shortcut::Send);
    harness.run();
    assert_eq!(harness.state().draft("c1"), "", "the draft is cleared");
    let sent = harness
        .state()
        .applied_actions()
        .iter()
        .any(|action| matches!(action, Action::SendMessage { text, .. } if text == "hi there"));
    assert!(
        sent,
        "expected a SendMessage action; got {:?}",
        harness.state().applied_actions()
    );
}

#[test]
fn notification_context_follows_the_settings() {
    let (mut harness, _store) = app_harness(|_| {});
    harness.run();

    // Defaults: enabled, all messages.
    let ctx = harness.state().notification_context();
    assert!(ctx.notifications_enabled());
    assert!(!ctx.notify_only_mentions());

    // The settings screen flips them and the context follows.
    harness.get_by_label("Settings").click();
    harness.run();
    harness.get_by_label("Enable notifications").click();
    harness.run();
    assert!(
        !harness
            .state()
            .notification_context()
            .notifications_enabled()
    );

    harness.get_by_label("Enable notifications").click();
    harness.run();
    harness.get_by_label("Notify only for mentions").click();
    harness.run();
    assert!(
        harness
            .state()
            .notification_context()
            .notify_only_mentions()
    );
}

#[test]
fn j_and_k_navigate_the_chat_list() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats(
                "default",
                &[chat("c1", "Alice"), chat("c2", "Bob"), chat("c3", "Carol")],
            )
            .expect("chats");
    });
    harness.run();

    // `j` selects the first chat, then moves down.
    harness.key_press(egui::Key::J);
    harness.run();
    let first = harness.state().chats()[0].id.clone();
    assert_eq!(harness.state().selected_chat(), Some(first.as_str()));

    harness.key_press(egui::Key::J);
    harness.run();
    let second = harness.state().chats()[1].id.clone();
    assert_eq!(harness.state().selected_chat(), Some(second.as_str()));

    // `k` moves back up.
    harness.key_press(egui::Key::K);
    harness.run();
    assert_eq!(harness.state().selected_chat(), Some(first.as_str()));
}

#[test]
fn settings_screen_edits_preferences() {
    let (mut harness, _store) = app_harness(|_| {});
    harness.run();
    harness.get_by_label("Settings").click();
    harness.run();

    // The M11 controls are present.
    harness.get_by_label("Compact mode");
    harness.get_by_label("Enable notifications");
    harness.get_by_label("Notify only for mentions");
    harness.get_by_label("Download folder");

    harness.get_by_label("Compact mode").click();
    harness.run();
    assert!(
        harness.state().compact_mode(),
        "clicking Compact mode should enable it"
    );
}

fn meeting(id: &str, subject: &str, start: &str) -> Meeting {
    Meeting {
        id: id.into(),
        subject: subject.into(),
        start: Some(start.into()),
        end: None,
        all_day: false,
        organizer: Some("Doe, Jane".into()),
        organizer_email: None,
        is_organizer: false,
        join_url: Some(
            "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0".into(),
        ),
        join_meeting_id: None,
        passcode: None,
        web_link: None,
        location: None,
        is_online: true,
    }
}

#[test]
fn calendar_page_renders_seeded_meetings_and_join_queues_an_action() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_meetings(
                "default",
                &[meeting("E1", "Standup", "2999-09-24T09:00:00.0000000")],
            )
            .expect("meetings");
    });
    harness.run();

    // Nav to the Calendar page (the central heading is "Meetings", so the nav
    // label is unambiguous).
    harness.get_by_label("Calendar").click();
    harness.run();
    assert_eq!(harness.state().page(), Page::Calendar);

    // The seeded meeting renders with its subject and organizer.
    harness.get_by_label("Standup");
    harness.get_by_label("Doe, Jane");

    // A Join button per meeting resolves the target; here it queues the action.
    harness.get_by_label("Join").click();
    harness.run();
    let joined = harness
        .state()
        .applied_actions()
        .iter()
        .any(|action| matches!(action, Action::JoinMeeting { target, .. } if target.contains("meetup-join")));
    assert!(
        joined,
        "expected a JoinMeeting action; got {:?}",
        harness.state().applied_actions()
    );

    // Cancelling queues a CancelMeeting for the event id.
    harness.get_by_label("Cancel").click();
    harness.run();
    let cancelled = harness
        .state()
        .applied_actions()
        .iter()
        .any(|action| matches!(action, Action::CancelMeeting { event_id } if event_id == "E1"));
    assert!(
        cancelled,
        "expected a CancelMeeting action; got {:?}",
        harness.state().applied_actions()
    );
}

#[test]
fn calendar_page_join_by_link_queues_a_join_action() {
    let (mut harness, _store) = app_harness(|_| {});
    harness.run();
    harness.get_by_label("Calendar").click();
    harness.run();

    // Type a link into the free-form join box and press Join meeting.
    harness.get_by_label("Join by link or ID").click();
    harness.run();
    harness
        .get_by_label("Join by link or ID")
        .type_text("https://teams.microsoft.com/l/meetup-join/19%3ameeting_y%40thread.v2/0");
    harness.run();
    harness.get_by_label("Join meeting").click();
    harness.run();

    let joined = harness
        .state()
        .applied_actions()
        .iter()
        .any(|action| matches!(action, Action::JoinMeeting { target, .. } if target.contains("meeting_y")));
    assert!(
        joined,
        "expected a JoinMeeting action; got {:?}",
        harness.state().applied_actions()
    );
}

// ----- M13: own identity & inline media --------------------------------------

#[test]
fn an_own_message_is_marked_and_rendered_from_the_identity() {
    // Seed a message whose sender MRI matches our identity but that was cached
    // before whoami resolved (`is_own = false`).
    let (mut harness, _store) = app_harness_with_session(
        |store| {
            store
                .upsert_chats("default", &[chat("c1", "Alice")])
                .expect("chats");
            let mine = message("c1", "m1", "self", "hi from me", false);
            store.upsert_messages("default", &[mine]).expect("messages");
        },
        |session| {
            session.set_identity(Identity {
                mri: "8:orgid:self".into(),
                oid: "self".into(),
                display_name: "Me".into(),
                tenant: "contoso".into(),
            });
        },
    );
    harness.run();
    harness.get_by_label("Alice").click();
    harness.run();

    // The text renders, and the identity marks the message own on the timeline
    // (so the bubble is drawn on the own side).
    harness.get_by_label("hi from me");
    assert!(
        harness.state().timeline()[0].is_own,
        "the identity must mark our message own"
    );
}

#[test]
fn a_message_with_an_image_renders_a_placeholder_not_only_text() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
        let mut image = message("c1", "m1", "Alice", "", false);
        image.raw = r#"<p><img src="https://example.com/photo.png"></p>"#.into();
        store
            .upsert_messages("default", &[image])
            .expect("messages");
    });
    harness.run();
    harness.get_by_label("Alice").click();
    harness.run();

    // The image placeholder is rendered (headless has no cache, so the fetch
    // stays pending) rather than only the plain text.
    harness.get_by_label("Loading image...");
    let requested = harness
        .state()
        .applied_actions()
        .iter()
        .any(|action| matches!(action, Action::LoadMedia { url } if url == "https://example.com/photo.png"));
    assert!(
        requested,
        "expected a LoadMedia action; got {:?}",
        harness.state().applied_actions()
    );
}

// ----- B1: global message search ---------------------------------------------

#[test]
fn search_page_renders_results_and_opening_one_selects_the_chat() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
        store
            .upsert_messages(
                "default",
                &[
                    message("c1", "m1", "Alice", "hello from alice", false),
                    message("c1", "m2", "Alice", "another hello", false),
                ],
            )
            .expect("messages");
    });
    harness.run();

    // The nav rail exposes the Search page.
    harness.get_by_label("Search").click();
    harness.run();
    assert_eq!(harness.state().page(), Page::Search);

    // Typing a query runs the local search and renders the matching rows.
    harness.get_by_label("Search messages").click();
    harness.run();
    harness.get_by_label("Search messages").type_text("hello");
    harness.run();
    assert_eq!(
        harness.state().search_results().len(),
        2,
        "both messages match; got {:?}",
        harness.state().search_results()
    );
    harness.get_by_label("hello from alice");
    harness.get_by_label("another hello");

    // Arrow navigation moves the result selection, not the chat list.
    harness.key_press(egui::Key::ArrowDown);
    harness.run();
    assert_eq!(harness.state().search_selected(), 1);
    assert_eq!(
        harness.state().selected_chat(),
        None,
        "Up/Down on the Search page must not navigate chats"
    );

    // Selecting a row opens its chat and highlights the message.
    harness.get_by_label("hello from alice").click();
    harness.run();
    assert_eq!(harness.state().page(), Page::Chats);
    assert_eq!(harness.state().selected_chat(), Some("c1"));
    assert_eq!(harness.state().highlight_message(), Some("m1"));
    // The highlighted message renders its marker in the timeline.
    harness.get_by_label("match");
}

#[test]
fn global_search_shortcut_opens_the_search_page() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
    });
    harness.run();

    // Ctrl/Cmd+Shift+F opens the global Search page (B1); plain Ctrl/Cmd+F
    // keeps its in-conversation meaning.
    harness.key_press_modifiers(
        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        egui::Key::F,
    );
    harness.run();
    assert_eq!(harness.state().page(), Page::Search);
}
