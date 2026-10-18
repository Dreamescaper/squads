//! People (directory) search service (backlog item **B3**).
//!
//! Wraps the vendored Graph `/users?$search="displayName:…"` call
//! ([`ost::api::search_people_data`]) into the owned [`Person`] model, so the
//! app's **people picker** — and, later, **B2a**'s "new chat" flow — can search
//! the directory without touching the wire projection.
//!
//! ```text
//!        UI (people picker / B2a new chat)
//!         │  search(query, limit)
//!         ▼
//!       People ──▶ Arc<ApiClient> ── Graph /users?$search=… ──▶ Person
//! ```
//!
//! ## Single window (bounded, no paging)
//!
//! The vendored helpers expose **no** paging cursor, so this is deliberately a
//! single bounded call: `limit` is clamped by the vendored path builder to
//! Graph's `1..=25` window ([`FIND_MAX_LIMIT`]) and one call returns at most
//! that many people. There is no `load_more`; the picker simply re-searches as
//! the user refines the query. An empty/whitespace query is rejected by the
//! vendored call before any network.
//!
//! ## Graph + `ConsistencyLevel`
//!
//! The people search is a **Graph** call (the Graph token) and the vendored
//! helper sends `ConsistencyLevel: eventual` — Graph answers `400` for a
//! `$search` request without it. Both facts are pinned by the offline stub
//! tests.

use std::sync::Arc;

use crate::api::ApiClient;
use crate::error::Result;
use crate::model::Person;

/// Default number of people a search asks Graph for (Graph caps at
/// [`FIND_MAX_LIMIT`](crate::api::FIND_MAX_LIMIT) = 25).
pub const PEOPLE_LIMIT: usize = 25;

/// The people-search service (see the module docs).
pub struct People {
    profile: String,
    client: Arc<ApiClient>,
}

impl People {
    /// Build the service over the shared protocol client.
    pub fn new(profile: impl Into<String>, client: Arc<ApiClient>) -> Self {
        Self {
            profile: profile.into(),
            client,
        }
    }

    /// The account profile this service was built for (carried for consistency
    /// with the other services; the directory search itself is profile-agnostic).
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The shared protocol client.
    pub fn client(&self) -> &Arc<ApiClient> {
        &self.client
    }

    /// Search the directory by display name, mapping the wire rows into owned
    /// [`Person`]s.
    ///
    /// Bounded and single-window: `limit` is clamped to Graph's `1..=25` window
    /// by the vendored path builder (there is no paging cursor). An
    /// empty/whitespace query is rejected before any network.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<Person>> {
        let wire = self.client.search_people(query, limit).await?;
        Ok(wire.iter().map(Person::from_member).collect())
    }
}

/// Redacting `Debug`: shows the profile and client presence, never a query or a
/// result.
impl std::fmt::Debug for People {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("People")
            .field("profile", &self.profile)
            .finish_non_exhaustive()
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

    fn requests(seen: &std::sync::Arc<Mutex<Vec<String>>>) -> Vec<String> {
        seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn test_people(base: &str) -> People {
        let client = Arc::new(ApiClient::from_tokens(&tokens_for(base)));
        People::new("default", client)
    }

    #[tokio::test]
    async fn search_hits_the_directory_with_consistency_level_and_parses() {
        let body = serde_json::json!({"value": [
            {"id": "u1", "displayName": "Ava Lindqvist",
             "mail": "ava@contoso.com", "userPrincipalName": "ava.upn@contoso.com"},
            {"id": "u2", "displayName": "Tom Becker",
             "mail": null, "userPrincipalName": "tom@contoso.com"},
            {"displayName": "no-id-skipped"}
        ]})
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let people = test_people(&base);

        let results = people.search("ava", 10).await.expect("search");
        assert_eq!(results.len(), 2, "the id-less row is skipped");
        assert_eq!(results[0].id, "u1");
        assert_eq!(results[0].display_name, "Ava Lindqvist");
        assert_eq!(results[0].email.as_deref(), Some("ava@contoso.com"));
        assert!(results[0].job_title.is_none());
        // mail absent -> the vendored mapping folds in userPrincipalName.
        assert_eq!(results[1].email.as_deref(), Some("tom@contoso.com"));
        assert_eq!(people.client().request_count(), 1);

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(
            req.starts_with(
                "GET /users?$search=%22displayName:ava%22&$select=id,displayName,mail,userPrincipalName&$top=10 "
            ),
            "unexpected request line: {req}"
        );
        // `$search` requires ConsistencyLevel: eventual, and it is a Graph call.
        assert!(req.contains("consistencylevel: eventual"), "{req}");
        assert!(req.contains("authorization: Bearer graph-token"), "{req}");
    }

    #[tokio::test]
    async fn search_clamps_the_limit_to_the_single_window() {
        // The vendored path clamps 100 -> 25 (FIND_MAX_LIMIT); no paging.
        let body = serde_json::json!({"value": []}).to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let people = test_people(&base);
        assert!(people.search("x", 100).await.unwrap().is_empty());

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(req.contains("$top=25 "), "limit must clamp to 25: {req}");
        assert_eq!(crate::api::FIND_MAX_LIMIT, 25);
    }

    #[tokio::test]
    async fn search_rejects_an_empty_query_before_network() {
        // Port 9 is unreachable; the vendored guard must fire first, so the
        // error is "empty query" and not a connection failure.
        let people = test_people("http://127.0.0.1:9");
        let err = people.search("", 10).await.unwrap_err();
        assert!(format!("{err}").contains("empty query"), "err = {err}");
        let err = people.search("   ", 10).await.unwrap_err();
        assert!(format!("{err}").contains("empty query"), "err = {err}");
    }

    #[tokio::test]
    async fn search_tolerates_unknown_shapes() {
        for raw in [serde_json::json!({}), serde_json::json!(null)] {
            let (base, _seen, task) = http_stub(vec![StubResponse::ok(raw.to_string())]).await;
            let people = test_people(&base);
            assert!(people.search("x", 5).await.unwrap().is_empty());
            task.await.expect("stub drains");
        }
    }

    #[test]
    fn people_is_send_and_sync_and_debug_is_redacting() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<People>();
        let people = test_people("http://127.0.0.1:9");
        let dbg = format!("{people:?}");
        assert!(dbg.contains("People"));
        assert!(dbg.contains("default"));
    }
}
