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
use squads_core::model::{
    Channel, ChatFolder, FileEntry, Identity, Meeting, Message, Person, PinnedMessage, PublicTeam,
    SendState, TeamMember,
};
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
    // The app renders emoji from message content and the channel-reaction
    // picker; the bundled fonts have no emoji glyphs, so allow tofu rather than
    // the harness's default panic (ADR-028).
    let harness = Harness::builder()
        .allow_missing_glyphs()
        .build_eframe(move |cc| {
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

// ----- B3: file search & the reusable people picker --------------------------

fn person(id: &str, name: &str, email: Option<&str>) -> Person {
    Person {
        id: id.into(),
        display_name: name.into(),
        email: email.map(str::to_string),
        job_title: None,
    }
}

#[test]
fn people_picker_search_renders_results_and_selection_yields_an_id() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
    });
    harness.run();

    // The picker is reachable from the Search page (B3), where B2a will also
    // open it.
    harness.get_by_label("Search").click();
    harness.run();
    harness.get_by_label("Find people").click();
    harness.run();
    assert!(
        harness.state().people_picker_open(),
        "the Find people button should open the picker"
    );

    // Seed the async directory results (headless has no directory service).
    harness.state_mut().set_people_results(
        "ava",
        vec![
            person("u1", "Ava Lindqvist", Some("ava@contoso.com")),
            person("u2", "Tom Becker", None),
        ],
    );
    harness.run();

    // The rows render (name + email).
    harness.get_by_label("Ava Lindqvist <ava@contoso.com>");
    harness.get_by_label("Tom Becker");

    // Selecting a person adds them to the selection, which carries the id a
    // caller (B2a) needs to start a chat.
    harness
        .get_by_label("Ava Lindqvist <ava@contoso.com>")
        .click();
    harness.run();
    assert_eq!(harness.state().selected_people().len(), 1);
    assert_eq!(harness.state().selected_people()[0].id, "u1");
    assert!(harness.state().is_person_selected("u1"));
    // The picked person renders as a removable chip.
    harness.get_by_label("Remove Ava Lindqvist");

    // Clicking the chip toggles the pick back off.
    harness.get_by_label("Remove Ava Lindqvist").click();
    harness.run();
    assert!(harness.state().selected_people().is_empty());
}

#[test]
fn files_page_search_renders_global_results_and_downloads() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
    });
    harness.run();
    harness.get_by_label("Files").click();
    harness.run();
    assert_eq!(harness.state().page(), Page::Files);

    // A non-empty query switches the page to the global results; seed them
    // (headless has no drive service).
    harness.state_mut().set_file_search(
        "quarterly",
        vec![FileEntry {
            id: "f1".into(),
            drive_id: Some("d1".into()),
            name: "quarterly.pdf".into(),
            size: 2048,
            mime: None,
            web_url: None,
            download_url: None,
            is_folder: false,
            created: None,
            modified: None,
            sender: None,
            share_url: None,
            attachment_id: None,
        }],
    );
    harness.run();
    harness.get_by_label("quarterly.pdf");

    // Download reuses the existing action; it is resolved from the in-memory
    // search results.
    harness.get_by_label("Download").click();
    harness.run();
    let downloading =
        harness.state().applied_actions().iter().any(
            |action| matches!(action, Action::DownloadFile { file_id, .. } if file_id == "f1"),
        );
    assert!(
        downloading,
        "expected a DownloadFile action; got {:?}",
        harness.state().applied_actions()
    );
}

// ----- B2a: new chat & chat management ---------------------------------------

#[test]
fn new_chat_flow_selects_a_person_and_creates() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
    });
    harness.run();

    // The chat list has a New chat entry point (B2a) that opens the picker in
    // new-chat mode.
    harness.get_by_label("New chat").click();
    harness.run();
    assert!(
        harness.state().new_chat_open(),
        "New chat opens the picker in new-chat mode"
    );
    assert!(harness.state().people_picker_open());

    // Seed directory results (headless has no directory service) and pick one.
    harness.state_mut().set_people_results(
        "ava",
        vec![person("u1", "Ava Lindqvist", Some("ava@contoso.com"))],
    );
    harness.run();
    harness
        .get_by_label("Ava Lindqvist <ava@contoso.com>")
        .click();
    harness.run();
    assert_eq!(harness.state().selected_people().len(), 1);

    // Create chat queues the create action and closes the picker.
    harness.get_by_label("Create chat").click();
    harness.run();
    assert!(
        harness
            .state()
            .applied_actions()
            .iter()
            .any(|action| matches!(action, Action::CreateChat)),
        "expected a CreateChat action; got {:?}",
        harness.state().applied_actions()
    );
    assert!(
        !harness.state().people_picker_open(),
        "the picker closes after creating"
    );
    assert!(!harness.state().new_chat_open());
}

#[test]
fn new_chat_group_reveals_a_topic_field() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
    });
    harness.run();
    harness.get_by_label("New chat").click();
    harness.run();

    harness.state_mut().set_people_results(
        "a",
        vec![
            person("u1", "Ava Lindqvist", Some("ava@contoso.com")),
            person("u2", "Tom Becker", None),
        ],
    );
    harness.run();
    harness
        .get_by_label("Ava Lindqvist <ava@contoso.com>")
        .click();
    harness.run();
    harness.get_by_label("Tom Becker").click();
    harness.run();
    assert_eq!(harness.state().selected_people().len(), 2);

    // 2+ people reveal the group topic field, which the view edits in place.
    harness.get_by_label("Group topic").click();
    harness.run();
    harness.get_by_label("Group topic").type_text("Launch");
    harness.run();
    assert_eq!(harness.state().new_chat_topic(), "Launch");
}

#[test]
fn chat_context_menu_mute_toggles_state() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
    });
    harness.run();

    // The "⋯" affordance opens Alice's context menu.
    harness.get_by_label("...").click();
    harness.run();
    assert_eq!(harness.state().chat_menu(), Some("c1"));

    // Mute toggles the local routing flag (and the menu offers Unmute).
    harness.get_by_label("Mute").click();
    harness.run();
    assert!(
        harness.state().is_chat_muted("c1"),
        "Mute sets the local flag"
    );
    harness.get_by_label("Unmute").click();
    harness.run();
    assert!(!harness.state().is_chat_muted("c1"));
}

#[test]
fn hiding_a_chat_collapses_it_and_it_can_be_shown_again() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
    });
    harness.run();
    harness.get_by_label("...").click();
    harness.run();
    harness.get_by_label("Hide").click();
    harness.run();
    assert!(harness.state().is_chat_hidden("c1"));

    // The row is collapsed; the show-hidden toggle appears.
    harness.get_by_label("Show hidden (1)").click();
    harness.run();
    assert!(harness.state().show_hidden());
    // The hidden row is listed again with an Unhide affordance.
    harness.get_by_label("Unhide");
    harness.get_by_label("Unhide").click();
    harness.run();
    assert!(!harness.state().is_chat_hidden("c1"));
}

#[test]
fn chat_context_menu_leave_confirms_first() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
    });
    harness.run();
    harness.get_by_label("...").click();
    harness.run();
    harness.get_by_label("Leave").click();
    harness.run();

    // Leave asks first: nothing is left until the confirmation.
    assert_eq!(harness.state().leave_confirm(), Some("c1"));
    assert!(
        !harness
            .state()
            .applied_actions()
            .iter()
            .any(|action| matches!(action, Action::ConfirmLeaveChat(_))),
        "Leave must not leave without confirmation"
    );
    harness.get_by_label("Confirm leave").click();
    harness.run();
    assert!(
        harness
            .state()
            .applied_actions()
            .iter()
            .any(|action| matches!(action, Action::ConfirmLeaveChat(id) if id == "c1")),
        "Confirm leave queues the leave; got {:?}",
        harness.state().applied_actions()
    );
}

// ----- B2b: folders & pinned messages ----------------------------------------

fn folder(id: &str, name: &str, items: &[&str]) -> ChatFolder {
    ChatFolder {
        id: id.into(),
        name: name.into(),
        folder_type: "UserCreated".into(),
        is_system: false,
        item_ids: items.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn folder_tabs_filter_the_chat_list() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats(
                "default",
                &[chat("c1", "Alice"), chat("c2", "Bob"), chat("c3", "Carol")],
            )
            .expect("chats");
        store
            .replace_chat_folders(
                "default",
                &[
                    folder("fav", "Favorites", &["c1"]),
                    folder("work", "Work", &["c2"]),
                ],
            )
            .expect("folders");
    });
    harness.run();

    // The tabs render with their counts.
    harness.get_by_label("All");
    harness.get_by_label("Favorites (1)");
    harness.get_by_label("Work (1)");

    // Selecting a folder filters the list to its chats.
    harness.get_by_label("Favorites (1)").click();
    harness.run();
    assert_eq!(harness.state().selected_folder(), Some("fav"));
    assert!(harness.state().chat_in_selected_folder("c1"));
    assert!(!harness.state().chat_in_selected_folder("c2"));
    harness.get_by_label("Alice");
    assert!(
        harness.query_by_label("Bob").is_none(),
        "a chat outside the folder is filtered out of the list"
    );

    // "All" restores every chat.
    harness.get_by_label("All").click();
    harness.run();
    assert_eq!(harness.state().selected_folder(), None);
    harness.get_by_label("Bob");
    harness.get_by_label("Carol");
}

#[test]
fn pinned_panel_renders_pins_and_unpin_removes_one() {
    let (mut harness, store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
        store
            .upsert_messages("default", &[message("c1", "m1", "Alice", "ship it", false)])
            .expect("messages");
        store
            .replace_chat_pins(
                "default",
                "c1",
                &[PinnedMessage {
                    chat_id: "c1".into(),
                    message_id: "m1".into(),
                    sender: Some("Alice".into()),
                    preview: Some("pinned note".into()),
                    time: Some("2026-01-02T09:30:00Z".into()),
                    pinned_at: None,
                    pinned_by: None,
                    graph_pin_id: None,
                    message: None,
                }],
            )
            .expect("pins");
    });
    harness.run();

    // Open the chat and its info pane.
    harness.get_by_label("Alice").click();
    harness.run();
    harness.get_by_label("Info").click();
    harness.run();

    // The pin renders (sender + preview) with its jump/unpin affordances.
    harness.get_by_label("Pinned messages");
    harness.get_by_label("pinned note");
    harness.get_by_label("Go");
    harness.get_by_label("Unpin");

    // Unpin drops it locally.
    harness.get_by_label("Unpin").click();
    harness.run();
    assert!(
        harness.state().applied_actions().iter().any(
            |action| matches!(action, Action::UnpinMessage { message_id, .. } if message_id == "m1")
        ),
        "expected an UnpinMessage action; got {:?}",
        harness.state().applied_actions()
    );
    assert!(harness.state().pins().is_empty());
    assert!(store.pinned_messages("default", "c1").unwrap().is_empty());
    harness.get_by_label("No pinned messages.");
}

#[test]
fn moving_the_selected_chat_into_a_folder_queues_the_action() {
    let (mut harness, _store) = app_harness(|store| {
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .expect("chats");
        store
            .replace_chat_folders("default", &[folder("work", "Work", &[])])
            .expect("folders");
    });
    harness.run();

    // The context menu offers the folder; picking it queues the move.
    harness.get_by_label("...").click();
    harness.run();
    harness.get_by_label("Work").click();
    harness.run();
    assert!(
        harness
            .state()
            .applied_actions()
            .iter()
            .any(|action| matches!(action, Action::MoveChatToFolder { folder_id, .. } if folder_id == "work")),
        "expected a MoveChatToFolder action; got {:?}",
        harness.state().applied_actions()
    );
}

// ----- B4: team administration -----------------------------------------------

#[test]
fn public_team_search_renders_results_and_join_queues() {
    let (mut harness, _store) = app_harness(|_| {});
    harness.run();
    harness.get_by_label("Teams").click();
    harness.run();

    // Seed the async search results (headless has no directory service).
    harness.state_mut().set_public_results(
        "eng",
        vec![
            PublicTeam {
                id: "t1".into(),
                display_name: "Engineering".into(),
                description: Some("Build things".into()),
                visibility: "public".into(),
                is_joined: false,
            },
            PublicTeam {
                id: "t2".into(),
                display_name: "Design".into(),
                description: None,
                visibility: "public".into(),
                is_joined: true,
            },
        ],
    );
    harness.run();

    // The rows render; the joined one shows Joined, the other a Join button.
    harness.get_by_label("Engineering");
    harness.get_by_label("Build things");
    harness.get_by_label("Joined");
    harness.get_by_label("Join").click();
    harness.run();
    assert!(
        harness
            .state()
            .applied_actions()
            .iter()
            .any(|action| matches!(action, Action::JoinTeam(id) if id == "t1")),
        "expected a JoinTeam action; got {:?}",
        harness.state().applied_actions()
    );
}

#[test]
fn member_management_lists_members_and_removes_with_confirmation() {
    let (mut harness, _store) = app_harness_with_session(
        |store| {
            store
                .upsert_teams(
                    "default",
                    &[Team {
                        id: "team1".into(),
                        name: "Engineering".into(),
                    }],
                )
                .expect("teams");
        },
        |session| {
            // The signed-in user owns the team.
            session.set_identity(Identity {
                mri: "8:orgid:me-oid".into(),
                oid: "me-oid".into(),
                display_name: "Me".into(),
                tenant: "contoso.onmicrosoft.com".into(),
            });
        },
    );
    harness.run();

    // Seed the fetched roster (headless has no team service).
    harness.state_mut().set_team_members(
        "team1",
        vec![
            TeamMember {
                id: "M-me".into(),
                user_id: Some("me-oid".into()),
                display_name: "Me".into(),
                email: Some("me@contoso.com".into()),
                roles: vec!["owner".into()],
            },
            TeamMember {
                id: "M-ava".into(),
                user_id: Some("ava-oid".into()),
                display_name: "Ava".into(),
                email: Some("ava@contoso.com".into()),
                roles: Vec::new(),
            },
        ],
    );
    harness.run();

    harness.get_by_label("Teams").click();
    harness.run();
    // Expanding the team reveals its roster; the owner flag unlocks Add/Remove.
    harness.get_by_label("Engineering").click();
    harness.run();
    harness.get_by_label("Members");
    harness.get_by_label("Me");
    harness.get_by_label("Ava");
    harness.get_by_label("Add member");

    // Remove asks first, then confirms.
    harness.get_by_label("Remove Ava").click();
    harness.run();
    harness.get_by_label("Confirm remove").click();
    harness.run();
    assert!(
        harness
            .state()
            .applied_actions()
            .iter()
            .any(|action| matches!(
                action,
                Action::ConfirmRemoveTeamMember { membership_id, .. } if membership_id == "M-ava"
            )),
        "expected a ConfirmRemoveTeamMember action; got {:?}",
        harness.state().applied_actions()
    );

    // Add member opens the people picker in add-member mode.
    harness.get_by_label("Add member").click();
    harness.run();
    assert!(
        harness.state().people_picker_open(),
        "Add member opens the people picker"
    );
    assert_eq!(harness.state().add_member_team(), Some("team1"));
    harness.get_by_label("Grant owner role");

    // Picking a person and adding queues one AddTeamMember per selection.
    harness.state_mut().set_people_results(
        "ava",
        vec![person("ava-oid", "Ava", Some("ava@contoso.com"))],
    );
    harness.run();
    harness.get_by_label("Ava <ava@contoso.com>").click();
    harness.run();
    harness.get_by_label("Add to team").click();
    harness.run();
    assert!(
        harness.state().applied_actions().iter().any(|action| matches!(
            action,
            Action::AddTeamMember { team_id, user_id, owner } if team_id == "team1" && user_id == "ava-oid" && !owner
        )),
        "expected an AddTeamMember action; got {:?}",
        harness.state().applied_actions()
    );
}

#[test]
fn channel_message_reactions_queue_the_channel_lane() {
    let channel = "19:ch@thread.tacv2";
    let (mut harness, _store) = app_harness(|store| {
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
                    id: channel.into(),
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
            .upsert_messages(
                "default",
                &[message(channel, "m1", "Ava", "react to me", false)],
            )
            .expect("messages");
    });
    harness.run();

    // Open the channel from the Teams page (it becomes the selected
    // conversation on the Chats page).
    harness.get_by_label("Teams").click();
    harness.run();
    harness.get_by_label("Engineering").click();
    harness.run();
    harness.get_by_label("General").click();
    harness.run();
    assert_eq!(harness.state().selected_chat(), Some(channel));

    // The channel message carries the reaction affordance; clicking an emoji
    // queues the channel-lane action (not the chat path).
    harness.get_by_label("react to me");
    harness.get_by_label("👍").click();
    harness.run();
    assert!(
        harness.state().applied_actions().iter().any(|action| matches!(
            action,
            Action::ReactInChannel { conversation_id, message_id, emoji, remove, is_reply }
                if conversation_id == channel && message_id == "m1" && emoji == "👍" && !remove && !is_reply
        )),
        "expected a ReactInChannel action; got {:?}",
        harness.state().applied_actions()
    );
}
