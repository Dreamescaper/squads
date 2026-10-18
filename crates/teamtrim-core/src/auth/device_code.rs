//! RFC 8628 device-authorization types and AAD error-code mapping.
//!
//! Two typed pieces:
//!
//! - [`DeviceCodeStart`] — the response to the initial `…/devicecode` request.
//! - [`PollResult`] — the classified result of a token poll, so callers never
//!   string-match AAD's error codes themselves.
//!
//! The transport ([`super::flow::AuthTransport`]) fetches the raw bytes; this
//! module turns them into those types. All parsing is pure and offline-testable.

use serde::Deserialize;

use crate::error::{CoreError, Result};

/// Default polling interval (seconds) when AAD omits `interval`.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
/// Extra delay (seconds) applied on every `slow_down`, per RFC 8628 §3.5.
pub const SLOW_DOWN_INCREMENT_SECS: u64 = 5;
/// Default device-code lifetime (seconds) when AAD omits `expires_in`.
pub const DEFAULT_DEVICE_CODE_LIFETIME_SECS: u64 = 900;

/// The initial device-code response (`…/oauth2/v2.0/devicecode`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DeviceCodeStart {
    /// Opaque code the client polls with. Never logged.
    pub device_code: String,
    /// Short code the user types at [`Self::verification_uri`].
    pub user_code: String,
    /// URL the user visits.
    pub verification_uri: String,
    /// Seconds the device code stays valid.
    #[serde(default = "default_expires_in")]
    pub expires_in: u64,
    /// Minimum seconds between polls.
    #[serde(default = "default_interval")]
    pub interval: u64,
    /// Human-readable instructions from AAD.
    #[serde(default)]
    pub message: String,
}

fn default_interval() -> u64 {
    DEFAULT_POLL_INTERVAL_SECS
}

fn default_expires_in() -> u64 {
    DEFAULT_DEVICE_CODE_LIFETIME_SECS
}

impl DeviceCodeStart {
    /// Parse from a JSON value (the raw HTTP body).
    pub fn from_json(value: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(value.clone())
            .map_err(|e| CoreError::Auth(format!("malformed device-code response: {e}")))
    }
}

/// A successful token response from the token endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TokenResponse {
    /// The access token (AAD/Skype-Spaces). Never logged.
    pub access_token: String,
    /// A fresh refresh token, if AAD rotated it.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Lifetime in seconds.
    #[serde(default)]
    pub expires_in: Option<u64>,
}

/// Classified result of polling the token endpoint with a device code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollResult {
    /// `authorization_pending` — keep polling at the current interval.
    Pending,
    /// `slow_down` — increase the interval by
    /// [`SLOW_DOWN_INCREMENT_SECS`] and keep polling.
    SlowDown,
    /// `authorization_declined` — the user refused.
    Denied,
    /// `expired_token` — the device code expired; restart the flow.
    Expired,
    /// A token was issued.
    Token(TokenResponse),
}

/// Classify a token-endpoint response into a [`PollResult`].
///
/// `status` is the HTTP status code; `body` the parsed JSON body. AAD's device
/// grant reports pending/declined/expired as HTTP 400 with an `error` field.
/// `bad_verification_code` and any other code become an [`CoreError::Auth`].
pub fn parse_poll_response(status: u16, body: &serde_json::Value) -> Result<PollResult> {
    if (200..300).contains(&status) {
        return Ok(PollResult::Token(parse_token_success(body)?));
    }
    let code = body
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown_error");
    match code {
        "authorization_pending" => Ok(PollResult::Pending),
        "slow_down" => Ok(PollResult::SlowDown),
        "authorization_declined" | "access_denied" => Ok(PollResult::Denied),
        "expired_token" => Ok(PollResult::Expired),
        // `bad_verification_code` (and anything unexpected) is a hard error.
        _ => Err(CoreError::Auth(format!(
            "token endpoint error '{code}': {}",
            first_line(body)
        ))),
    }
}

/// Extract a token response from a 2xx body.
pub fn parse_token_success(body: &serde_json::Value) -> Result<TokenResponse> {
    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CoreError::Auth("token response missing 'access_token'".into()))?
        .to_string();
    Ok(TokenResponse {
        access_token,
        refresh_token: body
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        expires_in: get_u64(body.get("expires_in")),
    })
}

/// `expires_in` may be a JSON number or a stringified number.
pub(crate) fn get_u64(value: Option<&serde_json::Value>) -> Option<u64> {
    let v = value?;
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    v.as_str()?.trim().parse().ok()
}

/// First line of `error_description`, for short, secret-free messages.
fn first_line(body: &serde_json::Value) -> String {
    body.get("error_description")
        .and_then(|v| v.as_str())
        .and_then(|d| d.lines().next())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_device_code_start_with_defaults() {
        let v = json!({
            "device_code": "DC",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://microsoft.com/devicelogin",
            "expires_in": 1800,
            "interval": 5,
            "message": "Go sign in"
        });
        let start = DeviceCodeStart::from_json(&v).expect("parse");
        assert_eq!(start.user_code, "ABCD-EFGH");
        assert_eq!(start.expires_in, 1800);
        assert_eq!(start.interval, 5);

        // Missing interval/expires_in/message fall back to defaults.
        let minimal = json!({
            "device_code": "DC",
            "user_code": "U",
            "verification_uri": "https://x"
        });
        let start = DeviceCodeStart::from_json(&minimal).expect("parse");
        assert_eq!(start.interval, DEFAULT_POLL_INTERVAL_SECS);
        assert_eq!(start.expires_in, DEFAULT_DEVICE_CODE_LIFETIME_SECS);
        assert!(start.message.is_empty());
    }

    #[test]
    fn maps_aad_error_codes() {
        assert_eq!(
            parse_poll_response(400, &json!({"error": "authorization_pending"})).unwrap(),
            PollResult::Pending
        );
        assert_eq!(
            parse_poll_response(400, &json!({"error": "slow_down"})).unwrap(),
            PollResult::SlowDown
        );
        assert_eq!(
            parse_poll_response(400, &json!({"error": "authorization_declined"})).unwrap(),
            PollResult::Denied
        );
        assert_eq!(
            parse_poll_response(400, &json!({"error": "expired_token"})).unwrap(),
            PollResult::Expired
        );
        // `access_denied` is an alias for declined.
        assert_eq!(
            parse_poll_response(403, &json!({"error": "access_denied"})).unwrap(),
            PollResult::Denied
        );
    }

    #[test]
    fn bad_verification_code_is_a_hard_error_without_secrets() {
        let err = parse_poll_response(
            400,
            &json!({
                "error": "bad_verification_code",
                "error_description": "AADSTS70000: The code is wrong.\r\nTrace ID: 123"
            }),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bad_verification_code"));
        assert!(msg.contains("AADSTS70000"));
        assert!(!msg.contains("Trace ID"), "must keep only the first line");
    }

    #[test]
    fn parses_success_token() {
        let out = parse_poll_response(
            200,
            &json!({
                "token_type": "Bearer",
                "access_token": "AT",
                "refresh_token": "RT",
                "expires_in": "3599"
            }),
        )
        .unwrap();
        assert_eq!(
            out,
            PollResult::Token(TokenResponse {
                access_token: "AT".into(),
                refresh_token: Some("RT".into()),
                expires_in: Some(3599)
            })
        );
    }

    #[test]
    fn missing_access_token_is_an_error() {
        assert!(parse_token_success(&json!({"token_type": "Bearer"})).is_err());
        assert!(parse_token_success(&json!({"access_token": ""})).is_err());
    }
}
