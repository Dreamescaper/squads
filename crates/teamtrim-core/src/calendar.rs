//! Offline-first calendar service (milestone **M12**).
//!
//! This is the layer the calendar tab drives. Like [`Teams`](crate::teams) and
//! [`Files`](crate::files), it ties the shared [`Store`] (instant, offline
//! reads) to the shared [`ApiClient`] (typed Graph calls), and every method is
//! testable offline against a local TCP stub.
//!
//! ```text
//!                    UI (calendar tab)
//!                     │  upcoming / week / schedule / cancel / resolve_join
//!                     ▼
//!                 Calendar ──▶ Arc<Store>      (M12: cached meetings)
//!                     └────▶ Arc<ApiClient>    (M2: typed Graph calls)
//! ```
//!
//! ## Everything is Graph
//!
//! The whole service rides the Graph token:
//!
//! | Operation | Graph call |
//! |---|---|
//! | Upcoming (next 7 days) | `GET /me/calendar/calendarView` |
//! | Week view (explicit window) | `GET /me/calendar/calendarView` |
//! | Schedule | `POST /me/calendar/events` (`isOnlineMeeting` + `teamsForBusiness`) |
//! | Cancel | `DELETE /me/calendar/events/{id}` |
//! | Join by numeric id | `GET /me/onlineMeetings?$filter=joinMeetingIdSettings/…` |
//!
//! ## Join hand-off to calls
//!
//! [`Calendar::resolve_join`] turns a pasted join URL (or a typed numeric
//! meeting id, which it resolves through Graph first) into a [`JoinTarget`]
//! (ADR-033). A target of kind `thread` carries the native conversation id the
//! existing call path dials ([`CallService::place`](crate::CallService::place)
//! via [`Action::PlaceCall`](crate::app::Action)); every other kind (a browser
//! `meeting-id` link, a plain `url`, or `unknown`) cannot start a call and is
//! surfaced to the user as the resolved target instead.
//!
//! ## Lobby
//!
//! Joining a meeting can park the caller in the waiting room until admitted.
//! [`lobby_step`] and the tiny [`Lobby`] holder wrap the vendored pure
//! `ost::api::lobby_next` state machine (`idle → joining → lobby →
//! admitted`, with `failed` reachable and `reset` from anywhere); no network is
//! involved. Live lobby signalling is not observable through the M4 bus
//! (B-008), so the UI drives the transitions from the call-signalling callbacks
//! once that producer exists.

use std::sync::Arc;

use crate::api::{ApiClient, JoinIdResolve, JoinTarget, LobbyEvent, LobbyState};
use crate::auth::tokens::now_unix;
use crate::error::{CoreError, Result};
use crate::model::Meeting;
use crate::store::Store;

/// The offline-first calendar service (see the module docs).
pub struct Calendar {
    profile: String,
    store: Arc<Store>,
    client: Arc<ApiClient>,
}

impl Calendar {
    /// Build the service over a shared store and protocol client.
    pub fn new(profile: impl Into<String>, store: Arc<Store>, client: Arc<ApiClient>) -> Self {
        Self {
            profile: profile.into(),
            store,
            client,
        }
    }

    /// The account profile this service reads/writes under.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The shared local store.
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// The shared protocol client.
    pub fn client(&self) -> &Arc<ApiClient> {
        &self.client
    }

    // ----- reads ----------------------------------------------------------

    /// The cached meetings that start at or after *now*. Purely local: never
    /// touches the network (the calendar tab's instant first paint).
    pub fn cached_upcoming(&self, limit: usize) -> Result<Vec<Meeting>> {
        self.store
            .upcoming_meetings(&self.profile, now_unix() as i64, limit)
    }

    /// Fetch the next 7 days of meetings (Graph `calendarView`), cache them and
    /// return the cached upcoming set (soonest first).
    ///
    /// Call [`Calendar::cached_upcoming`] first for an immediate render, then
    /// this for the refresh.
    pub async fn upcoming(&self, limit: usize) -> Result<Vec<Meeting>> {
        let infos = self.client.list_upcoming_meetings(limit).await?;
        let meetings: Vec<Meeting> = infos.iter().map(Meeting::from).collect();
        self.store.upsert_meetings(&self.profile, &meetings)?;
        self.cached_upcoming(limit)
    }

    /// Fetch the meetings in the explicit window `[start_secs, start_secs+days]`
    /// (Graph `calendarView`), cache them and return them in wire order
    /// (soonest first).
    ///
    /// `days` must be `1..=31`; the vendored call rejects anything else before
    /// any network. This backs the calendar **week view**.
    pub async fn week(&self, start_secs: u64, days: u64, limit: usize) -> Result<Vec<Meeting>> {
        let infos = self
            .client
            .list_week_meetings(start_secs, days, limit)
            .await?;
        let meetings: Vec<Meeting> = infos.iter().map(Meeting::from).collect();
        self.store.upsert_meetings(&self.profile, &meetings)?;
        Ok(meetings)
    }

    // ----- writes ---------------------------------------------------------

    /// Schedule a meeting (Graph `POST /me/calendar/events`) with a Teams online
    /// meeting, cache the created event and return it (with its `join_url`).
    ///
    /// The inputs are validated first (the vendored
    /// [`validate_schedule`](crate::api::validate_schedule)) so a bad subject,
    /// non-Graph datetime, `end <= start` or empty timezone is rejected before
    /// any network.
    pub async fn schedule(
        &self,
        subject: &str,
        start: &str,
        end: &str,
        time_zone: &str,
    ) -> Result<Meeting> {
        // Fail fast (and identically) before building the client call; the
        // vendored `schedule_meeting_data` validates again, harmlessly.
        ost::api::validate_schedule(subject, start, end, time_zone)
            .map_err(|e| CoreError::NotReady(e.to_string()))?;
        let info = self
            .client
            .schedule_meeting(subject, start, end, time_zone, true)
            .await?;
        let meeting = Meeting::from(&info);
        self.store
            .upsert_meetings(&self.profile, std::slice::from_ref(&meeting))?;
        Ok(meeting)
    }

    /// Cancel one meeting (Graph `DELETE /me/calendar/events/{id}`) and drop the
    /// cached row.
    pub async fn cancel(&self, event_id: &str) -> Result<()> {
        self.client.cancel_meeting(event_id).await?;
        self.store.delete_meeting(&self.profile, event_id)?;
        Ok(())
    }

    // ----- join -----------------------------------------------------------

    /// Resolve a pasted join string (a Teams link) **or** a typed numeric
    /// meeting id into a [`JoinTarget`].
    ///
    /// * A string the vendored [`normalize_join_meeting_id`](crate::api::normalize_join_meeting_id)
    ///   accepts (`9..=15` digits) is looked up through Graph
    ///   (`GET /me/onlineMeetings`), and the returned `joinWebUrl` is parsed.
    ///   A disclosed passcode that differs from `passcode` is an error
    ///   ([`CoreError::NotReady`]). **In practice the lookup always falls back**:
    ///   the Teams first-party token is not granted `OnlineMeetings.Read`, and
    ///   the vendored Graph client refuses `/me/onlineMeetings` before any
    ///   request (`graph_denied`), so a numeric id resolves to the browser
    ///   `/meet/<id>?p=` link the Teams web client uses (kind `meeting-id`).
    ///   That is also what [`JoinIdResolve::NotFound`] (an outside meeting)
    ///   yields.
    /// * Anything else is classified purely by
    ///   [`parse_join_url`](crate::api::parse_join_url): a native thread id (a
    ///   `meetup-join/19:…@thread…` link or a bare thread id) → kind `thread`,
    ///   which the call path can dial; other `https` links → kind `url`;
    ///   everything else → kind `unknown`.
    pub async fn resolve_join(&self, target_or_url: &str, passcode: &str) -> Result<JoinTarget> {
        let Some(id) = ost::api::normalize_join_meeting_id(target_or_url) else {
            return Ok(ost::api::parse_join_url(target_or_url));
        };
        match self.client.resolve_join_meeting_id(&id, passcode).await {
            Ok(JoinIdResolve::Found(resolved)) => {
                Ok(ost::api::parse_join_url(&resolved.join_web_url))
            }
            Ok(JoinIdResolve::PasscodeMismatch) => Err(CoreError::NotReady(
                "meeting passcode does not match".into(),
            )),
            // An outside meeting (`NotFound`) **or** a lookup the Teams sign-in
            // cannot make both resolve to the browser fallback; the passcode,
            // when typed, goes in the `?p=` query the web client uses.
            Ok(JoinIdResolve::NotFound) | Err(_) => Ok(browser_join_fallback(&id, passcode)),
        }
    }
}

/// The browser `/meet/<id>?p=<passcode>` join link (kind `meeting-id`) for a
/// numeric meeting id the Graph lookup could not resolve.
fn browser_join_fallback(id: &str, passcode: &str) -> JoinTarget {
    let passcode = passcode.trim();
    let url = if passcode.is_empty() {
        format!("https://teams.microsoft.com/meet/{id}")
    } else {
        format!("https://teams.microsoft.com/meet/{id}?p={passcode}")
    };
    JoinTarget {
        kind: "meeting-id".to_string(),
        thread_id: None,
        meeting_id: Some(id.to_string()),
        url,
    }
}

/// Redacting `Debug`: no meeting subjects or join URLs.
impl std::fmt::Debug for Calendar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Calendar")
            .field("profile", &self.profile)
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

/// Drive the vendored pure lobby state machine one step.
///
/// `idle → joining → lobby → admitted`, with `failed` reachable from
/// `joining`/`lobby` and `reset` returning to `idle` from anywhere; terminal
/// states ignore stale progress events. See [`Lobby`] for a small holder.
pub fn lobby_step(state: LobbyState, event: LobbyEvent) -> LobbyState {
    ost::api::lobby_next(state, event)
}

/// A tiny lobby-state holder the UI drives with [`LobbyEvent`]s.
///
/// Wraps the pure [`lobby_step`] so the calendar tab can keep one value and
/// feed it the call-signalling callbacks (`Start` on Join, `LobbySignal` on a
/// waiting-room signal, `Admit`/`Reject` on the outcome, `Reset` on dismiss).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lobby {
    state: LobbyState,
}

impl Default for Lobby {
    fn default() -> Self {
        Lobby {
            state: LobbyState::Idle,
        }
    }
}

impl Lobby {
    /// A fresh lobby in the `idle` state.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current lobby state.
    pub fn state(&self) -> LobbyState {
        self.state
    }

    /// Apply one event and return the new state.
    pub fn apply(&mut self, event: LobbyEvent) -> LobbyState {
        self.state = lobby_step(self.state, event);
        self.state
    }

    /// Return to `idle` (dismiss / retry / new join string).
    pub fn reset(&mut self) -> LobbyState {
        self.apply(LobbyEvent::Reset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{StubResponse, http_stub};
    use crate::auth::tokens::{StoredToken, TokenSet};
    use std::sync::Mutex;

    fn tokens_for(base: &str) -> TokenSet {
        let future = crate::auth::tokens::now_unix() + 3600;
        let mk = |name: &str| StoredToken::with_expiry(name, future);
        TokenSet {
            refresh_token: "refresh-secret".into(),
            tenant: "contoso.onmicrosoft.com".into(),
            region_gtms: serde_json::json!({ "graphBase": base }),
            personal: false,
            spaces: mk("spaces-token"),
            skype: mk("skype-token"),
            graph: mk("graph-token"),
            ic3: mk("ic3-token"),
            recorder: mk("recorder-token"),
        }
    }

    fn test_calendar(base: &str) -> (Arc<Store>, Calendar) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let client = Arc::new(ApiClient::from_tokens(&tokens_for(base)));
        let calendar = Calendar::new("default", Arc::clone(&store), client);
        (store, calendar)
    }

    fn requests(seen: &std::sync::Arc<Mutex<Vec<String>>>) -> Vec<String> {
        seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn view_json() -> String {
        serde_json::json!({
            "value": [
                {"id": "E2", "subject": "Sync",
                 "isOnlineMeeting": true,
                 "onlineMeeting": {"joinUrl": "https://teams.microsoft.com/l/meetup-join/19%3ameeting_b%40thread.v2/0"},
                 "start": {"dateTime": "2026-09-29T10:00:00.0000000", "timeZone": "UTC"},
                 "end": {"dateTime": "2026-09-29T10:30:00.0000000", "timeZone": "UTC"},
                 "organizer": {"emailAddress": {"name": "Doe, Jane", "address": "jane@contoso.com"}}},
                {"id": "E1", "subject": "Standup",
                 "isOnlineMeeting": false,
                 "start": {"dateTime": "2026-09-24T09:00:00.0000000", "timeZone": "UTC"},
                 "end": {"dateTime": "2026-09-24T09:15:00.0000000", "timeZone": "UTC"}}
            ]
        })
        .to_string()
    }

    #[tokio::test]
    async fn upcoming_fetches_parses_caches_and_orders() {
        // `now` is real, so seed the stub with far-future events so the
        // `cached_upcoming` filter keeps them.
        let body = view_json().replace("2026-", "2999-");
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let (store, calendar) = test_calendar(&base);

        let got = calendar.upcoming(25).await.expect("upcoming");
        assert_eq!(
            got.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["E1", "E2"],
            "soonest first"
        );
        assert_eq!(got[0].subject, "Standup");
        assert!(got[1].is_online);
        assert_eq!(got[1].organizer.as_deref(), Some("Doe, Jane"));
        assert_eq!(got[1].organizer_email.as_deref(), Some("jane@contoso.com"));
        assert!(got[1].join_url.is_some());
        // Cached and offline-readable.
        assert_eq!(calendar.cached_upcoming(25).unwrap().len(), 2);
        assert_eq!(
            store.meeting("E1", "default").unwrap().unwrap().subject,
            "Standup"
        );

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(
            req.starts_with("GET /me/calendar/calendarView?"),
            "unexpected request line: {req}"
        );
        assert!(req.contains("$top=25"), "{req}");
        assert!(req.contains("$orderby=start/dateTime"), "{req}");
        assert!(req.contains("startDateTime="), "{req}");
        assert!(req.contains("authorization: Bearer graph-token"), "{req}");
    }

    #[tokio::test]
    async fn week_uses_the_explicit_window_and_validates_days() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok(view_json())]).await;
        let (_store, calendar) = test_calendar(&base);

        // 2026-09-28T00:00:00Z = 1790553600.
        let got = calendar.week(1_790_553_600, 7, 50).await.expect("week");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "E2");

        // `days` out of range is rejected before any network.
        let err = calendar.week(1_790_553_600, 0, 10).await.unwrap_err();
        assert!(format!("{err}").contains("1..=31"), "err = {err}");

        task.await.expect("stub drains");
        let reqs = requests(&seen);
        assert_eq!(reqs.len(), 1, "the rejected call made no request");
        assert!(
            reqs[0].starts_with("GET /me/calendar/calendarView?"),
            "{reqs:?}"
        );
        assert!(
            reqs[0].contains("startDateTime=2026-09-28T00%3A00%3A00Z"),
            "{reqs:?}"
        );
        assert!(
            reqs[0].contains("endDateTime=2026-10-05T00%3A00%3A00Z"),
            "{reqs:?}"
        );
    }

    #[tokio::test]
    async fn schedule_validates_then_posts_the_teams_body() {
        let created = serde_json::json!({
            "id": "C1", "subject": "Sync",
            "isOnlineMeeting": true,
            "onlineMeeting": {"joinUrl": "https://teams.microsoft.com/l/meetup-join/new"},
            "start": {"dateTime": "2026-09-29T10:00:00", "timeZone": "UTC"},
            "end": {"dateTime": "2026-09-29T10:30:00", "timeZone": "UTC"}
        })
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(created)]).await;
        let (store, calendar) = test_calendar(&base);

        let meeting = calendar
            .schedule("Sync", "2026-09-29T10:00:00", "2026-09-29T10:30:00", "UTC")
            .await
            .expect("schedule");
        assert_eq!(meeting.id, "C1");
        assert_eq!(meeting.subject, "Sync");
        assert!(meeting.is_online);
        assert_eq!(
            meeting.join_url.as_deref(),
            Some("https://teams.microsoft.com/l/meetup-join/new")
        );
        // Cached from the write's own answer.
        assert_eq!(store.meeting("C1", "default").unwrap().unwrap().id, "C1");

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(
            req.starts_with("POST /me/calendar/events "),
            "unexpected request line: {req}"
        );
        assert!(req.contains("\"subject\":\"Sync\""), "{req}");
        assert!(
            req.contains("\"dateTime\":\"2026-09-29T10:00:00\""),
            "{req}"
        );
        assert!(req.contains("\"timeZone\":\"UTC\""), "{req}");
        assert!(req.contains("\"isOnlineMeeting\":true"), "{req}");
        assert!(
            req.contains("\"onlineMeetingProvider\":\"teamsForBusiness\""),
            "{req}"
        );

        // Invalid input is rejected before any request.
        let err = calendar
            .schedule("", "2026-09-29T10:00:00", "2026-09-29T10:30:00", "UTC")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("subject"), "err = {err}");
        let err = calendar
            .schedule("S", "2026-09-29T10:30:00", "2026-09-29T10:00:00", "UTC")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("after start"), "err = {err}");
        assert_eq!(calendar.client().request_count(), 1, "no extra request");
    }

    #[tokio::test]
    async fn cancel_deletes_the_event_and_drops_the_cache() {
        let (base, seen, task) = http_stub(vec![StubResponse::status(204)]).await;
        let (store, calendar) = test_calendar(&base);
        store
            .upsert_meetings(
                "default",
                &[Meeting::from_info(&crate::api::MeetingInfo {
                    id: "C1".into(),
                    subject: "Sync".into(),
                    start: Some("2999-09-29T10:00:00".into()),
                    end: None,
                    join_url: None,
                    organizer: None,
                    organizer_email: None,
                    is_organizer: false,
                    is_online: true,
                })],
            )
            .unwrap();

        calendar.cancel("C1").await.expect("cancel");
        assert!(store.meeting("C1", "default").unwrap().is_none());

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(
            req.starts_with("DELETE /me/calendar/events/C1 "),
            "unexpected request line: {req}"
        );

        // A malformed id is rejected before any network.
        let err = calendar.cancel("a/b").await.unwrap_err();
        assert!(format!("{err}").contains("event_id"), "err = {err}");
    }

    #[tokio::test]
    async fn resolve_join_parses_a_pasted_url_purely() {
        // No service call: a link is classified locally.
        let (_store, calendar) = test_calendar("http://127.0.0.1:9");
        let target = calendar
            .resolve_join(
                "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0?context=%7b%7d",
                "",
            )
            .await
            .expect("resolve");
        assert_eq!(target.kind, "thread");
        assert_eq!(target.thread_id.as_deref(), Some("19:meeting_x@thread.v2"));
        assert_eq!(
            calendar.client().request_count(),
            0,
            "no network for a link"
        );

        // A bare thread id passes straight through.
        let target = calendar.resolve_join("19:abc@thread.v2", "").await.unwrap();
        assert_eq!(target.kind, "thread");

        // A non-Teams link is an `url`; garbage is `unknown`.
        let target = calendar
            .resolve_join("https://example.com/x", "")
            .await
            .unwrap();
        assert_eq!(target.kind, "url");
        let target = calendar.resolve_join("hello", "").await.unwrap();
        assert_eq!(target.kind, "unknown");
    }

    #[tokio::test]
    async fn resolve_join_numeric_id_falls_back_to_the_browser_link() {
        // The Graph `onlineMeetings` lookup is denied with the Teams sign-in
        // (`OnlineMeetings.Read` is not granted): the vendored `graph_get`
        // refuses `/me/onlineMeetings` before any request, so a numeric id
        // resolves to the browser `/meet/<id>?p=` link (kind `meeting-id`).
        // Port 9 is unreachable; no request is attempted.
        let (_store, calendar) = test_calendar("http://127.0.0.1:9");

        let target = calendar
            .resolve_join("123 456 789 012", "abc123")
            .await
            .expect("resolve");
        assert_eq!(target.kind, "meeting-id");
        assert_eq!(target.meeting_id.as_deref(), Some("123456789012"));
        assert_eq!(
            target.url,
            "https://teams.microsoft.com/meet/123456789012?p=abc123"
        );

        // Without a passcode the `?p=` query is omitted.
        let target = calendar.resolve_join("123456789012", "").await.unwrap();
        assert_eq!(target.url, "https://teams.microsoft.com/meet/123456789012");

        // A digit count outside 9..=15 is not a meeting id: it falls to the
        // pure parser (here, `unknown`).
        let target = calendar.resolve_join("12345", "").await.unwrap();
        assert_eq!(target.kind, "unknown");
    }

    #[test]
    fn lobby_holder_walks_the_state_machine() {
        use crate::api::{LobbyEvent as E, LobbyState as S};
        let mut lobby = Lobby::new();
        assert_eq!(lobby.state(), S::Idle);
        assert_eq!(lobby.apply(E::Start), S::Joining);
        assert_eq!(lobby.apply(E::Placed), S::Joining);
        assert_eq!(lobby.apply(E::LobbySignal), S::Lobby);
        assert_eq!(lobby.apply(E::Admit), S::Admitted);
        // Terminal states ignore progress; reset returns to idle.
        assert_eq!(lobby.apply(E::Reject), S::Admitted);
        assert_eq!(lobby.reset(), S::Idle);
        // The free function matches the holder.
        assert_eq!(lobby_step(S::Idle, E::Start), S::Joining);
        assert_eq!(lobby_step(S::Joining, E::Reject), S::Failed);
        assert_eq!(lobby_step(S::Failed, E::Start), S::Joining);
        assert_eq!(lobby_step(S::Lobby, E::Reset), S::Idle);
    }

    #[test]
    fn calendar_is_send_and_sync_and_debug_is_redacting() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Calendar>();
        let (_store, calendar) = test_calendar("http://127.0.0.1:9");
        let dbg = format!("{calendar:?}");
        assert!(dbg.contains("Calendar"));
        assert!(dbg.contains("default"));
    }

    #[test]
    fn cached_upcoming_never_touches_the_network() {
        // Port 9 is unreachable; the cached read must not connect.
        let (store, calendar) = test_calendar("http://127.0.0.1:9");
        store
            .upsert_meetings(
                "default",
                &[Meeting {
                    id: "E1".into(),
                    subject: "Later".into(),
                    start: Some("2999-01-01T00:00:00".into()),
                    end: None,
                    all_day: false,
                    organizer: None,
                    organizer_email: None,
                    is_organizer: false,
                    join_url: None,
                    join_meeting_id: None,
                    passcode: None,
                    web_link: None,
                    location: None,
                    is_online: true,
                }],
            )
            .unwrap();
        assert_eq!(calendar.cached_upcoming(10).unwrap().len(), 1);
        assert_eq!(calendar.profile(), "default");
        assert_eq!(calendar.client().request_count(), 0);
    }
}
