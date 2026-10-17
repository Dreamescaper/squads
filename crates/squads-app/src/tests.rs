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
use squads_core::model::{Channel, FileEntry, Message, SendState};
use squads_core::{AppDirs, Chat, Session, SessionState, Settings, Store, Team};

use crate::app::{Action, Page, SquadsApp};

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
    let dirs = temp_dirs();
    let store = Arc::new(Store::open_in_memory().expect("in-memory store"));
    setup(&store);

    let tokens = Arc::new(MemoryTokenStore::new());
    let mut session =
        Session::with_token_store_and_db(dirs.clone(), "default", tokens, store.clone())
            .expect("session");
    session.set_state(SessionState::Ready);

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
