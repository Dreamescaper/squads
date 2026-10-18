//! PKCE (RFC 7636) helpers for the browser sign-in flow.
//!
//! The flow is standard OAuth2 authorization-code + PKCE against AAD:
//!
//! 1. [`PkceChallenge::generate`] creates a high-entropy `code_verifier`, its
//!    `S256` `code_challenge` and a random `state`.
//! 2. [`authorize_url`] builds the URL the user opens in a browser.
//! 3. A loopback listener receives the redirect, and [`parse_redirect`] turns
//!    the query string into a [`RedirectOutcome`].
//!
//! The `code_challenge` derivation has a known-answer test from RFC 7636
//! Appendix B.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use super::OAuthParams;
use crate::error::{CoreError, Result};

/// Minimum `code_verifier` length required by RFC 7636 §4.1.
pub const VERIFIER_MIN_LEN: usize = 43;
/// Maximum `code_verifier` length required by RFC 7636 §4.1.
pub const VERIFIER_MAX_LEN: usize = 128;

/// Bytes of secure randomness behind a verifier/state. 32 bytes → a 43-char
/// base64url string, comfortably inside the RFC range.
const RANDOM_BYTES: usize = 32;

/// One PKCE attempt: the secret verifier, its derived challenge and the
/// anti-CSRF `state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkceChallenge {
    /// The secret kept by the client (`code_verifier`).
    pub verifier: String,
    /// `BASE64URL-NOPAD(SHA256(verifier))`.
    pub challenge: String,
    /// Random opaque value echoed back in the redirect.
    pub state: String,
}

impl PkceChallenge {
    /// Generate a fresh verifier/challenge/state pair from a CSPRNG.
    pub fn generate() -> Result<Self> {
        let verifier = random_b64url()?;
        let state = random_b64url()?;
        debug_assert!(is_valid_verifier(&verifier));
        Ok(Self {
            challenge: challenge_s256(&verifier),
            verifier,
            state,
        })
    }
}

/// 32 random bytes, base64url (no padding). Used for both the verifier and the
/// `state`.
fn random_b64url() -> Result<String> {
    let mut bytes = [0u8; RANDOM_BYTES];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| CoreError::Auth(format!("secure randomness unavailable: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// `BASE64URL-NOPAD(SHA256(verifier))` — the `S256` code challenge.
pub fn challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Whether `verifier` satisfies RFC 7636 §4.1: 43–128 characters from the
/// unreserved set `[A-Za-z0-9-._~]`.
pub fn is_valid_verifier(verifier: &str) -> bool {
    let len = verifier.len();
    (VERIFIER_MIN_LEN..=VERIFIER_MAX_LEN).contains(&len)
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// Build the AAD authorize URL for `params` with the given challenge/state.
///
/// Always uses `response_type=code`, `response_mode=query` and
/// `code_challenge_method=S256`.
pub fn authorize_url(params: &OAuthParams, challenge: &str, state: &str) -> Result<String> {
    let base = format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/authorize",
        params.tenant
    );
    let mut url = url::Url::parse(&base)
        .map_err(|e| CoreError::Auth(format!("invalid authorize URL: {e}")))?;
    url.query_pairs_mut()
        .append_pair("client_id", &params.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &params.redirect_uri)
        .append_pair("response_mode", "query")
        .append_pair("scope", &params.scope)
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.into())
}

/// The parsed result of the browser redirect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectOutcome {
    /// Success: an authorization code was returned (with the echoed `state`).
    Code {
        /// The authorization code to redeem.
        code: String,
        /// The `state` echoed by AAD (validated by the caller).
        state: Option<String>,
    },
    /// AAD returned an OAuth error (`error` + optional `error_description`).
    Error {
        /// Machine-readable error code, e.g. `access_denied`.
        error: String,
        /// Human-readable description, if provided.
        description: Option<String>,
    },
}

/// Parse a redirect URL (or bare query string) into a [`RedirectOutcome`].
///
/// Accepts a full URL (`https://…/nativeclient?code=…`), a `?`-prefixed query or
/// a bare `code=…&state=…` string. Error parameters take precedence over a code.
pub fn parse_redirect(raw: &str) -> Result<RedirectOutcome> {
    let query = match raw.split_once('?') {
        Some((_, q)) => q,
        None => raw.strip_prefix('?').unwrap_or(raw),
    };

    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut description = None;
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            "error_description" => description = Some(v.into_owned()),
            _ => {}
        }
    }

    if let Some(error) = error {
        return Ok(RedirectOutcome::Error { error, description });
    }
    match code {
        Some(code) if !code.is_empty() => Ok(RedirectOutcome::Code { code, state }),
        _ => Err(CoreError::Auth(
            "redirect contained neither 'code' nor 'error'".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 Appendix B known-answer test.
    #[test]
    fn rfc7636_appendix_b_known_answer() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            challenge_s256(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        // The published verifier is RFC-valid.
        assert!(is_valid_verifier(verifier));
    }

    #[test]
    fn generated_challenge_is_verifier_derived_and_valid() {
        let p = PkceChallenge::generate().expect("generate");
        assert!(is_valid_verifier(&p.verifier));
        assert_eq!(p.verifier.len(), 43);
        assert_eq!(p.challenge, challenge_s256(&p.verifier));
        assert!(!p.challenge.contains('='), "challenge must be unpadded");
        assert!(!p.state.is_empty());
        // Two attempts must not collide.
        let q = PkceChallenge::generate().expect("generate");
        assert_ne!(p.verifier, q.verifier);
        assert_ne!(p.state, q.state);
    }

    #[test]
    fn verifier_length_and_charset_rules() {
        assert!(!is_valid_verifier(&"a".repeat(42)));
        assert!(is_valid_verifier(&"a".repeat(43)));
        assert!(is_valid_verifier(&"a".repeat(128)));
        assert!(!is_valid_verifier(&"a".repeat(129)));
        assert!(!is_valid_verifier("has space and ! invalid"));
    }

    #[test]
    fn authorize_url_has_required_params() {
        let params = OAuthParams::for_account(false);
        let url = authorize_url(&params, "CHAL", "ST").expect("url");
        assert!(url.starts_with("https://login.microsoftonline.com/common/oauth2/v2.0/authorize?"));
        let parsed = url::Url::parse(&url).expect("parse");
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q.get("client_id").unwrap(), &params.client_id);
        assert_eq!(q.get("response_type").unwrap(), "code");
        assert_eq!(q.get("response_mode").unwrap(), "query");
        assert_eq!(q.get("code_challenge").unwrap(), "CHAL");
        assert_eq!(q.get("code_challenge_method").unwrap(), "S256");
        assert_eq!(q.get("state").unwrap(), "ST");
        assert_eq!(q.get("redirect_uri").unwrap(), &params.redirect_uri);
        assert_eq!(q.get("scope").unwrap(), &params.scope);
    }

    #[test]
    fn parse_redirect_success() {
        let out = parse_redirect(
            "https://login.microsoftonline.com/common/oauth2/nativeclient?code=abc%20123&state=xyz&session_state=s",
        )
        .expect("parse");
        assert_eq!(
            out,
            RedirectOutcome::Code {
                code: "abc 123".into(),
                state: Some("xyz".into())
            }
        );
        // Bare query and bare pairs work too.
        assert!(matches!(
            parse_redirect("?code=a&state=b").unwrap(),
            RedirectOutcome::Code { .. }
        ));
        assert!(matches!(
            parse_redirect("code=a").unwrap(),
            RedirectOutcome::Code { state: None, .. }
        ));
    }

    #[test]
    fn parse_redirect_error_takes_precedence_and_reports() {
        let out = parse_redirect("?error=access_denied&error_description=User+said+no").unwrap();
        assert_eq!(
            out,
            RedirectOutcome::Error {
                error: "access_denied".into(),
                description: Some("User said no".into())
            }
        );
        let out = parse_redirect("?code=c&error=server_error").unwrap();
        assert!(matches!(out, RedirectOutcome::Error { .. }));
    }

    #[test]
    fn parse_redirect_rejects_empty() {
        assert!(parse_redirect("?state=only").is_err());
        assert!(parse_redirect("?code=").is_err());
    }
}
