//! The five-token model.
//!
//! Teams sign-in yields one *long-lived* credential (the AAD refresh token) and
//! five short-lived access tokens that are periodically re-minted from it:
//!
//! | [`TokenKind`] | Audience / resource | How it is obtained |
//! |---|---|---|
//! | `Spaces` | `https://api.spaces.skype.com` | device-code or refresh grant |
//! | `Skype` | Teams `authsvc` (native chat) | exchange of the `Spaces` token |
//! | `Graph` | `https://graph.microsoft.com` | refresh grant |
//! | `Ic3` | `https://ic3.teams.office.com` | refresh grant |
//! | `Recorder` | `4580fd1d-…` (call recording) | refresh grant |
//!
//! [`TokenSet`] is the unit that is persisted by an [`TokenStore`](super::store::TokenStore)
//! and handed to the protocol clients. Token *contents* are never printed: both
//! [`StoredToken`] and [`TokenSet`] implement a redacting [`Debug`].

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A token is treated as expired once less than this many seconds of life
/// remain, so refresh happens *before* requests start failing.
pub const EXPIRY_SKEW_SECS: u64 = 300;

/// Assumed lifetime when a token endpoint omits `expires_in`.
pub const DEFAULT_TOKEN_LIFETIME_SECS: u64 = 3600;

/// Current Unix time in seconds (saturating, so the clock is never wrong by
/// panicking).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One access token plus its absolute expiry (Unix seconds).
///
/// Mirrors `ost::auth::StoredToken` but stores the absolute deadline as a plain
/// `u64` and applies the same five-minute skew when deciding expiry.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredToken {
    /// The opaque bearer token. Never logged.
    pub token: String,
    /// Absolute expiry as Unix seconds.
    pub expires_at: u64,
}

impl StoredToken {
    /// Build a token whose deadline is `now + expires_in` (or
    /// [`DEFAULT_TOKEN_LIFETIME_SECS`] when the lifetime is unknown).
    pub fn from_lifetime(token: impl Into<String>, expires_in: Option<u64>) -> Self {
        let life = expires_in.unwrap_or(DEFAULT_TOKEN_LIFETIME_SECS);
        Self {
            token: token.into(),
            expires_at: now_unix().saturating_add(life),
        }
    }

    /// Build a token with an explicit absolute deadline (used by tests).
    pub fn with_expiry(token: impl Into<String>, expires_at: u64) -> Self {
        Self {
            token: token.into(),
            expires_at,
        }
    }

    /// Whether the token is expired *as of* `now`, applying [`EXPIRY_SKEW_SECS`].
    pub fn is_expired_at(&self, now: u64) -> bool {
        now.saturating_add(EXPIRY_SKEW_SECS) >= self.expires_at
    }

    /// Whether the token is expired as of the current wall clock.
    pub fn is_expired(&self) -> bool {
        self.is_expired_at(now_unix())
    }

    /// Seconds of *usable* life left (never negative); zero means "refresh now".
    pub fn remaining_secs(&self, now: u64) -> u64 {
        self.expires_at
            .saturating_sub(now.saturating_add(EXPIRY_SKEW_SECS))
    }

    /// The bearer token value.
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl fmt::Debug for StoredToken {
    /// Redacted: reveals presence and expiry, never the token.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredToken")
            .field("token", &format_args!("<{} chars>", self.token.len()))
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Identifies one of the five access tokens in a [`TokenSet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    /// AAD token for `https://api.spaces.skype.com` (the "AAD/Skype-Spaces" token).
    Spaces,
    /// Teams `authsvc` Skype token used by the native chat service.
    Skype,
    /// Microsoft Graph access token.
    Graph,
    /// IC3 token (Trouter realtime authentication).
    ///
    /// Serialized as `"ic3"`.
    #[serde(rename = "ic3")]
    Ic3,
    /// Call-recorder service token.
    Recorder,
}

impl TokenKind {
    /// Every kind, in acquisition order.
    pub const ALL: [TokenKind; 5] = [
        TokenKind::Spaces,
        TokenKind::Skype,
        TokenKind::Graph,
        TokenKind::Ic3,
        TokenKind::Recorder,
    ];

    /// Stable lowercase identifier (also the persisted form).
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Spaces => "spaces",
            TokenKind::Skype => "skype",
            TokenKind::Graph => "graph",
            TokenKind::Ic3 => "ic3",
            TokenKind::Recorder => "recorder",
        }
    }

    /// Human label for progress reporting / UIs.
    pub fn label(self) -> &'static str {
        match self {
            TokenKind::Spaces => "AAD/Skype-Spaces token",
            TokenKind::Skype => "Skype token",
            TokenKind::Graph => "Graph token",
            TokenKind::Ic3 => "IC3 token",
            TokenKind::Recorder => "recorder token",
        }
    }

    /// The resource scope used to mint this token by a refresh grant, without
    /// `offline_access`. `Skype` is `None`: it is produced by an exchange, not a
    /// refresh grant.
    pub fn scope(self) -> Option<&'static str> {
        match self {
            TokenKind::Spaces => Some("https://api.spaces.skype.com/.default"),
            TokenKind::Skype => None,
            TokenKind::Graph => Some("https://graph.microsoft.com/.default"),
            TokenKind::Ic3 => Some("https://ic3.teams.office.com/.default"),
            TokenKind::Recorder => Some("4580fd1d-e5a3-4f56-9ad1-aab0e3bf8f76/.default"),
        }
    }
}

/// Everything needed to talk to Teams: one refresh token plus the five access
/// tokens and the region routing blob returned by the `authsvc` exchange.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenSet {
    /// AAD refresh token — the long-lived credential. Never logged.
    pub refresh_token: String,
    /// AAD tenant used for the token endpoint (`common` / `consumers` / a guid).
    pub tenant: String,
    /// `regionGtms` from the `authsvc` exchange (routing table); may be `null`.
    #[serde(default)]
    pub region_gtms: serde_json::Value,
    /// Whether this is a personal (consumer) account.
    #[serde(default)]
    pub personal: bool,
    /// AAD/Skype-Spaces access token.
    pub spaces: StoredToken,
    /// Skype (native chat) token.
    pub skype: StoredToken,
    /// Graph access token.
    pub graph: StoredToken,
    /// IC3 access token.
    pub ic3: StoredToken,
    /// Recorder access token.
    pub recorder: StoredToken,
}

impl TokenSet {
    /// Borrow the token entry for `kind`.
    pub fn token(&self, kind: TokenKind) -> &StoredToken {
        match kind {
            TokenKind::Spaces => &self.spaces,
            TokenKind::Skype => &self.skype,
            TokenKind::Graph => &self.graph,
            TokenKind::Ic3 => &self.ic3,
            TokenKind::Recorder => &self.recorder,
        }
    }

    /// Mutably borrow the token entry for `kind`.
    pub fn token_mut(&mut self, kind: TokenKind) -> &mut StoredToken {
        match kind {
            TokenKind::Spaces => &mut self.spaces,
            TokenKind::Skype => &mut self.skype,
            TokenKind::Graph => &mut self.graph,
            TokenKind::Ic3 => &mut self.ic3,
            TokenKind::Recorder => &mut self.recorder,
        }
    }

    /// The long-lived refresh token.
    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    /// The tenant the tokens were minted against.
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// The `regionGtms` routing blob (`null` when absent).
    pub fn region_gtms(&self) -> &serde_json::Value {
        &self.region_gtms
    }

    /// Convenience accessor for the Skype (native chat) token value.
    pub fn skype_token(&self) -> &str {
        &self.skype.token
    }

    /// Kinds that are expired as of `now` (with skew applied), in
    /// [`TokenKind::ALL`] order.
    pub fn expired_kinds_at(&self, now: u64) -> Vec<TokenKind> {
        TokenKind::ALL
            .into_iter()
            .filter(|k| self.token(*k).is_expired_at(now))
            .collect()
    }

    /// Kinds that are expired as of the current wall clock.
    pub fn expired_kinds(&self) -> Vec<TokenKind> {
        self.expired_kinds_at(now_unix())
    }

    /// Whether any token needs refreshing.
    pub fn needs_refresh(&self) -> bool {
        TokenKind::ALL.iter().any(|k| self.token(*k).is_expired())
    }
}

impl fmt::Debug for TokenSet {
    /// Redacted: shows which tokens exist and when they expire, never contents.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSet")
            .field(
                "refresh_token",
                &format_args!("<{} chars>", self.refresh_token.len()),
            )
            .field("tenant", &self.tenant)
            .field("region_gtms", &!self.region_gtms.is_null())
            .field("personal", &self.personal)
            .field("spaces", &self.spaces)
            .field("skype", &self.skype)
            .field("graph", &self.graph)
            .field("ic3", &self.ic3)
            .field("recorder", &self.recorder)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(expires_at: u64) -> TokenSet {
        let t = StoredToken::with_expiry("secret", expires_at);
        TokenSet {
            refresh_token: "refresh-secret".into(),
            tenant: "common".into(),
            region_gtms: serde_json::json!({"a": 1}),
            personal: false,
            spaces: t.clone(),
            skype: t.clone(),
            graph: t.clone(),
            ic3: t.clone(),
            recorder: t,
        }
    }

    #[test]
    fn valid_when_more_than_skew_remains() {
        let now = 1_000_000;
        // 301s of life left: not expired (skew is strict).
        let tok = StoredToken::with_expiry("x", now + 301);
        assert!(!tok.is_expired_at(now));
        assert_eq!(tok.remaining_secs(now), 1);
    }

    #[test]
    fn expires_within_skew_window() {
        let now = 1_000_000;
        // Exactly at the skew boundary: treated as expired.
        assert!(StoredToken::with_expiry("x", now + EXPIRY_SKEW_SECS).is_expired_at(now));
        // Well in the past: expired.
        assert!(StoredToken::with_expiry("x", now - 10).is_expired_at(now));
        assert_eq!(
            StoredToken::with_expiry("x", now - 10).remaining_secs(now),
            0
        );
    }

    #[test]
    fn unknown_lifetime_gets_default() {
        let before = now_unix();
        let tok = StoredToken::from_lifetime("x", None);
        assert!(tok.expires_at >= before + DEFAULT_TOKEN_LIFETIME_SECS);
        assert!(!tok.is_expired());
    }

    #[test]
    fn expired_kinds_lists_only_expired() {
        let now = 1_000_000;
        let mut s = set(now + 3600);
        s.graph = StoredToken::with_expiry("g", now - 1);
        s.ic3 = StoredToken::with_expiry("i", now - 1);
        assert_eq!(
            s.expired_kinds_at(now),
            vec![TokenKind::Graph, TokenKind::Ic3]
        );

        let fresh = set(now + 3600);
        assert!(fresh.expired_kinds_at(now).is_empty());
    }

    #[test]
    fn needs_refresh_uses_the_current_clock() {
        // A far-future set is fresh; a far-past set needs refreshing.
        let future = set(now_unix() + 3600);
        assert!(!future.needs_refresh());
        let past = set(now_unix().saturating_sub(10));
        assert!(past.needs_refresh());
        assert_eq!(past.expired_kinds().len(), TokenKind::ALL.len());
    }

    #[test]
    fn accessors_round_trip_by_kind() {
        let mut s = set(1);
        s.token_mut(TokenKind::Recorder).token = "r".into();
        assert_eq!(s.token(TokenKind::Recorder).token(), "r");
        assert_eq!(s.token(TokenKind::Spaces).token(), "secret");
        assert_eq!(s.refresh_token(), "refresh-secret");
        assert_eq!(s.tenant(), "common");
    }

    #[test]
    fn debug_never_leaks_token_contents() {
        let s = set(1);
        let dbg = format!("{s:?}");
        assert!(
            !dbg.contains("secret"),
            "TokenSet Debug leaked a token: {dbg}"
        );
        assert!(!dbg.contains("refresh-secret"));
        let tok = format!("{:?}", s.spaces);
        assert!(!tok.contains("secret"));
    }

    #[test]
    fn json_round_trips() {
        let s = set(1234);
        let json = serde_json::to_string(&s).expect("serialize");
        let back: TokenSet = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.tenant, "common");
        assert_eq!(back.spaces.expires_at, 1234);
        assert_eq!(back.region_gtms, s.region_gtms);
        // ic3 serializes with the documented key.
        assert!(json.contains("\"ic3\""));
    }
}
