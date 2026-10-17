//! Protocol clients (milestone **M2**).
//!
//! This module is the thin, UI-free facade over the vendored protocol crate
//! (ADR-001): we **wrap** `ost` rather than reimplement it. Two things live
//! here:
//!
//! - [`tokens_to_config`] — the pure bridge from our M1 [`TokenSet`] into the
//!   vendored `ost::config::Config` the client is built from.
//! - [`ApiClient`] — owns an `ost::api::client::TeamsClient` built with
//!   `TeamsClient::from_config` (ADR-010), exposing typed calls
//!   ([`ApiClient::list_chats`], [`ApiClient::read_messages`]), region-routing
//!   accessors and a light request counter.
//!
//! Constructing an [`ApiClient`] never touches the network: `from_config` only
//! stores the in-memory tokens and the shared HTTP pool is lazy. Network access
//! happens on the first `list_chats` / `read_messages` call, which is why the
//! exit-criterion test drives the real code path against a local TCP stub.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use ost::api::ChatInfo;
use ost::api::MessageInfo;
use ost::api::client::TeamsClient;
use ost::config::Config;

use crate::auth::tokens::{StoredToken, TokenSet};
use crate::error::Result;

/// Map our M1 [`TokenSet`] into the vendored [`ost::config::Config`].
///
/// Pure and offline. Each token carries our **absolute** `expires_at` straight
/// through (we deliberately do not use `StoredToken::new`, which would recompute
/// the deadline from a lifetime and shift it by the call time). `region_gtms`
/// is stored as its JSON string (the vendored `Config` keeps it as a string for
/// TOML compatibility) and is `None` when our blob is JSON `null`.
pub fn tokens_to_config(tokens: &TokenSet) -> Config {
    // Absolute-deadline conversion: `ost::auth::StoredToken` also has a
    // five-minute skew in `is_expired`, matching our own expiry rule.
    let stored = |t: &StoredToken| ost::auth::StoredToken {
        token: t.token.clone(),
        expires_at: Some(t.expires_at),
    };

    Config {
        access_token: Some(stored(&tokens.spaces)),
        refresh_token: Some(tokens.refresh_token.clone()),
        tenant_id: Some(tokens.tenant.clone()),
        skype_token: Some(stored(&tokens.skype)),
        graph_token: Some(stored(&tokens.graph)),
        ic3_token: Some(stored(&tokens.ic3)),
        recorder_token: Some(stored(&tokens.recorder)),
        region_gtms: if tokens.region_gtms.is_null() {
            None
        } else {
            Some(tokens.region_gtms.to_string())
        },
        teams_partition: None,
        teams_region: None,
        teams_ring: None,
    }
}

/// A typed, instrumented facade over the vendored Teams client.
///
/// Holds one `ost::api::client::TeamsClient` (built from an in-memory config)
/// plus the `regionGtms` routing blob and a request counter. `Debug` is
/// redacting: it never reveals the tokens the client is holding.
pub struct ApiClient {
    client: TeamsClient,
    region_gtms: serde_json::Value,
    /// Number of high-level calls issued (list_chats etc.).
    requests: AtomicU64,
}

impl ApiClient {
    /// Build a client from our token set. Offline: no request is made here.
    pub fn from_tokens(tokens: &TokenSet) -> Self {
        Self::from_config(tokens_to_config(tokens))
    }

    /// Build a client over an explicit vendored [`Config`] (tests, dynamic
    /// region overrides). Offline: no request is made here.
    pub fn from_config(config: Config) -> Self {
        let region_gtms = config.get_region_gtms().unwrap_or(serde_json::Value::Null);
        Self {
            client: TeamsClient::from_config(config),
            region_gtms,
            requests: AtomicU64::new(0),
        }
    }

    /// The `regionGtms` routing blob this client was built with (`null` when
    /// the token set carried none).
    pub fn region_gtms(&self) -> &serde_json::Value {
        &self.region_gtms
    }

    /// Resolved chat-service base URL (`region_gtms.chatService` or the
    /// regional default). Handy for diagnostics and later routing work.
    pub fn chat_service_url(&self) -> String {
        self.client.chat_service_url()
    }

    /// Resolved chat-service aggregator base URL.
    pub fn chatsvcagg_url(&self) -> String {
        self.client.chatsvcagg_url()
    }

    /// Resolved apps-platform middle-tier base URL.
    pub fn middle_tier_url(&self) -> String {
        self.client.middle_tier_url()
    }

    /// How many high-level calls this client has served (cheap instrumentation;
    /// the per-request observer lives in the vendored crate).
    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    fn counted(&self) -> u64 {
        self.requests.fetch_add(1, Ordering::Relaxed)
    }

    /// List recent chats (native CSA → chatsvcagg → chat-service fallback,
    /// exactly as the vendored crate implements it).
    pub async fn list_chats(&self, limit: usize) -> Result<Vec<ChatInfo>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span = tracing::debug_span!(target: "squads_core::api", "api.list_chats", limit, call);
        let chats = ost::api::list_chats_data(&self.client, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = chats.len(), call, "listed chats");
        Ok(chats)
    }

    /// Read the newest page of messages for `chat_id`.
    pub async fn read_messages(&self, chat_id: &str, limit: usize) -> Result<Vec<MessageInfo>> {
        use tracing::Instrument as _;
        let call = self.counted();
        let span =
            tracing::debug_span!(target: "squads_core::api", "api.read_messages", limit, call);
        let messages = ost::api::read_messages_data(&self.client, chat_id, limit)
            .instrument(span)
            .await?;
        tracing::debug!(count = messages.len(), call, "read messages");
        Ok(messages)
    }
}

impl fmt::Debug for ApiClient {
    /// Redacted: shows routing presence and call count, never token contents.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiClient")
            .field("region_gtms", &!self.region_gtms.is_null())
            .field("chat_service_url", &self.chat_service_url())
            .field("requests", &self.request_count())
            .finish()
    }
}

/// Serve exactly one HTTP request: capture it, reply `200` with `body`.
///
/// Test-only helper shared with the session tests so both drive the same
/// offline path. The captured request head lets callers assert what the client
/// sent (auth header, path) without a live tenant.
#[cfg(test)]
pub(crate) async fn one_shot_json_stub(
    body: String,
) -> (
    String,
    std::sync::Arc<std::sync::Mutex<String>>,
    tokio::task::JoinHandle<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("stub binds");
    let base = format!("http://{}", listener.local_addr().expect("stub addr"));
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let log = std::sync::Arc::clone(&seen);
    let task = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = sock.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if String::from_utf8_lossy(&buf).contains("\r\n\r\n") {
                break;
            }
        }
        *log.lock().unwrap_or_else(|e| e.into_inner()) = String::from_utf8_lossy(&buf).to_string();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        sock.write_all(resp.as_bytes()).await.ok();
        sock.shutdown().await.ok();
    });
    (base, seen, task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::tokens::{StoredToken, TokenKind};

    fn sample_tokens(region_gtms: serde_json::Value) -> TokenSet {
        TokenSet {
            refresh_token: "refresh-secret".into(),
            tenant: "contoso.onmicrosoft.com".into(),
            region_gtms,
            personal: false,
            spaces: StoredToken::with_expiry("spaces-token", 1_700_000_001),
            skype: StoredToken::with_expiry("skype-token", 1_700_000_002),
            graph: StoredToken::with_expiry("graph-token", 1_700_000_003),
            ic3: StoredToken::with_expiry("ic3-token", 1_700_000_004),
            recorder: StoredToken::with_expiry("recorder-token", 1_700_000_005),
        }
    }

    /// Like [`sample_tokens`] but with live (non-expired) deadlines, for tests
    /// that drive the real transport (which refuses expired tokens).
    fn fresh_tokens(region_gtms: serde_json::Value) -> TokenSet {
        let future = crate::auth::tokens::now_unix() + 3600;
        let mk = |name: &str| StoredToken::with_expiry(name, future);
        TokenSet {
            refresh_token: "refresh-secret".into(),
            tenant: "contoso.onmicrosoft.com".into(),
            region_gtms,
            personal: false,
            spaces: mk("spaces-token"),
            skype: mk("skype-token"),
            graph: mk("graph-token"),
            ic3: mk("ic3-token"),
            recorder: mk("recorder-token"),
        }
    }

    #[test]
    fn tokens_to_config_maps_every_field() {
        let region = serde_json::json!({
            "csaBase": "http://127.0.0.1:9",
            "chatService": "https://emea.ng.msg.teams.microsoft.com",
            "region": "emea"
        });
        let cfg = tokens_to_config(&sample_tokens(region.clone()));

        fn at(t: &Option<ost::auth::StoredToken>) -> &ost::auth::StoredToken {
            t.as_ref().expect("token present")
        }
        // Absolute deadlines are carried through verbatim (not recomputed).
        assert_eq!(at(&cfg.access_token).token, "spaces-token");
        assert_eq!(at(&cfg.access_token).expires_at, Some(1_700_000_001));
        assert_eq!(at(&cfg.skype_token).token, "skype-token");
        assert_eq!(at(&cfg.skype_token).expires_at, Some(1_700_000_002));
        assert_eq!(at(&cfg.graph_token).token, "graph-token");
        assert_eq!(at(&cfg.graph_token).expires_at, Some(1_700_000_003));
        assert_eq!(at(&cfg.ic3_token).token, "ic3-token");
        assert_eq!(at(&cfg.ic3_token).expires_at, Some(1_700_000_004));
        assert_eq!(at(&cfg.recorder_token).token, "recorder-token");
        assert_eq!(at(&cfg.recorder_token).expires_at, Some(1_700_000_005));

        assert_eq!(cfg.refresh_token.as_deref(), Some("refresh-secret"));
        assert_eq!(cfg.tenant_id.as_deref(), Some("contoso.onmicrosoft.com"));

        // The JSON string round-trips back through the vendored accessor.
        assert_eq!(cfg.get_region_gtms(), Some(region));
    }

    #[test]
    fn null_region_gtms_maps_to_none() {
        let cfg = tokens_to_config(&sample_tokens(serde_json::Value::Null));
        assert!(cfg.region_gtms.is_none());
        assert!(cfg.get_region_gtms().is_none());
    }

    #[test]
    fn client_is_offline_until_called_and_debug_is_redacted() {
        let cfg = tokens_to_config(&sample_tokens(serde_json::json!({"region": "emea"})));
        let client = ApiClient::from_config(cfg);
        assert_eq!(client.request_count(), 0);
        assert_eq!(client.region_gtms(), &serde_json::json!({"region": "emea"}));
        // Routing accessors fall back to the regional defaults.
        assert!(
            client
                .chat_service_url()
                .contains("ng.msg.teams.microsoft.com")
        );
        let dbg = format!("{client:?}");
        assert!(!dbg.contains("spaces-token") && !dbg.contains("graph-token"));
    }

    #[test]
    fn region_gtms_can_override_service_urls() {
        let cfg = tokens_to_config(&sample_tokens(serde_json::json!({
            "chatService": "http://127.0.0.1:1234/chat",
            "chatServiceAggregator": "http://127.0.0.1:1234/agg",
            "middleTier": "http://127.0.0.1:1234/mt"
        })));
        let client = ApiClient::from_config(cfg);
        assert_eq!(client.chat_service_url(), "http://127.0.0.1:1234/chat");
        assert_eq!(client.chatsvcagg_url(), "http://127.0.0.1:1234/agg");
        assert_eq!(client.middle_tier_url(), "http://127.0.0.1:1234/mt");
    }

    /// The M2 exit criterion: `list_chats` returns structured data from a local
    /// fake. Chats carry a `topic`, so the mate-name resolution (which would add
    /// a `whoami` + roster + history request) is never entered and the stub
    /// only has to answer the CSA conversations request.
    #[tokio::test]
    async fn list_chats_against_local_fake_returns_parsed_chats() {
        let body = serde_json::json!({
            "conversations": [
                {
                    "id": "19:alice@unq.gbl.spaces",
                    "threadProperties": { "topic": "Alice" },
                    "lastMessage": {
                        "imdisplayname": "Alice",
                        "content": "<p>hello <b>world</b></p>",
                        "originalarrivaltime": "2026-01-02T03:04:05Z"
                    }
                },
                {
                    "id": "19:team@thread.v2",
                    "threadProperties": { "topic": "Team sync" },
                    "lastMessage": {
                        "imdisplayname": "Bob",
                        "content": "<p>standup at 5</p>",
                        "originalarrivaltime": "2026-01-03T03:04:05Z"
                    }
                }
            ]
        })
        .to_string();
        let (base, seen, task) = one_shot_json_stub(body).await;

        let tokens = fresh_tokens(serde_json::json!({
            "csaBase": base,
            "graphBase": base,
            "chatService": base
        }));
        let client = ApiClient::from_tokens(&tokens);
        let chats = client.list_chats(20).await.expect("list chats");

        assert_eq!(chats.len(), 2, "two conversations parsed");
        assert_eq!(chats[0].id, "19:alice@unq.gbl.spaces");
        assert_eq!(chats[0].name, "Alice");
        assert!(!chats[0].is_group);
        assert_eq!(chats[0].last_message_sender.as_deref(), Some("Alice"));
        assert_eq!(
            chats[0].last_message_preview.as_deref(),
            Some("hello world")
        );
        assert_eq!(
            chats[0].last_message_time.as_deref(),
            Some("2026-01-02T03:04:05Z")
        );
        assert_eq!(chats[1].name, "Team sync");
        assert!(chats[1].is_group, "group thread id contains 'thread'");
        assert_eq!(client.request_count(), 1);

        task.await.expect("stub drains");
        let req = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            req.starts_with("GET /teams/users/ME/conversations?view=mychats&pageSize=20 "),
            "unexpected request line: {req}"
        );
        // The skype token from our set reached the transport.
        assert!(req.contains("authorization: Bearer skype-token"), "{req}");
    }

    #[tokio::test]
    async fn read_messages_against_local_fake_returns_parsed_messages() {
        let body = serde_json::json!({
            "messages": [
                {
                    "id": "1700000000000",
                    "imdisplayname": "Alice",
                    "content": "<p>hi</p>",
                    "messagetype": "RichText/Html",
                    "originalarrivaltime": "2026-01-02T03:04:05Z",
                    "from": "https://teams.microsoft.com/api/v1/users/ME/contacts/8:orgid:aaaa"
                }
            ],
            "_metadata": { "backwardLink": null }
        })
        .to_string();
        let (base, _seen, task) = one_shot_json_stub(body).await;

        let tokens = fresh_tokens(serde_json::json!({ "chatService": base }));
        let client = ApiClient::from_tokens(&tokens);
        let msgs = client
            .read_messages("19:alice@unq.gbl.spaces", 50)
            .await
            .expect("read messages");

        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].id, "1700000000000");
        assert_eq!(msgs[0].sender, "Alice");
        assert_eq!(msgs[0].sender_mri, "8:orgid:aaaa");
        assert_eq!(msgs[0].content, "hi");
        task.await.expect("stub drains");
    }

    #[test]
    fn stored_token_kind_values_are_distinct() {
        // Guard against a copy/paste regression in `tokens_to_config`: every
        // kind must map to its own token value.
        let set = sample_tokens(serde_json::json!({"r": 1}));
        let cfg = tokens_to_config(&set);
        let mut seen = std::collections::HashSet::new();
        for kind in TokenKind::ALL {
            let value = set.token(kind).token.clone();
            assert!(seen.insert(value.clone()), "duplicate token {value}");
        }
        assert_eq!(
            cfg.access_token.as_ref().map(|t| t.token.as_str()),
            Some("spaces-token")
        );
    }
}
