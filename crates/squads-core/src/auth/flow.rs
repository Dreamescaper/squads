//! Sign-in and refresh orchestration over a small transport abstraction.
//!
//! [`AuthDriver`] is generic over [`AuthTransport`], so the whole flow — device
//! code, Skype exchange, the three derived refresh grants and persistence — is
//! exercised in tests with a deterministic, offline fake ([`HttpTransport`] is
//! the only real-network implementation and is never run in tests).
//!
//! Progress is reported through a caller-supplied [`FnMut`] as
//! [`SignInProgress`] values; the driver never prints to stdout.
//!
//! Note: [`HttpTransport::exchange_skype`] delegates to the vendored crate's
//! `exchange_skype_token`, which always uses the vendored shared HTTP client.
//! That call therefore cannot be redirected to a local stub and is only
//! meaningful in the real sign-in path.

use std::future::Future;

use serde_json::Value;

use super::OAuthParams;
use super::device_code::{
    DeviceCodeStart, PollResult, SLOW_DOWN_INCREMENT_SECS, TokenResponse, get_u64,
    parse_poll_response,
};
use super::store::TokenStore;
use super::tokens::{StoredToken, TokenKind, TokenSet};
use crate::error::{CoreError, Result};

/// Default AAD login host; overridable in tests.
pub const DEFAULT_LOGIN_BASE: &str = "https://login.microsoftonline.com";

/// A token obtained from a refresh-token grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantedToken {
    /// The access token value. Never printed.
    pub token: String,
    /// A rotated refresh token, if the endpoint returned one.
    pub refresh_token: Option<String>,
    /// Lifetime in seconds.
    pub expires_in: Option<u64>,
}

/// Result of the `authsvc` Skype exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkypeExchange {
    /// The Skype token value. Never printed.
    pub token: String,
    /// Lifetime in seconds.
    pub expires_in: Option<u64>,
    /// `regionGtms` routing blob.
    pub region_gtms: Option<Value>,
}

/// The network operations the sign-in flow needs.
///
/// Implemented once for real HTTP ([`HttpTransport`]) and once per test as a
/// fake. Uses return-position `impl Future` (native async-in-trait) rather than
/// `async-trait`, and every future is `Send` so the driver can be spawned.
pub trait AuthTransport: Send + Sync {
    /// Request a device code from `…/{tenant}/oauth2/v2.0/devicecode`.
    fn request_device_code(
        &self,
        params: &OAuthParams,
    ) -> impl Future<Output = Result<DeviceCodeStart>> + Send;

    /// Poll the token endpoint with a device code.
    fn poll_device_code(
        &self,
        params: &OAuthParams,
        device_code: &str,
    ) -> impl Future<Output = Result<PollResult>> + Send;

    /// Exchange an AAD token for a Skype token (+ `regionGtms`).
    fn exchange_skype(
        &self,
        aad_token: &str,
        personal: bool,
    ) -> impl Future<Output = Result<SkypeExchange>> + Send;

    /// Refresh-token grant minting the token for `kind`.
    fn refresh_grant(
        &self,
        params: &OAuthParams,
        refresh_token: &str,
        kind: TokenKind,
    ) -> impl Future<Output = Result<GrantedToken>> + Send;
}

/// Progress emitted while signing in or refreshing.
///
/// The UI consumes these (it can map them onto [`SessionState`](crate::SessionState));
/// `Debug` never reveals token contents.
#[derive(Debug, Clone, PartialEq)]
pub enum SignInProgress {
    /// Requesting a device code from AAD.
    RequestingDeviceCode,
    /// Waiting for the user to enter the code in a browser.
    AwaitingUser {
        /// The short code the user types.
        user_code: String,
        /// The verification URL.
        verification_uri: String,
        /// AAD's human-readable instructions.
        message: String,
    },
    /// Polling the token endpoint.
    Polling,
    /// Exchanging the AAD token for a Skype token.
    ExchangingSkype,
    /// Minting one of the derived tokens.
    AcquiringToken(TokenKind),
    /// Writing the token set to the store.
    Persisting,
    /// Done; carries the complete set.
    Completed(TokenSet),
    /// A failure; carries a message safe to show (no secrets).
    Failed(String),
}

impl SignInProgress {
    /// A short hint for the `SigningIn` UI state.
    pub fn hint(&self) -> String {
        match self {
            SignInProgress::RequestingDeviceCode => "Requesting device code…".into(),
            SignInProgress::AwaitingUser { user_code, .. } => user_code.clone(),
            SignInProgress::Polling => "Waiting for sign-in…".into(),
            SignInProgress::ExchangingSkype => "Exchanging Skype token…".into(),
            SignInProgress::AcquiringToken(kind) => format!("Acquiring {}…", kind.label()),
            SignInProgress::Persisting => "Saving credentials…".into(),
            SignInProgress::Completed(_) => "Ready".into(),
            SignInProgress::Failed(_) => "Sign-in failed".into(),
        }
    }

    /// Whether this is a terminal progress value.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            SignInProgress::Completed(_) | SignInProgress::Failed(_)
        )
    }
}

/// The real, HTTP-backed transport.
///
/// Holds a shared `reqwest` client (the vendored crate's pool) and the AAD login
/// base URL. Never logs request bodies or tokens.
pub struct HttpTransport {
    http: reqwest::Client,
    login_base: String,
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransport {
    /// Build a transport using the same connection pool as the protocol crate.
    pub fn new() -> Self {
        Self {
            http: ost::api::client::shared_http(),
            login_base: DEFAULT_LOGIN_BASE.to_string(),
        }
    }

    /// Build a transport over an explicit client (default AAD base).
    pub fn with_client(http: reqwest::Client) -> Self {
        Self {
            http,
            login_base: DEFAULT_LOGIN_BASE.to_string(),
        }
    }

    /// Build a transport pointed at an alternate login base (tests only).
    #[doc(hidden)]
    pub fn with_login_base(base: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            login_base: base.into(),
        }
    }

    fn device_code_url(&self, tenant: &str) -> String {
        format!("{}/{tenant}/oauth2/v2.0/devicecode", self.login_base)
    }

    /// The AAD token endpoint for `tenant`. Equals
    /// `ost::auth::oauth::token_endpoint` for the default login base, but is
    /// overridable so tests never touch the network.
    fn token_url(&self, tenant: &str) -> String {
        format!("{}/{tenant}/oauth2/v2.0/token", self.login_base)
    }
}

impl AuthTransport for HttpTransport {
    fn request_device_code(
        &self,
        params: &OAuthParams,
    ) -> impl Future<Output = Result<DeviceCodeStart>> + Send {
        async move {
            let url = self.device_code_url(&params.tenant);
            let form = vec![
                ("client_id".to_string(), params.client_id.clone()),
                ("scope".to_string(), params.scope.clone()),
            ];
            let (status, body) = post_form_json(&self.http, &url, &form).await?;
            if !(200..300).contains(&status) {
                return Err(auth_http_error("device code request", status, &body));
            }
            DeviceCodeStart::from_json(&body)
        }
    }

    fn poll_device_code(
        &self,
        params: &OAuthParams,
        device_code: &str,
    ) -> impl Future<Output = Result<PollResult>> + Send {
        async move {
            let url = self.token_url(&params.tenant);
            let form = vec![
                (
                    "grant_type".to_string(),
                    "urn:ietf:params:oauth:grant-type:device_code".to_string(),
                ),
                ("client_id".to_string(), params.client_id.clone()),
                ("device_code".to_string(), device_code.to_string()),
            ];
            let (status, body) = post_form_json(&self.http, &url, &form).await?;
            parse_poll_response(status, &body)
        }
    }

    fn exchange_skype(
        &self,
        aad_token: &str,
        personal: bool,
    ) -> impl Future<Output = Result<SkypeExchange>> + Send {
        // Borrow into the async block; the token is never logged.
        let aad_token = aad_token.to_string();
        async move {
            let (token, expires_in, region_gtms) =
                ost::auth::skype::exchange_skype_token(&aad_token, personal)
                    .await
                    .map_err(|e| CoreError::Auth(format!("Skype token exchange failed: {e}")))?;
            Ok(SkypeExchange {
                token,
                expires_in,
                region_gtms,
            })
        }
    }

    fn refresh_grant(
        &self,
        params: &OAuthParams,
        refresh_token: &str,
        kind: TokenKind,
    ) -> impl Future<Output = Result<GrantedToken>> + Send {
        let refresh_token = refresh_token.to_string();
        async move {
            let scope = kind.scope().ok_or_else(|| {
                CoreError::Auth(format!("{} is not a refresh grant", kind.as_str()))
            })?;
            let url = self.token_url(&params.tenant);
            let form = vec![
                ("client_id".to_string(), params.client_id.clone()),
                ("grant_type".to_string(), "refresh_token".to_string()),
                ("refresh_token".to_string(), refresh_token),
                (
                    "scope".to_string(),
                    ost::auth::oauth::normalize_scopes(&format!("{scope} offline_access")),
                ),
            ];
            let (status, body) = post_form_json(&self.http, &url, &form).await?;
            if !(200..300).contains(&status) {
                return Err(auth_http_error("token refresh", status, &body));
            }
            Ok(GrantedToken {
                token: required_str(&body, "access_token", "token refresh")?,
                refresh_token: body
                    .get("refresh_token")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                expires_in: get_u64(body.get("expires_in")),
            })
        }
    }
}

/// POST a form and return `(status, json_body)`; a non-JSON body becomes `null`.
async fn post_form_json(
    http: &reqwest::Client,
    url: &str,
    form: &[(String, String)],
) -> Result<(u16, Value)> {
    let resp = http
        .post(url)
        .form(form)
        .send()
        .await
        .map_err(|e| CoreError::Auth(format!("token endpoint unreachable: {e}")))?;
    let status = resp.status().as_u16();
    let body = resp.json::<Value>().await.unwrap_or(Value::Null);
    Ok((status, body))
}

/// Secret-free HTTP error: AAD error code plus the first description line.
fn auth_http_error(what: &str, status: u16, body: &Value) -> CoreError {
    let code = body
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown_error");
    let desc = body
        .get("error_description")
        .and_then(|v| v.as_str())
        .and_then(|d| d.lines().next())
        .unwrap_or("");
    CoreError::Auth(format!("{what} failed (HTTP {status}): {code} {desc}"))
}

fn required_str(body: &Value, field: &str, what: &str) -> Result<String> {
    body.get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| CoreError::Auth(format!("{what} response missing '{field}'")))
}

/// Orchestrates sign-in and token refresh over an [`AuthTransport`].
pub struct AuthDriver<T: AuthTransport> {
    transport: T,
}

impl<T: AuthTransport> AuthDriver<T> {
    /// Wrap a transport.
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// The wrapped transport.
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Run the full device-code sign-in, persist the [`TokenSet`] and return it.
    ///
    /// Order: device code → poll → Skype exchange → Graph/IC3/Recorder refresh
    /// grants → save. Derived-token failures are fatal so the resulting set is
    /// always complete.
    pub async fn sign_in<F>(
        &self,
        use_personal: bool,
        profile: &str,
        store: &dyn TokenStore,
        mut on_progress: F,
    ) -> Result<TokenSet>
    where
        F: FnMut(SignInProgress) + Send,
    {
        let params = OAuthParams::for_account(use_personal);

        on_progress(SignInProgress::RequestingDeviceCode);
        let start = self.transport.request_device_code(&params).await?;
        on_progress(SignInProgress::AwaitingUser {
            user_code: start.user_code.clone(),
            verification_uri: start.verification_uri.clone(),
            message: start.message.clone(),
        });

        let token = self
            .poll_for_token(&params, &start, &mut on_progress)
            .await?;
        let refresh_token = token
            .refresh_token
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                CoreError::Auth("token response did not include a refresh token".into())
            })?;

        on_progress(SignInProgress::ExchangingSkype);
        let skype = self
            .transport
            .exchange_skype(&token.access_token, use_personal)
            .await?;

        let mut refresh_token = refresh_token;
        let graph = self
            .acquire(
                &params,
                &mut refresh_token,
                TokenKind::Graph,
                &mut on_progress,
            )
            .await?;
        let ic3 = self
            .acquire(
                &params,
                &mut refresh_token,
                TokenKind::Ic3,
                &mut on_progress,
            )
            .await?;
        let recorder = self
            .acquire(
                &params,
                &mut refresh_token,
                TokenKind::Recorder,
                &mut on_progress,
            )
            .await?;

        let set = TokenSet {
            refresh_token,
            tenant: params.tenant.clone(),
            region_gtms: skype.region_gtms.unwrap_or(Value::Null),
            personal: use_personal,
            spaces: StoredToken::from_lifetime(token.access_token, token.expires_in),
            skype: StoredToken::from_lifetime(skype.token, skype.expires_in),
            graph,
            ic3,
            recorder,
        };

        on_progress(SignInProgress::Persisting);
        store.save(profile, &set)?;
        on_progress(SignInProgress::Completed(set.clone()));
        Ok(set)
    }

    /// Re-mint **only** the expired tokens of `current`, persist and return the
    /// updated set. When nothing has expired this is a no-op.
    pub async fn refresh<F>(
        &self,
        current: &TokenSet,
        profile: &str,
        store: &dyn TokenStore,
        mut on_progress: F,
    ) -> Result<TokenSet>
    where
        F: FnMut(SignInProgress) + Send,
    {
        let expired = current.expired_kinds();
        if expired.is_empty() {
            return Ok(current.clone());
        }

        let params = OAuthParams::for_account(current.personal);
        let mut next = current.clone();
        let mut refresh_token = current.refresh_token.clone();

        // The Skype token is an exchange of the Spaces token, so re-mint Spaces
        // first when either is stale.
        if expired.contains(&TokenKind::Spaces) {
            next.spaces = self
                .acquire(
                    &params,
                    &mut refresh_token,
                    TokenKind::Spaces,
                    &mut on_progress,
                )
                .await?;
        }
        if expired.contains(&TokenKind::Skype) {
            on_progress(SignInProgress::ExchangingSkype);
            let exchange = self
                .transport
                .exchange_skype(&next.spaces.token, current.personal)
                .await?;
            next.skype = StoredToken::from_lifetime(exchange.token, exchange.expires_in);
            if let Some(gtms) = exchange.region_gtms {
                next.region_gtms = gtms;
            }
        }
        for kind in [TokenKind::Graph, TokenKind::Ic3, TokenKind::Recorder] {
            if expired.contains(&kind) {
                *next.token_mut(kind) = self
                    .acquire(&params, &mut refresh_token, kind, &mut on_progress)
                    .await?;
            }
        }

        next.refresh_token = refresh_token;
        on_progress(SignInProgress::Persisting);
        store.save(profile, &next)?;
        on_progress(SignInProgress::Completed(next.clone()));
        Ok(next)
    }

    /// Poll the token endpoint until a token arrives, the code expires or AAD
    /// declines, honouring `interval` and `slow_down`.
    async fn poll_for_token<F>(
        &self,
        params: &OAuthParams,
        start: &DeviceCodeStart,
        on_progress: &mut F,
    ) -> Result<TokenResponse>
    where
        F: FnMut(SignInProgress) + Send,
    {
        let mut interval = start.interval.max(1);
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(start.expires_in.max(interval));

        loop {
            on_progress(SignInProgress::Polling);
            match self
                .transport
                .poll_device_code(params, &start.device_code)
                .await?
            {
                PollResult::Token(token) => return Ok(token),
                PollResult::Denied => {
                    return Err(CoreError::Auth("sign-in was declined".into()));
                }
                PollResult::Expired => {
                    return Err(CoreError::Auth("device code expired before sign-in".into()));
                }
                PollResult::Pending => {}
                PollResult::SlowDown => {
                    interval = interval.saturating_add(SLOW_DOWN_INCREMENT_SECS);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(CoreError::Auth("device code expired before sign-in".into()));
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    }

    /// Mint one derived token by refresh grant, rotating the refresh token if
    /// AAD returned a new one.
    async fn acquire<F>(
        &self,
        params: &OAuthParams,
        refresh_token: &mut String,
        kind: TokenKind,
        on_progress: &mut F,
    ) -> Result<StoredToken>
    where
        F: FnMut(SignInProgress) + Send,
    {
        on_progress(SignInProgress::AcquiringToken(kind));
        let grant = self
            .transport
            .refresh_grant(params, refresh_token, kind)
            .await?;
        if let Some(new_refresh) = grant.refresh_token.filter(|s| !s.is_empty()) {
            *refresh_token = new_refresh;
        }
        Ok(StoredToken::from_lifetime(grant.token, grant.expires_in))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::{MemoryTokenStore, TokenStore};
    use std::sync::{Arc, Mutex};

    /// What the fake was asked to do, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        DeviceCode,
        Poll,
        ExchangeSkype,
        Grant(TokenKind),
    }

    /// Deterministic offline transport: succeeds immediately and records calls.
    #[derive(Default)]
    struct FakeTransport {
        calls: Mutex<Vec<Call>>,
    }

    impl FakeTransport {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
        fn record(&self, c: Call) {
            self.calls.lock().unwrap().push(c);
        }
    }

    impl AuthTransport for FakeTransport {
        fn request_device_code(
            &self,
            _params: &OAuthParams,
        ) -> impl Future<Output = Result<DeviceCodeStart>> + Send {
            self.record(Call::DeviceCode);
            async move {
                Ok(DeviceCodeStart {
                    device_code: "dc".into(),
                    user_code: "ABCD-EFGH".into(),
                    verification_uri: "https://microsoft.com/devicelogin".into(),
                    expires_in: 900,
                    interval: 0,
                    message: "Sign in".into(),
                })
            }
        }

        fn poll_device_code(
            &self,
            _params: &OAuthParams,
            _device_code: &str,
        ) -> impl Future<Output = Result<PollResult>> + Send {
            self.record(Call::Poll);
            async move {
                Ok(PollResult::Token(TokenResponse {
                    access_token: "aad-access".into(),
                    refresh_token: Some("refresh-1".into()),
                    expires_in: Some(3600),
                }))
            }
        }

        fn exchange_skype(
            &self,
            _aad_token: &str,
            _personal: bool,
        ) -> impl Future<Output = Result<SkypeExchange>> + Send {
            self.record(Call::ExchangeSkype);
            async move {
                Ok(SkypeExchange {
                    token: "skype-token".into(),
                    expires_in: Some(3600),
                    region_gtms: Some(serde_json::json!({"region": "emea"})),
                })
            }
        }

        fn refresh_grant(
            &self,
            _params: &OAuthParams,
            _refresh_token: &str,
            kind: TokenKind,
        ) -> impl Future<Output = Result<GrantedToken>> + Send {
            self.record(Call::Grant(kind));
            async move {
                Ok(GrantedToken {
                    token: format!("new-{}", kind.as_str()),
                    refresh_token: Some(format!("rotate-{}", kind.as_str())),
                    expires_in: Some(3600),
                })
            }
        }
    }

    fn future_set(
        spaces_at: u64,
        skype_at: u64,
        graph_at: u64,
        ic3_at: u64,
        rec_at: u64,
    ) -> TokenSet {
        TokenSet {
            refresh_token: "refresh-0".into(),
            tenant: "common".into(),
            region_gtms: serde_json::json!({"old": true}),
            personal: false,
            spaces: StoredToken::with_expiry("old-spaces", spaces_at),
            skype: StoredToken::with_expiry("old-skype", skype_at),
            graph: StoredToken::with_expiry("old-graph", graph_at),
            ic3: StoredToken::with_expiry("old-ic3", ic3_at),
            recorder: StoredToken::with_expiry("old-recorder", rec_at),
        }
    }

    #[tokio::test]
    async fn full_sign_in_produces_complete_token_set() {
        let store = MemoryTokenStore::new();
        let driver = AuthDriver::new(FakeTransport::default());
        let mut progress = Vec::new();

        let set = driver
            .sign_in(false, "default", &store, |p| progress.push(p))
            .await
            .expect("sign in");

        // Complete five-token model.
        assert_eq!(set.spaces.token(), "aad-access");
        assert_eq!(set.skype_token(), "skype-token");
        assert_eq!(set.graph.token(), "new-graph");
        assert_eq!(set.ic3.token(), "new-ic3");
        assert_eq!(set.recorder.token(), "new-recorder");
        assert_eq!(set.tenant(), "common");
        assert!(!set.needs_refresh());
        assert_eq!(set.region_gtms(), &serde_json::json!({"region": "emea"}));
        // Refresh token is the last rotated one.
        assert_eq!(set.refresh_token(), "rotate-recorder");

        // Persisted.
        let stored = store.load("default").unwrap().expect("stored");
        assert_eq!(stored.refresh_token(), "rotate-recorder");

        // Progress shape.
        assert_eq!(
            progress.first(),
            Some(&SignInProgress::RequestingDeviceCode)
        );
        assert!(matches!(
            progress.get(1),
            Some(SignInProgress::AwaitingUser { user_code, .. }) if user_code == "ABCD-EFGH"
        ));
        assert!(matches!(
            progress.last(),
            Some(SignInProgress::Completed(_))
        ));

        // Call order.
        assert_eq!(
            driver.transport().calls(),
            vec![
                Call::DeviceCode,
                Call::Poll,
                Call::ExchangeSkype,
                Call::Grant(TokenKind::Graph),
                Call::Grant(TokenKind::Ic3),
                Call::Grant(TokenKind::Recorder),
            ]
        );
    }

    #[tokio::test]
    async fn refresh_re_mints_only_expired() {
        let store = MemoryTokenStore::new();
        let driver = AuthDriver::new(FakeTransport::default());
        // Only IC3 is (near) expired.
        let current = future_set(
            4_000_000_000,
            4_000_000_000,
            4_000_000_000,
            1_000, // ic3 expired
            4_000_000_000,
        );
        let mut progress = Vec::new();

        let refreshed = driver
            .refresh(&current, "default", &store, |p| progress.push(p))
            .await
            .expect("refresh");

        assert_eq!(
            driver.transport().calls(),
            vec![Call::Grant(TokenKind::Ic3)],
            "only the expired token must be re-minted"
        );
        assert_eq!(refreshed.ic3.token(), "new-ic3");
        // Untouched kinds keep their old values.
        assert_eq!(refreshed.spaces.token(), "old-spaces");
        assert_eq!(refreshed.skype.token(), "old-skype");
        assert_eq!(refreshed.graph.token(), "old-graph");
        assert_eq!(refreshed.recorder.token(), "old-recorder");
        assert_eq!(refreshed.region_gtms(), &serde_json::json!({"old": true}));
        assert_eq!(refreshed.refresh_token(), "rotate-ic3");
        assert!(store.load("default").unwrap().is_some());
    }

    #[tokio::test]
    async fn refresh_re_exchanges_skype_when_spaces_expires() {
        let store = MemoryTokenStore::new();
        let driver = AuthDriver::new(FakeTransport::default());
        let current = future_set(
            1_000, // spaces expired
            1_000, // skype expired
            4_000_000_000,
            4_000_000_000,
            4_000_000_000,
        );

        let refreshed = driver
            .refresh(&current, "default", &store, |_| {})
            .await
            .expect("refresh");

        assert_eq!(
            driver.transport().calls(),
            vec![
                Call::Grant(TokenKind::Spaces),
                Call::ExchangeSkype,
                // Spaces grant rotates the refresh token; skype exchange doesn't.
            ]
        );
        assert_eq!(refreshed.spaces.token(), "new-spaces");
        assert_eq!(refreshed.skype_token(), "skype-token");
        assert_eq!(
            refreshed.region_gtms(),
            &serde_json::json!({"region": "emea"})
        );
    }

    #[tokio::test]
    async fn refresh_with_nothing_expired_is_a_noop() {
        let store = MemoryTokenStore::new();
        let driver = AuthDriver::new(FakeTransport::default());
        let current = future_set(
            4_000_000_000,
            4_000_000_000,
            4_000_000_000,
            4_000_000_000,
            4_000_000_000,
        );

        let refreshed = driver
            .refresh(&current, "default", &store, |_| {})
            .await
            .expect("refresh");

        assert!(driver.transport().calls().is_empty());
        assert_eq!(refreshed.spaces.token(), "old-spaces");
        assert!(store.load("default").unwrap().is_none());
    }

    #[tokio::test]
    async fn sign_in_fails_without_refresh_token() {
        // A transport whose poll omits the refresh token.
        struct NoRefresh;
        impl AuthTransport for NoRefresh {
            fn request_device_code(
                &self,
                _p: &OAuthParams,
            ) -> impl Future<Output = Result<DeviceCodeStart>> + Send {
                async move {
                    Ok(DeviceCodeStart {
                        device_code: "dc".into(),
                        user_code: "U".into(),
                        verification_uri: "https://x".into(),
                        expires_in: 900,
                        interval: 0,
                        message: String::new(),
                    })
                }
            }
            fn poll_device_code(
                &self,
                _p: &OAuthParams,
                _d: &str,
            ) -> impl Future<Output = Result<PollResult>> + Send {
                async move {
                    Ok(PollResult::Token(TokenResponse {
                        access_token: "at".into(),
                        refresh_token: None,
                        expires_in: Some(10),
                    }))
                }
            }
            fn exchange_skype(
                &self,
                _a: &str,
                _p: bool,
            ) -> impl Future<Output = Result<SkypeExchange>> + Send {
                async { unreachable!("not reached in this test") }
            }
            fn refresh_grant(
                &self,
                _p: &OAuthParams,
                _r: &str,
                _k: TokenKind,
            ) -> impl Future<Output = Result<GrantedToken>> + Send {
                async { unreachable!("not reached in this test") }
            }
        }

        let store = MemoryTokenStore::new();
        let driver = AuthDriver::new(NoRefresh);
        let err = driver
            .sign_in(false, "default", &store, |_| {})
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("refresh token"));
        assert!(store.load("default").unwrap().is_none());
    }

    // ----- HttpTransport against a local one-shot HTTP stub -------------------

    /// Serve exactly one HTTP request, capturing it, replying with `response`.
    async fn one_shot_stub(response: String) -> (String, Arc<Mutex<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let seen = Arc::new(Mutex::new(String::new()));
        let log = seen.clone();
        tokio::spawn(async move {
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
                let text = String::from_utf8_lossy(&buf);
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            *log.lock().unwrap() = String::from_utf8_lossy(&buf).to_string();
            sock.write_all(response.as_bytes()).await.ok();
            sock.shutdown().await.ok();
        });
        (format!("http://{addr}"), seen)
    }

    fn http_json(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn http_transport_requests_device_code_and_parses() {
        let (base, seen) = one_shot_stub(http_json(
            "200 OK",
            r#"{"device_code":"DC","user_code":"U-CODE","verification_uri":"https://v","expires_in":900,"interval":5,"message":"hi"}"#,
        ))
        .await;
        let transport = HttpTransport::with_login_base(base);
        let start = transport
            .request_device_code(&OAuthParams::for_account(false))
            .await
            .expect("device code");
        assert_eq!(start.user_code, "U-CODE");

        let req = seen.lock().unwrap().clone();
        assert!(req.starts_with("POST /common/oauth2/v2.0/devicecode "));
        assert!(req.contains("client_id=1fec8e78-bce4-4aaf-ab1b-5451cc387264"));
        assert!(req.contains("scope="));
    }

    #[tokio::test]
    async fn http_transport_refresh_grant_uses_kind_scope() {
        let (base, seen) = one_shot_stub(http_json(
            "200 OK",
            r#"{"access_token":"AT","refresh_token":"RT2","expires_in":3600}"#,
        ))
        .await;
        let transport = HttpTransport::with_login_base(base);
        let grant = transport
            .refresh_grant(&OAuthParams::for_account(false), "RT1", TokenKind::Graph)
            .await
            .expect("grant");
        assert_eq!(grant.token, "AT");
        assert_eq!(grant.refresh_token.as_deref(), Some("RT2"));

        let req = seen.lock().unwrap().clone();
        assert!(req.starts_with("POST /common/oauth2/v2.0/token "));
        assert!(req.contains("grant_type=refresh_token"));
        // The Graph scope is URL-encoded into the form.
        assert!(req.contains("graph.microsoft.com%2F.default"));
        assert!(req.contains("offline_access"));
    }

    #[tokio::test]
    async fn http_transport_refresh_error_is_coded_not_leaky() {
        let (base, _) = one_shot_stub(http_json(
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"AADSTS70000: bad.\r\nTrace ID: x"}"#,
        ))
        .await;
        let transport = HttpTransport::with_login_base(base);
        let err = transport
            .refresh_grant(
                &OAuthParams::for_account(false),
                "SECRET-RT",
                TokenKind::Ic3,
            )
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("invalid_grant") && msg.contains("AADSTS70000"));
        assert!(!msg.contains("SECRET-RT") && !msg.contains("Trace ID"));
    }
}
