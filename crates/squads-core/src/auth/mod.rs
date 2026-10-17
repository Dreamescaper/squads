//! Authentication (milestone **M1**).
//!
//! This module wraps the vendored protocol crate's auth surface into a
//! UI-free, testable API:
//!
//! - [`tokens`] — the five-token model ([`TokenSet`], [`StoredToken`], [`TokenKind`])
//!   with expiry/skew predicates and redacting `Debug`.
//! - [`pkce`] — RFC 7636 code verifier/challenge, `state`, authorize URL and
//!   redirect parsing for the browser sign-in flow.
//! - [`device_code`] — RFC 8628 device-code request/response and AAD error-code
//!   mapping.
//! - [`store`] — the [`TokenStore`] abstraction: an atomic 0600 `FileTokenStore`
//!   (default), an in-memory store for tests, and a feature-gated keyring TODO.
//! - [`flow`] — [`AuthDriver`], which orchestrates sign-in and refresh over a
//!   small [`AuthTransport`] so tests can substitute a deterministic fake.
//!
//! Live network calls (the `HttpTransport`) are exercised only through the real
//! sign-in path; everything else is unit-tested offline.
//!
//! [`AuthDriver`]: flow::AuthDriver
//! [`AuthTransport`]: flow::AuthTransport
//! [`TokenSet`]: tokens::TokenSet
//! [`StoredToken`]: tokens::StoredToken
//! [`TokenKind`]: tokens::TokenKind

pub mod device_code;
pub mod flow;
pub mod pkce;
pub mod store;
pub mod tokens;

pub use device_code::{DeviceCodeStart, PollResult, TokenResponse};
pub use flow::{AuthDriver, AuthTransport, HttpTransport, SignInProgress};
pub use pkce::{PkceChallenge, RedirectOutcome};
pub use store::{DEFAULT_PROFILE, FileTokenStore, MemoryTokenStore, TokenStore};
pub use tokens::{StoredToken, TokenKind, TokenSet};

use ost::auth::AuthConfig;

/// OAuth2 parameters for one account kind (work/school vs. personal).
///
/// Wraps the vendored [`AuthConfig`] (first-party Teams client id, redirect and
/// `api.spaces.skype.com` scope) in owned strings so it can travel through the
/// async transport abstraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthParams {
    /// The first-party Teams public-client id.
    pub client_id: String,
    /// AAD tenant: `common` (work/school) or `consumers` (personal).
    pub tenant: String,
    /// Primary resource scope (`https://api.spaces.skype.com/.default offline_access`).
    pub scope: String,
    /// Redirect URI registered for the public client.
    pub redirect_uri: String,
    /// Whether this is a personal (consumer) account.
    pub personal: bool,
}

impl OAuthParams {
    /// Build the parameters for a work/school (`personal == false`) or personal
    /// account.
    pub fn for_account(personal: bool) -> Self {
        let cfg = if personal {
            AuthConfig::personal()
        } else {
            AuthConfig::work()
        };
        Self {
            client_id: cfg.client_id.to_string(),
            tenant: cfg.tenant.to_string(),
            scope: cfg.scope.to_string(),
            redirect_uri: cfg.redirect_uri.to_string(),
            personal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_use_first_party_teams_client() {
        let work = OAuthParams::for_account(false);
        assert_eq!(work.client_id, "1fec8e78-bce4-4aaf-ab1b-5451cc387264");
        assert_eq!(work.tenant, "common");
        assert!(work.scope.contains("api.spaces.skype.com/.default"));
        assert!(work.scope.contains("offline_access"));
        assert!(!work.personal);

        let personal = OAuthParams::for_account(true);
        assert_eq!(personal.tenant, "consumers");
        assert!(personal.personal);
    }
}
