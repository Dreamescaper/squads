//! Local-first message search (backlog item **B1**).
//!
//! Teams' Graph `POST /search/query` (entity `chatMessage`, see
//! [`ost::api::search_messages_data`]) needs the `Chat.Read` delegated scope,
//! which the Teams first-party sign-in token does **not** carry, so it is
//! expected to 403 live (the vendored crate's own notes flag it as unproven).
//! The store, however, already holds the cached history — so the **primary**
//! path here searches the local SQLite store (a synchronous, cheap `LIKE` scan
//! over an indexed column; see [`crate::store::Store::search_messages`]) and the
//! server call is an **optional secondary** that is never required (see
//! ADR-036).
//!
//! ```text
//!        UI
//!         │  local(query)                (sync, offline, primary)
//!         ▼
//!      Search ──▶ Arc<Store>   ── LIKE scan ──▶ SearchHit (snippet + chat name)
//!         └────▶ Arc<ApiClient>  (async, optional)  Graph /search/query
//! ```
//!
//! ## Pure helpers
//!
//! [`escape_like`] / [`like_pattern`] turn arbitrary user input into a literal
//! `LIKE … ESCAPE '\'` pattern (so a query containing `%`/`_`/`\` cannot broaden
//! the match), and [`snippet`] builds the short excerpt centred on the match.
//! Both are pure and unit-tested.

use std::sync::Arc;

use crate::api::ApiClient;
use crate::error::{CoreError, Result};
use crate::model::SearchHit;
use crate::store::{SearchRow, Store};

/// How many characters a [`snippet`] shows before it is ellipsised.
pub const SNIPPET_LEN: usize = 120;

/// Default cap on the number of hits a search returns.
pub const SEARCH_LIMIT: usize = 50;

/// Escape the SQL `LIKE` metacharacters (`%`, `_`) and the escape character
/// itself (`\`), so user input matches literally.
///
/// The backslash is escaped **first** (in the same pass, character by
/// character) so an input `\` becomes `\\` and never consumes the escape of a
/// following `%`/`_`. Pair this with a query of the form
/// `… LIKE '%' || ? || '%' ESCAPE '\'` (or [`like_pattern`]).
pub fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// The `%…%` substring `LIKE` pattern for `query`, with every metacharacter
/// escaped (see [`escape_like`]).
pub fn like_pattern(query: &str) -> String {
    format!("%{}%", escape_like(query))
}

/// Build a plain-text snippet of `content` centred on the first
/// case-insensitive occurrence of `query`.
///
/// The window is at most `max_len` characters; when it does not start at the
/// beginning / end of `content` an ellipsis (`…`) is prepended / appended. With
/// no match (e.g. a server-side summary that has already trimmed the match away)
/// the snippet is the first `max_len` characters. Pure and total: it counts
/// `char`s, never bytes, so it never panics on a unicode boundary, and an empty
/// `content` (or `max_len == 0`) yields an empty string.
pub fn snippet(content: &str, query: &str, max_len: usize) -> String {
    let content = content.trim();
    if content.is_empty() || max_len == 0 {
        return String::new();
    }
    let chars: Vec<char> = content.chars().collect();
    let total = chars.len();

    let (start, end) = match find_ci(&chars, query.trim()) {
        Some((match_start, match_end)) => {
            let match_len = match_end - match_start;
            let pad = max_len.saturating_sub(match_len) / 2;
            let mut start = match_start.saturating_sub(pad);
            let mut end = (start + max_len).min(total);
            // If the window ran off the end, shift it left so it stays full.
            if end - start < max_len {
                start = end.saturating_sub(max_len);
            }
            end = end.max(match_end.min(total));
            (start, end)
        }
        None => (0, max_len.min(total)),
    };

    let mut out = String::with_capacity(end - start + 2);
    if start > 0 {
        out.push('…');
    }
    out.extend(chars[start..end].iter());
    if end < total {
        out.push('…');
    }
    out
}

/// The first case-insensitive (ASCII, matching SQLite's `LIKE`) occurrence of
/// `query` in `chars`, as a `(start, end)` char-index range.
fn find_ci(chars: &[char], query: &str) -> Option<(usize, usize)> {
    let needle: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    if needle.is_empty() || needle.len() > chars.len() {
        return None;
    }
    let hay: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
    (0..=chars.len() - needle.len())
        .find(|&i| hay[i..i + needle.len()] == needle[..])
        .map(|i| (i, i + needle.len()))
}

/// The local-first message-search service (see the module docs).
///
/// Built over the shared [`Store`] (always) and, when the session is signed in,
/// the shared [`ApiClient`] (for the optional server fallback). A signed-out
/// handle is perfectly usable: [`Search::local`] and [`Search::search`] return
/// cached results with no network at all.
pub struct Search {
    profile: String,
    store: Arc<Store>,
    client: Option<Arc<ApiClient>>,
}

impl Search {
    /// Build a **local-only** search over `store` (no server fallback).
    pub fn new(profile: impl Into<String>, store: Arc<Store>) -> Self {
        Self {
            profile: profile.into(),
            store,
            client: None,
        }
    }

    /// Attach the protocol client that backs the optional server fallback.
    pub fn with_client(mut self, client: Arc<ApiClient>) -> Self {
        self.client = Some(client);
        self
    }

    /// The account profile this search is scoped to.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The shared local store.
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// The protocol client, when one is attached.
    pub fn client(&self) -> Option<&Arc<ApiClient>> {
        self.client.as_ref()
    }

    /// Whether the optional server fallback is available.
    pub fn has_client(&self) -> bool {
        self.client.is_some()
    }

    /// Search the **local** store, newest first, resolving each hit's chat name
    /// and building a [`snippet`].
    ///
    /// Synchronous and offline — the UI calls this directly (it is cheap). An
    /// empty/whitespace query (or `limit == 0`) returns no rows, never
    /// "everything".
    pub fn local(
        &self,
        query: &str,
        chat_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let rows = self
            .store
            .search_messages(&self.profile, query, chat_id, limit)?;
        Ok(rows
            .into_iter()
            .map(|row| self.hit_from_row(row, query))
            .collect())
    }

    /// Search the **server** (Graph `/search/query`, entity `chatMessage`).
    ///
    /// Optional: this is expected to 403 live because the Teams sign-in token
    /// does not carry `Chat.Read` (see the module docs and ADR-036). Returns
    /// [`CoreError::NotSignedIn`] when no client is attached.
    pub async fn server(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let client = self.client.as_ref().ok_or(CoreError::NotSignedIn)?;
        let page = client.search_messages(query, limit).await?;
        Ok(page
            .hits
            .into_iter()
            .map(|info| self.hit_from_info(info, query))
            .collect())
    }

    /// Search **local-first**: return the cached hits, and only when the local
    /// store has none fall back to the (optional) server. A server error is
    /// logged and swallowed — the feature never depends on the server path.
    pub async fn search(
        &self,
        query: &str,
        chat_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let local = self.local(query, chat_id, limit)?;
        if !local.is_empty() || self.client.is_none() {
            return Ok(local);
        }
        match self.server(query, limit).await {
            Ok(hits) => Ok(hits),
            Err(error) => {
                tracing::debug!(%error, "server message search unavailable");
                Ok(local)
            }
        }
    }

    /// The cached display name of `chat_id` (the id itself when not cached).
    fn chat_name(&self, chat_id: &str) -> String {
        self.store
            .chat(chat_id, &self.profile)
            .ok()
            .flatten()
            .map(|chat| chat.name)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| chat_id.to_string())
    }

    fn hit_from_row(&self, row: SearchRow, query: &str) -> SearchHit {
        SearchHit {
            chat_name: self.chat_name(&row.chat_id),
            snippet: snippet(&row.content, query, SNIPPET_LEN),
            chat_id: row.chat_id,
            message_id: row.message_id,
            sender: row.sender,
            timestamp: row.timestamp,
            is_own: row.is_own,
        }
    }

    fn hit_from_info(&self, info: ost::api::SearchHitInfo, query: &str) -> SearchHit {
        SearchHit {
            chat_name: self.chat_name(&info.chat_id),
            snippet: snippet(&info.preview, query, SNIPPET_LEN),
            chat_id: info.chat_id,
            message_id: info.message_id,
            sender: info.sender,
            timestamp: info.timestamp,
            // A server hit carries neither an own-message flag nor a sender MRI
            // to compare against, so it is never marked own.
            is_own: false,
        }
    }
}

/// Redacting `Debug`: shows the profile and whether a client is attached, never
/// the query or the hits.
impl std::fmt::Debug for Search {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Search")
            .field("profile", &self.profile)
            .field("has_client", &self.has_client())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{StubResponse, http_stub};
    use crate::auth::tokens::{StoredToken, TokenSet};
    use crate::model::{Message, SendState};

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

    fn message(chat_id: &str, id: &str, sender: &str, content: &str, ts: &str) -> Message {
        Message {
            id: id.into(),
            chat_id: chat_id.into(),
            sender_mri: format!("8:orgid:{sender}"),
            sender_name: sender.into(),
            timestamp: ts.into(),
            content: content.into(),
            raw: format!("<p>{content}</p>"),
            reactions: Vec::new(),
            reply_to: None,
            client_message_id: None,
            edited: false,
            is_own: false,
            send_state: SendState::Sent,
        }
    }

    fn chat(id: &str, name: &str) -> crate::model::Chat {
        crate::model::Chat {
            id: id.into(),
            name: name.into(),
            is_group: false,
            last_message_time: None,
            last_message_sender: None,
            last_message_preview: None,
            unread: false,
            pinned: false,
        }
    }

    // ----- pure helpers ----------------------------------------------------

    #[test]
    fn escape_like_escapes_metacharacters_and_the_escape_char() {
        assert_eq!(escape_like("plain"), "plain");
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("a\\b"), "a\\\\b");
        // A backslash before a wildcard does not swallow the wildcard's escape.
        assert_eq!(escape_like("\\%"), "\\\\\\%");
        assert_eq!(escape_like(""), "");
    }

    #[test]
    fn like_pattern_wraps_the_escaped_query() {
        assert_eq!(like_pattern("ship"), "%ship%");
        assert_eq!(like_pattern("50%_x"), "%50\\%\\_x%");
    }

    #[test]
    fn snippet_is_returned_whole_when_it_fits() {
        assert_eq!(snippet("hello world", "world", 120), "hello world");
        // Trims surrounding whitespace.
        assert_eq!(snippet("  hello  ", "hello", 120), "hello");
    }

    #[test]
    fn snippet_centres_on_the_match_and_ellipsises() {
        let content = format!("{}NEEDLE{}", "a".repeat(200), "b".repeat(200));
        let out = snippet(&content, "needle", 40);
        assert!(out.contains("NEEDLE"), "the match is kept: {out}");
        assert!(out.starts_with('…'), "a left ellipsis is added: {out}");
        assert!(out.ends_with('…'), "a right ellipsis is added: {out}");
        // At most max_len content chars plus the two ellipses.
        assert!(out.chars().count() <= 42, "bounded: {out}");
    }

    #[test]
    fn snippet_truncates_when_there_is_no_match() {
        let content = "x".repeat(300);
        let out = snippet(&content, "absent", 20);
        assert_eq!(out, format!("{}…", "x".repeat(20)));
        assert_eq!(snippet("", "x", 20), "");
        assert_eq!(snippet("abc", "a", 0), "");
    }

    #[test]
    fn snippet_is_unicode_safe_and_matches_ascii_case_insensitively() {
        let content = "Café ☕ 日本語 😀 and a Needle here";
        let out = snippet(content, "NEEDLE", 20);
        assert!(out.contains("Needle"), "{out}");
        // A unicode query is found exactly and never panics on a boundary.
        let out = snippet("préfixe 日本語 suffixe", "日本語", 8);
        assert!(out.contains("日本語"), "{out}");
        // Counting chars (not bytes) keeps the bound for multi-byte content.
        assert!(snippet(&"é".repeat(300), "absent", 30).chars().count() <= 31);
    }

    // ----- the local service ----------------------------------------------

    #[test]
    fn local_resolves_chat_names_and_builds_snippets() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .upsert_chats("default", &[chat("c1", "Alice")])
            .unwrap();
        store
            .upsert_messages(
                "default",
                &[message(
                    "c1",
                    "m1",
                    "Alice",
                    "the ship has sailed",
                    "2026-01-01T00:00:00Z",
                )],
            )
            .unwrap();

        let search = Search::new("default", Arc::clone(&store));
        assert!(!search.has_client());
        let hits = search.local("ship", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chat_name, "Alice");
        assert_eq!(hits[0].message_id, "m1");
        assert_eq!(hits[0].sender, "Alice");
        assert_eq!(hits[0].snippet, "the ship has sailed");
        assert!(!hits[0].is_own);

        // An uncached chat falls back to the id as its name.
        store
            .upsert_messages(
                "default",
                &[message(
                    "c9",
                    "m9",
                    "Bob",
                    "ship it",
                    "2026-01-02T00:00:00Z",
                )],
            )
            .unwrap();
        let hits = search.local("ship", None, 10).unwrap();
        assert_eq!(hits[0].chat_id, "c9");
        assert_eq!(hits[0].chat_name, "c9");
    }

    #[test]
    fn local_is_empty_for_a_blank_query() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .upsert_messages(
                "default",
                &[message(
                    "c1",
                    "m1",
                    "Alice",
                    "anything",
                    "2026-01-01T00:00:00Z",
                )],
            )
            .unwrap();
        let search = Search::new("default", store);
        assert!(search.local("", None, 10).unwrap().is_empty());
        assert!(search.local("   ", None, 10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn search_returns_local_without_touching_the_server() {
        // No stub is running: a local hit must short-circuit before any request.
        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .upsert_messages(
                "default",
                &[message(
                    "c1",
                    "m1",
                    "Alice",
                    "local hit",
                    "2026-01-01T00:00:00Z",
                )],
            )
            .unwrap();
        let client = Arc::new(ApiClient::from_tokens(&tokens_for("http://127.0.0.1:9")));
        let search = Search::new("default", store).with_client(client);
        let hits = search.search("hit", None, 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "local hit");
    }

    #[tokio::test]
    async fn search_falls_back_to_the_server_when_local_is_empty() {
        let body = serde_json::json!({
            "value": [{ "hitsContainers": [{
                "hits": [{
                    "summary": "…Ship it…",
                    "resource": {
                        "id": "1758600000000",
                        "createdDateTime": "2026-09-22T09:12:05Z",
                        "from": {"emailAddress": {"name": "Megan Harper"}},
                        "chatId": "19:chat1@thread.v2"
                    }
                }],
                "total": 1,
                "moreResultsAvailable": false
            }] }]
        })
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;

        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .upsert_chats("default", &[chat("19:chat1@thread.v2", "Megan")])
            .unwrap();
        let client = Arc::new(ApiClient::from_tokens(&tokens_for(&base)));
        let search = Search::new("default", store).with_client(client);

        let hits = search.search("ship", None, 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id, "1758600000000");
        assert_eq!(hits[0].chat_name, "Megan");
        assert_eq!(hits[0].sender, "Megan Harper");
        assert!(!hits[0].is_own);

        task.await.expect("stub drains");
        let req = seen.lock().unwrap_or_else(|e| e.into_inner())[0].clone();
        assert!(req.starts_with("POST /search/query "), "unexpected: {req}");
        assert!(req.contains("\"entityTypes\":[\"chatMessage\"]"), "{req}");
        assert!(req.contains("\"queryString\":\"ship\""), "{req}");
    }

    #[tokio::test]
    async fn server_without_a_client_is_not_signed_in() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let search = Search::new("default", store);
        let err = search.server("x", 10).await.unwrap_err();
        assert!(matches!(err, CoreError::NotSignedIn), "got {err:?}");
        // And `search` still succeeds (empty) with no client.
        assert!(search.search("x", None, 10).await.unwrap().is_empty());
    }

    #[test]
    fn search_is_send_and_sync_and_debug_is_redacting() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Search>();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let search = Search::new("default", store);
        let dbg = format!("{search:?}");
        assert!(dbg.contains("Search"));
        assert!(dbg.contains("default"));
        assert!(dbg.contains("has_client"));
    }
}
