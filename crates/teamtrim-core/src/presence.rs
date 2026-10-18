//! Presence service (milestone **M8**).
//!
//! Teams exposes presence through **two different services**, and this module
//! wraps both behind one transport-injectable service so the rest of the core
//! (and the UI) never has to care which one is in use:
//!
//! * the **Graph** service — `GET /me/presence` (own presence only) and
//!   `POST /me/presence/setUserPreferredPresence`. These are the vendored
//!   [`ost::api::get_presence_data`] / `set_presence` calls. **The first-party
//!   Teams token does not carry the `Presence.Read*` scopes, so these 403
//!   against a live tenant** (the vendored crate documents this itself).
//! * the **unified** service — `POST
//!   https://presence.teams.microsoft.com/v1/presence/getpresence/` (batch read)
//!   and `PUT …/v1/me/forceavailability/` (set own status). This is the service
//!   the Teams web/native clients use, is what `better-teams` switched to, and
//!   is therefore the **default** backend here.
//!
//! ```text
//!                 UI
//!                  │  get(ids) / own() / set(availability, activity)
//!                  ▼
//!              Presence ──▶ PresenceBackend::{Graph, Unified}
//! ```
//!
//! ## Testability
//!
//! The service is transport-injectable: the Graph backend goes through the
//! shared [`ApiClient`] (a local TCP stub in tests) and the unified backend takes
//! an **overridable base URL** plus the **token to send** as parameters. Both
//! parse raw JSON through pure, defensive mapping functions
//! ([`parse_unified_presence`], [`Availability::from_wire`], …) that unit tests
//! pin with realistic fixtures and unknown values (anything unrecognised
//! degrades to [`Availability::PresenceUnknown`] / [`Activity::Unknown`] — never
//! a panic).
//!
//! ## The unverified bit (see `docs/BLOCKERS.md`, ADR-021)
//!
//! The **token audience and the auth header for the unified service are
//! unverified**: `better-teams` minted a token for the
//! `https://presence.teams.microsoft.com` resource, but we only hold the M1
//! `spaces`/`skype`/`graph`/`ic3`/`recorder` tokens. The unified backend
//! therefore takes the token to send as a parameter; [`Presence::unified`] and
//! [`Session::presence`](crate::Session::presence) default to the **skype**
//! token (Bearer), which is a documented, unverified choice — not a fact.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{ApiClient, PresenceInfo};
use crate::error::{CoreError, Result};

/// The default unified-presence base URL, used when `region_gtms` carries no
/// `presenceBase` override.
pub const DEFAULT_PRESENCE_BASE: &str = "https://presence.teams.microsoft.com";

/// The `expirationDuration` sent with a forced availability (matches the
/// vendored Graph helper).
pub const PRESENCE_EXPIRATION: &str = "PT1H";

/// Teams availability, using the exact wire strings the services emit.
///
/// A value we do not recognise (a new Teams state, a typo, an empty string)
/// maps to [`Availability::PresenceUnknown`] rather than failing a parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Availability {
    /// Actively available.
    Available,
    /// Available but idle (the client has gone idle without going Away).
    AvailableIdle,
    /// Busy.
    Busy,
    /// Busy but idle.
    BusyIdle,
    /// Do not disturb.
    DoNotDisturb,
    /// Be right back.
    BeRightBack,
    /// Away.
    Away,
    /// Offline.
    Offline,
    /// The service reported a value we do not model.
    #[default]
    PresenceUnknown,
}

impl Availability {
    /// The exact wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Availability::Available => "Available",
            Availability::AvailableIdle => "AvailableIdle",
            Availability::Busy => "Busy",
            Availability::BusyIdle => "BusyIdle",
            Availability::DoNotDisturb => "DoNotDisturb",
            Availability::BeRightBack => "BeRightBack",
            Availability::Away => "Away",
            Availability::Offline => "Offline",
            Availability::PresenceUnknown => "PresenceUnknown",
        }
    }

    /// Parse a wire value case-insensitively, tolerating `_`/`-`/space
    /// separators and the `dnd`/`brb` shorthands. Unknown → `PresenceUnknown`.
    pub fn from_wire(value: &str) -> Self {
        match normalized_key(value).as_str() {
            "available" => Availability::Available,
            "availableidle" => Availability::AvailableIdle,
            "busy" => Availability::Busy,
            "busyidle" => Availability::BusyIdle,
            "donotdisturb" | "dnd" => Availability::DoNotDisturb,
            "berightback" | "brb" => Availability::BeRightBack,
            "away" => Availability::Away,
            "offline" => Availability::Offline,
            _ => Availability::PresenceUnknown,
        }
    }

    /// Whether this is an available-ish state (for a green dot).
    pub fn is_available(self) -> bool {
        matches!(self, Availability::Available | Availability::AvailableIdle)
    }
}

/// Teams activity (the finer-grained "what are they doing" alongside
/// availability). Unknown values degrade to [`Activity::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Activity {
    /// Available.
    Available,
    /// Available but idle.
    AvailableIdle,
    /// Busy.
    Busy,
    /// In a call.
    InACall,
    /// In a conference call.
    InAConferenceCall,
    /// Do not disturb.
    DoNotDisturb,
    /// Presenting.
    Presenting,
    /// Away.
    Away,
    /// Be right back.
    BeRightBack,
    /// Offline.
    Offline,
    /// Off work.
    OffWork,
    /// Out of office.
    OutOfOffice,
    /// The service reported an activity we do not model.
    #[default]
    Unknown,
}

impl Activity {
    /// The exact wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Activity::Available => "Available",
            Activity::AvailableIdle => "AvailableIdle",
            Activity::Busy => "Busy",
            Activity::InACall => "InACall",
            Activity::InAConferenceCall => "InAConferenceCall",
            Activity::DoNotDisturb => "DoNotDisturb",
            Activity::Presenting => "Presenting",
            Activity::Away => "Away",
            Activity::BeRightBack => "BeRightBack",
            Activity::Offline => "Offline",
            Activity::OffWork => "OffWork",
            Activity::OutOfOffice => "OutOfOffice",
            Activity::Unknown => "Unknown",
        }
    }

    /// Parse a wire value case-insensitively, tolerating `_`/`-`/space
    /// separators. Unknown → `Activity::Unknown`.
    pub fn from_wire(value: &str) -> Self {
        match normalized_key(value).as_str() {
            "available" => Activity::Available,
            "availableidle" => Activity::AvailableIdle,
            "busy" | "busyidle" => Activity::Busy,
            "inacall" => Activity::InACall,
            "inaconferencecall" => Activity::InAConferenceCall,
            "donotdisturb" => Activity::DoNotDisturb,
            "presenting" => Activity::Presenting,
            "away" => Activity::Away,
            "berightback" => Activity::BeRightBack,
            "offline" => Activity::Offline,
            "offwork" => Activity::OffWork,
            "outofoffice" => Activity::OutOfOffice,
            _ => Activity::Unknown,
        }
    }
}

/// The activity that pairs with an availability when a caller sets only the
/// availability.
pub fn default_activity(availability: Availability) -> Activity {
    match availability {
        Availability::Available => Activity::Available,
        Availability::AvailableIdle => Activity::AvailableIdle,
        Availability::Busy | Availability::BusyIdle => Activity::Busy,
        Availability::DoNotDisturb => Activity::DoNotDisturb,
        Availability::BeRightBack => Activity::BeRightBack,
        Availability::Away => Activity::Away,
        Availability::Offline => Activity::Offline,
        Availability::PresenceUnknown => Activity::Unknown,
    }
}

/// Lower-cased wire key with separators removed, for tolerant matching.
fn normalized_key(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// One contact's presence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactPresence {
    /// The contact's MRI (`8:orgid:<guid>`).
    pub mri: String,
    /// Availability.
    pub availability: Availability,
    /// Activity.
    pub activity: Activity,
    /// Free-text status message, when the service named one.
    pub status_message: Option<String>,
}

/// The signed-in user's own presence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnPresence {
    /// Availability.
    pub availability: Availability,
    /// Activity.
    pub activity: Activity,
}

impl OwnPresence {
    /// The "we do not know" sentinel (Graph and the unified service cannot
    /// always answer, and the unified `own()` without an MRI returns this).
    pub fn unknown() -> Self {
        OwnPresence {
            availability: Availability::PresenceUnknown,
            activity: Activity::Unknown,
        }
    }

    /// Map a vendored Graph [`PresenceInfo`] into an owned [`OwnPresence`].
    pub fn from_graph(info: &PresenceInfo) -> Self {
        OwnPresence {
            availability: Availability::from_wire(&info.availability),
            activity: Activity::from_wire(&info.activity),
        }
    }

    /// Drop the MRI from a [`ContactPresence`] (unified `own()` reuses the
    /// batch read).
    pub fn from_contact(contact: ContactPresence) -> Self {
        OwnPresence {
            availability: contact.availability,
            activity: contact.activity,
        }
    }
}

/// The base URL for the unified presence service: `region_gtms.presenceBase`
/// when present and non-empty, else [`DEFAULT_PRESENCE_BASE`].
pub fn presence_base_from_region(region: &Value) -> String {
    region
        .get("presenceBase")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_PRESENCE_BASE.to_string())
}

/// The Graph presence backend (own presence only).
///
/// Wraps the vendored Graph calls through the shared [`ApiClient`]. Live, the
/// Teams first-party token lacks the `Presence.Read*` scopes, so these 403; the
/// backend exists so a caller with a suitably-scoped token (or a stub in tests)
/// can still use it.
pub struct GraphBackend {
    client: Arc<ApiClient>,
}

impl GraphBackend {
    /// Build the backend over the shared protocol client.
    pub fn new(client: Arc<ApiClient>) -> Self {
        Self { client }
    }

    /// The shared protocol client.
    pub fn client(&self) -> &Arc<ApiClient> {
        &self.client
    }

    /// `GET /me/presence` (own presence; Graph has no batch read here).
    pub async fn own(&self) -> Result<OwnPresence> {
        let info = self.client.get_presence().await?;
        Ok(OwnPresence::from_graph(&info))
    }

    /// `POST /me/presence/setUserPreferredPresence`.
    pub async fn set(&self, availability: Availability, activity: Option<Activity>) -> Result<()> {
        let activity = activity.unwrap_or_else(|| default_activity(availability));
        self.client
            .set_user_preferred_presence(
                availability.as_str(),
                activity.as_str(),
                PRESENCE_EXPIRATION,
            )
            .await
    }
}

impl fmt::Debug for GraphBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GraphBackend")
            .field("client", &self.client)
            .finish()
    }
}

/// The unified (`presence.teams.microsoft.com`) backend.
///
/// The base URL is injectable so tests point it at a local stub; the token to
/// send is a parameter (see the module docs — the audience/header is
/// **unverified**).
pub struct UnifiedBackend {
    http: reqwest::Client,
    base: String,
    token: String,
    own_mri: Option<String>,
}

impl UnifiedBackend {
    /// Build the backend against `base` (trailing `/` trimmed) sending `token`.
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.into().trim_end_matches('/').to_string(),
            token: token.into(),
            own_mri: None,
        }
    }

    /// Record our own MRI so [`UnifiedBackend::own`] can ask for it explicitly.
    pub fn with_own_mri(mut self, mri: impl Into<String>) -> Self {
        self.own_mri = Some(mri.into());
        self
    }

    /// The resolved base URL.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Batch read: `POST {base}/v1/presence/getpresence/` with `{"ids":[…]}`.
    pub async fn get(&self, ids: &[String]) -> Result<Vec<ContactPresence>> {
        let url = format!("{}/v1/presence/getpresence/", self.base);
        let body = serde_json::json!({ "ids": ids });
        let value = self
            .send(reqwest::Method::POST, &url, Some(&body), "getpresence")
            .await?;
        Ok(parse_unified_presence(&value))
    }

    /// Own presence. With a known MRI this asks for it explicitly; otherwise it
    /// sends an empty id list and takes the first entry the service returns
    /// (a heuristic — see the module docs).
    pub async fn own(&self) -> Result<OwnPresence> {
        let ids: Vec<String> = match &self.own_mri {
            Some(mri) => vec![mri.clone()],
            None => Vec::new(),
        };
        let list = self.get(&ids).await?;
        Ok(list
            .into_iter()
            .next()
            .map(OwnPresence::from_contact)
            .unwrap_or_else(OwnPresence::unknown))
    }

    /// Set own status: `PUT {base}/v1/me/forceavailability/`.
    pub async fn set(&self, availability: Availability, activity: Option<Activity>) -> Result<()> {
        let url = format!("{}/v1/me/forceavailability/", self.base);
        let activity = activity.unwrap_or_else(|| default_activity(availability));
        let body = serde_json::json!({
            "availability": availability.as_str(),
            "activity": activity.as_str(),
        });
        self.send(reqwest::Method::PUT, &url, Some(&body), "forceavailability")
            .await
            .map(|_| ())
    }

    /// Send one request and parse a JSON (or empty) response.
    async fn send(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&Value>,
        op: &str,
    ) -> Result<Value> {
        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Accept", "application/json");
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| CoreError::Protocol(anyhow::Error::from(e)))?;
        let status = response.status();
        if !status.is_success() {
            return Err(CoreError::Protocol(anyhow::anyhow!(
                "presence {op} {url} returned status {status}"
            )));
        }
        let text = response
            .text()
            .await
            .map_err(|e| CoreError::Protocol(anyhow::Error::from(e)))?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| {
            CoreError::Protocol(anyhow::anyhow!(
                "presence {op} {url} returned invalid JSON: {e}"
            ))
        })
    }
}

impl fmt::Debug for UnifiedBackend {
    /// Redacting: the token is never printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnifiedBackend")
            .field("base", &self.base)
            .field("token", &"<redacted>")
            .field("own_mri", &self.own_mri)
            .finish()
    }
}

/// The two presence backends behind one interface.
#[derive(Debug)]
pub enum PresenceBackend {
    /// The Graph service (own presence only).
    Graph(GraphBackend),
    /// The unified `presence.teams.microsoft.com` service.
    Unified(UnifiedBackend),
}

impl PresenceBackend {
    /// A short label (`"graph"` / `"unified"`) for diagnostics.
    pub fn kind(&self) -> &'static str {
        match self {
            PresenceBackend::Graph(_) => "graph",
            PresenceBackend::Unified(_) => "unified",
        }
    }

    /// Batch read. The Graph service has no batch endpoint, so a Graph backend
    /// returns an empty list here (use [`PresenceBackend::own`] instead).
    pub async fn get(&self, ids: &[String]) -> Result<Vec<ContactPresence>> {
        match self {
            PresenceBackend::Graph(_) => {
                tracing::debug!("graph presence has no batch read; returning empty");
                Ok(Vec::new())
            }
            PresenceBackend::Unified(unified) => unified.get(ids).await,
        }
    }

    /// Read own presence.
    pub async fn own(&self) -> Result<OwnPresence> {
        match self {
            PresenceBackend::Graph(graph) => graph.own().await,
            PresenceBackend::Unified(unified) => unified.own().await,
        }
    }

    /// Set own availability (and optional activity).
    pub async fn set(&self, availability: Availability, activity: Option<Activity>) -> Result<()> {
        match self {
            PresenceBackend::Graph(graph) => graph.set(availability, activity).await,
            PresenceBackend::Unified(unified) => unified.set(availability, activity).await,
        }
    }
}

/// The presence service: a backend plus an in-memory cache of the last values
/// seen (so a UI can paint a contact's dot before the next fetch).
pub struct Presence {
    profile: String,
    backend: PresenceBackend,
    cache: Mutex<HashMap<String, ContactPresence>>,
    own: Mutex<Option<OwnPresence>>,
}

impl Presence {
    /// Build the service over an explicit backend.
    pub fn with_backend(profile: impl Into<String>, backend: PresenceBackend) -> Self {
        Self {
            profile: profile.into(),
            backend,
            cache: Mutex::new(HashMap::new()),
            own: Mutex::new(None),
        }
    }

    /// Build a **Graph** backend service over the shared protocol client.
    pub fn graph(profile: impl Into<String>, client: Arc<ApiClient>) -> Self {
        Self::with_backend(profile, PresenceBackend::Graph(GraphBackend::new(client)))
    }

    /// Build a **unified** backend service against `base`, sending `token`.
    pub fn unified(
        profile: impl Into<String>,
        base: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self::with_backend(
            profile,
            PresenceBackend::Unified(UnifiedBackend::new(base, token)),
        )
    }

    /// The account profile this service belongs to.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The backend in use.
    pub fn backend(&self) -> &PresenceBackend {
        &self.backend
    }

    /// Record our own MRI so the unified backend can ask for it explicitly.
    pub fn set_own_mri(&mut self, mri: impl Into<String>) {
        if let PresenceBackend::Unified(unified) = &mut self.backend {
            unified.own_mri = Some(mri.into());
        }
    }

    /// Batch-read the presence of `ids`, caching every entry returned.
    pub async fn get(&self, ids: &[String]) -> Result<Vec<ContactPresence>> {
        let found = self.backend.get(ids).await?;
        let mut cache = self.lock_cache();
        for contact in &found {
            cache.insert(contact.mri.clone(), contact.clone());
        }
        Ok(found)
    }

    /// The cached presence of one MRI, if a previous `get`/`own` saw it.
    pub fn cached(&self, mri: &str) -> Option<ContactPresence> {
        self.lock_cache().get(mri).cloned()
    }

    /// Every cached contact presence (unordered).
    pub fn cached_all(&self) -> Vec<ContactPresence> {
        self.lock_cache().values().cloned().collect()
    }

    /// Read own presence, caching the result.
    pub async fn own(&self) -> Result<OwnPresence> {
        let own = self.backend.own().await?;
        *self.lock_own() = Some(own);
        Ok(own)
    }

    /// The cached own presence, if `own`/`set` ran.
    pub fn cached_own(&self) -> Option<OwnPresence> {
        *self.lock_own()
    }

    /// Set own availability (and optional activity), caching the result.
    pub async fn set(&self, availability: Availability, activity: Option<Activity>) -> Result<()> {
        self.backend.set(availability, activity).await?;
        let activity = activity.unwrap_or_else(|| default_activity(availability));
        *self.lock_own() = Some(OwnPresence {
            availability,
            activity,
        });
        Ok(())
    }

    fn lock_cache(&self) -> MutexGuard<'_, HashMap<String, ContactPresence>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_own(&self) -> MutexGuard<'_, Option<OwnPresence>> {
        self.own.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl fmt::Debug for Presence {
    /// Redacting-ish: counts and backend kind, never tokens or status text.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Presence")
            .field("profile", &self.profile)
            .field("backend", &self.backend.kind())
            .field("cached", &self.lock_cache().len())
            .finish_non_exhaustive()
    }
}

// ----- mapping (pure, defensive) ------------------------------------------

/// Map the raw JSON of a unified `getpresence` response into contacts.
///
/// Accepts the shapes the service is known to use: a top-level array, or an
/// object wrapping the array under `value`/`presence`/`presences`/`result`, or a
/// single entry object. Each entry may carry `availability`/`activity` directly
/// or nested under a `presence` object. Entries without an `mri`/`id` are
/// dropped (they cannot be keyed); everything else is best-effort and never
/// panics.
pub fn parse_unified_presence(value: &Value) -> Vec<ContactPresence> {
    entries_of(value)
        .into_iter()
        .filter_map(parse_contact)
        .collect()
}

/// Map the raw JSON of a unified response into the first entry's own presence
/// (falls back to [`OwnPresence::unknown`]).
pub fn parse_unified_own(value: &Value) -> OwnPresence {
    parse_unified_presence(value)
        .into_iter()
        .next()
        .map(OwnPresence::from_contact)
        .unwrap_or_else(OwnPresence::unknown)
}

/// The list of candidate entry objects in a raw unified response.
fn entries_of(value: &Value) -> Vec<&Value> {
    if let Some(array) = value.as_array() {
        return array.iter().collect();
    }
    for key in ["value", "presence", "presences", "result"] {
        if let Some(array) = value.get(key).and_then(Value::as_array) {
            return array.iter().collect();
        }
    }
    if str_at(value, &["mri", "id"]).is_some() {
        return vec![value];
    }
    Vec::new()
}

/// Map one unified entry into a [`ContactPresence`].
fn parse_contact(entry: &Value) -> Option<ContactPresence> {
    let mri = str_at(entry, &["mri", "id", "user"])?;
    // Availability/activity may sit on the entry or inside a `presence` object.
    let inner = entry
        .get("presence")
        .filter(|v| v.is_object())
        .unwrap_or(entry);
    let availability = Availability::from_wire(
        &str_at(inner, &["availability", "availabilityText"])
            .or_else(|| str_at(entry, &["availability"]))
            .unwrap_or_default(),
    );
    let activity = Activity::from_wire(
        &str_at(inner, &["activity", "activityText"])
            .or_else(|| str_at(entry, &["activity"]))
            .unwrap_or_default(),
    );
    let status_message = status_message_of(inner).or_else(|| status_message_of(entry));
    Some(ContactPresence {
        mri,
        availability,
        activity,
        status_message,
    })
}

/// A non-empty (trimmed) string at the first present key (case-insensitive).
fn str_at(value: &Value, keys: &[&str]) -> Option<String> {
    let object = value.as_object()?;
    for key in keys {
        let found = object.get(*key).or_else(|| {
            object
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v)
        });
        match found {
            Some(Value::String(s)) if !s.trim().is_empty() => return Some(s.trim().to_string()),
            Some(Value::Number(n)) => return Some(n.to_string()),
            _ => {}
        }
    }
    None
}

/// The status message from an entry: a `statusMessage` string, or the
/// `message`/`text` inside a `statusMessage` object.
fn status_message_of(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    let field = object
        .get("statusMessage")
        .or_else(|| object.get("statusmessage"))?;
    match field {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Object(_) => str_at(field, &["message", "text"]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{StubResponse, http_stub};
    use crate::auth::tokens::{StoredToken, TokenSet};

    fn tokens_for(base: &str) -> TokenSet {
        let future = crate::auth::tokens::now_unix() + 3600;
        let mk = |name: &str| StoredToken::with_expiry(name, future);
        TokenSet {
            refresh_token: "refresh-secret".into(),
            tenant: "contoso.onmicrosoft.com".into(),
            region_gtms: serde_json::json!({ "graphBase": base, "presenceBase": base }),
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

    // ----- mapping --------------------------------------------------------

    #[test]
    fn availability_parses_wire_values_and_unknowns() {
        assert_eq!(
            Availability::from_wire("Available"),
            Availability::Available
        );
        assert_eq!(
            Availability::from_wire("availableidle"),
            Availability::AvailableIdle
        );
        assert_eq!(Availability::from_wire("Busy"), Availability::Busy);
        assert_eq!(Availability::from_wire("BusyIdle"), Availability::BusyIdle);
        assert_eq!(
            Availability::from_wire("DoNotDisturb"),
            Availability::DoNotDisturb
        );
        assert_eq!(Availability::from_wire("DND"), Availability::DoNotDisturb);
        assert_eq!(
            Availability::from_wire("BeRightBack"),
            Availability::BeRightBack
        );
        assert_eq!(Availability::from_wire("brb"), Availability::BeRightBack);
        assert_eq!(Availability::from_wire("Away"), Availability::Away);
        assert_eq!(Availability::from_wire("Offline"), Availability::Offline);
        // Anything unrecognised degrades — never panics.
        assert_eq!(
            Availability::from_wire("Available_Idle"),
            Availability::AvailableIdle
        );
        assert_eq!(
            Availability::from_wire("something-new"),
            Availability::PresenceUnknown
        );
        assert_eq!(Availability::from_wire(""), Availability::PresenceUnknown);
        // Round-trips through the wire string.
        for a in [
            Availability::Available,
            Availability::AvailableIdle,
            Availability::Busy,
            Availability::BusyIdle,
            Availability::DoNotDisturb,
            Availability::BeRightBack,
            Availability::Away,
            Availability::Offline,
            Availability::PresenceUnknown,
        ] {
            assert_eq!(Availability::from_wire(a.as_str()), a);
        }
        assert!(Availability::Available.is_available());
        assert!(!Availability::Busy.is_available());
    }

    #[test]
    fn activity_parses_wire_values_and_unknowns() {
        assert_eq!(Activity::from_wire("InACall"), Activity::InACall);
        assert_eq!(
            Activity::from_wire("InAConferenceCall"),
            Activity::InAConferenceCall
        );
        assert_eq!(Activity::from_wire("Presenting"), Activity::Presenting);
        assert_eq!(Activity::from_wire("OffWork"), Activity::OffWork);
        assert_eq!(Activity::from_wire("OutOfOffice"), Activity::OutOfOffice);
        assert_eq!(Activity::from_wire("busyidle"), Activity::Busy);
        for a in [
            Activity::Available,
            Activity::AvailableIdle,
            Activity::Busy,
            Activity::InACall,
            Activity::InAConferenceCall,
            Activity::DoNotDisturb,
            Activity::Presenting,
            Activity::Away,
            Activity::BeRightBack,
            Activity::Offline,
            Activity::OffWork,
            Activity::OutOfOffice,
            Activity::Unknown,
        ] {
            assert_eq!(Activity::from_wire(a.as_str()), a);
        }
        // Unknown and empty are Unknown.
        assert_eq!(Activity::from_wire("nope"), Activity::Unknown);
        assert_eq!(Activity::from_wire(""), Activity::Unknown);
        assert_eq!(default_activity(Availability::Offline), Activity::Offline);
    }

    #[test]
    fn parse_unified_presence_handles_flat_and_nested_arrays() {
        // A flat top-level array.
        let flat = serde_json::json!([
            { "mri": "8:orgid:aaaa", "availability": "Available", "activity": "Available" },
            { "mri": "8:orgid:bbbb", "availability": "Busy", "activity": "InACall",
              "statusMessage": "heads down" }
        ]);
        let parsed = parse_unified_presence(&flat);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].mri, "8:orgid:aaaa");
        assert_eq!(parsed[0].availability, Availability::Available);
        assert!(parsed[0].status_message.is_none());
        assert_eq!(parsed[1].availability, Availability::Busy);
        assert_eq!(parsed[1].activity, Activity::InACall);
        assert_eq!(parsed[1].status_message.as_deref(), Some("heads down"));

        // A wrapper object with `value`, entries nesting presence.
        let wrapped = serde_json::json!({
            "value": [{
                "id": "8:orgid:cccc",
                "presence": {
                    "availability": "DoNotDisturb",
                    "activity": "Presenting",
                    "statusMessage": { "message": "in a meeting" }
                }
            }]
        });
        let parsed = parse_unified_presence(&wrapped);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].mri, "8:orgid:cccc");
        assert_eq!(parsed[0].availability, Availability::DoNotDisturb);
        assert_eq!(parsed[0].activity, Activity::Presenting);
        assert_eq!(parsed[0].status_message.as_deref(), Some("in a meeting"));

        // A single entry object.
        let single = serde_json::json!({
            "mri": "8:orgid:dddd", "availability": "Away", "activity": "Away"
        });
        assert_eq!(parse_unified_presence(&single).len(), 1);
    }

    #[test]
    fn parse_unified_presence_is_defensive() {
        // Unknown availability/activity values degrade, entries without an MRI
        // are dropped, and odd shapes never panic.
        let odd = serde_json::json!([
            { "mri": "8:orgid:aaaa", "availability": "WhoKnows", "activity": "Whatever" },
            { "availability": "Available" },
            "a string",
            42
        ]);
        let parsed = parse_unified_presence(&odd);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].availability, Availability::PresenceUnknown);
        assert_eq!(parsed[0].activity, Activity::Unknown);

        for value in [
            Value::Null,
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!("text"),
            serde_json::json!(7),
        ] {
            assert!(parse_unified_presence(&value).is_empty());
            assert_eq!(parse_unified_own(&value), OwnPresence::unknown());
        }
    }

    #[test]
    fn graph_presence_info_maps_wire_values_and_unknowns() {
        let info = PresenceInfo {
            availability: "Busy".into(),
            activity: "InACall".into(),
        };
        let own = OwnPresence::from_graph(&info);
        assert_eq!(own.availability, Availability::Busy);
        assert_eq!(own.activity, Activity::InACall);

        // The vendored helper's own set: dnd → Presenting, offline → OffWork.
        let away = PresenceInfo {
            availability: "Offline".into(),
            activity: "OffWork".into(),
        };
        let own = OwnPresence::from_graph(&away);
        assert_eq!(own.availability, Availability::Offline);
        assert_eq!(own.activity, Activity::OffWork);

        // Unknown values never panic.
        let weird = PresenceInfo {
            availability: "Nonsense".into(),
            activity: String::new(),
        };
        let own = OwnPresence::from_graph(&weird);
        assert_eq!(own.availability, Availability::PresenceUnknown);
        assert_eq!(own.activity, Activity::Unknown);
    }

    #[test]
    fn presence_base_prefers_region_override() {
        assert_eq!(
            presence_base_from_region(&serde_json::json!({"presenceBase": "http://127.0.0.1:9/"})),
            "http://127.0.0.1:9"
        );
        assert_eq!(
            presence_base_from_region(&serde_json::json!({"presenceBase": ""})),
            DEFAULT_PRESENCE_BASE
        );
        assert_eq!(
            presence_base_from_region(&serde_json::json!({})),
            DEFAULT_PRESENCE_BASE
        );
        assert_eq!(
            presence_base_from_region(&serde_json::json!({"presenceBase": 3})),
            DEFAULT_PRESENCE_BASE
        );
    }

    // ----- Graph backend (through the shared stub) ------------------------

    #[tokio::test]
    async fn graph_backend_owns_and_reads_own_presence_from_the_stub() {
        let body = serde_json::json!({ "availability": "Busy", "activity": "InACall" }).to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;
        let client = Arc::new(ApiClient::from_tokens(&tokens_for(&base)));
        let presence = Presence::graph("default", client);

        let own = presence.own().await.expect("own presence");
        assert_eq!(own.availability, Availability::Busy);
        assert_eq!(own.activity, Activity::InACall);
        assert_eq!(presence.cached_own(), Some(own));

        // Graph cannot batch others' presence.
        let none = presence.get(&["8:orgid:aaaa".to_string()]).await.unwrap();
        assert!(none.is_empty());

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(
            req.starts_with("GET /me/presence "),
            "unexpected request line: {req}"
        );
        assert!(req.contains("authorization: Bearer graph-token"), "{req}");
    }

    #[tokio::test]
    async fn graph_backend_set_posts_the_preferred_presence_body() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok("{}")]).await;
        let client = Arc::new(ApiClient::from_tokens(&tokens_for(&base)));
        let presence = Presence::graph("default", client);

        presence
            .set(Availability::DoNotDisturb, None)
            .await
            .expect("set presence");
        // `set` caches the new own presence.
        assert_eq!(
            presence.cached_own().unwrap().availability,
            Availability::DoNotDisturb
        );

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(
            req.starts_with("POST /me/presence/setUserPreferredPresence "),
            "unexpected request line: {req}"
        );
        assert!(req.contains("\"availability\":\"DoNotDisturb\""), "{req}");
        assert!(req.contains("\"activity\":\"DoNotDisturb\""), "{req}");
        assert!(req.contains("\"expirationDuration\":\"PT1H\""), "{req}");
        assert!(req.contains("authorization: Bearer graph-token"), "{req}");
    }

    // ----- Unified backend (local stub) -----------------------------------

    #[tokio::test]
    async fn unified_backend_batch_read_sends_ids_and_token() {
        let body = serde_json::json!([
            { "mri": "8:orgid:aaaa", "availability": "Available", "activity": "Available" },
            { "mri": "8:orgid:bbbb", "availability": "Away", "activity": "Away" }
        ])
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;

        let backend = UnifiedBackend::new(&base, "skype-token");
        let got = backend
            .get(&["8:orgid:aaaa".to_string(), "8:orgid:bbbb".to_string()])
            .await
            .expect("get presence");
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].availability, Availability::Away);

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(
            req.starts_with("POST /v1/presence/getpresence/ "),
            "unexpected request line: {req}"
        );
        assert!(
            req.contains("\"ids\":[\"8:orgid:aaaa\",\"8:orgid:bbbb\"]"),
            "the ids must be the request body: {req}"
        );
        assert!(req.contains("authorization: Bearer skype-token"), "{req}");
    }

    #[tokio::test]
    async fn unified_backend_own_asks_for_our_mri_when_known() {
        let body = serde_json::json!([
            { "mri": "8:orgid:me", "availability": "Available", "activity": "Available" }
        ])
        .to_string();
        let (base, seen, task) = http_stub(vec![StubResponse::ok(body)]).await;

        let backend = UnifiedBackend::new(&base, "skype-token").with_own_mri("8:orgid:me");
        let own = backend.own().await.expect("own presence");
        assert_eq!(own.availability, Availability::Available);

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(req.contains("\"ids\":[\"8:orgid:me\"]"), "{req}");
    }

    #[tokio::test]
    async fn unified_backend_set_puts_forceavailability() {
        let (base, seen, task) = http_stub(vec![StubResponse::ok("{}")]).await;
        let backend = UnifiedBackend::new(&base, "skype-token");

        backend
            .set(Availability::Away, Some(Activity::BeRightBack))
            .await
            .expect("set presence");

        task.await.expect("stub drains");
        let req = requests(&seen)[0].clone();
        assert!(
            req.starts_with("PUT /v1/me/forceavailability/ "),
            "unexpected request line: {req}"
        );
        assert!(req.contains("\"availability\":\"Away\""), "{req}");
        assert!(req.contains("\"activity\":\"BeRightBack\""), "{req}");
        assert!(req.contains("authorization: Bearer skype-token"), "{req}");
    }

    #[tokio::test]
    async fn unified_backend_maps_an_error_status() {
        let (base, _seen, task) = http_stub(vec![StubResponse::status(500)]).await;
        let backend = UnifiedBackend::new(&base, "skype-token");
        let err = backend.get(&[]).await.unwrap_err();
        assert!(format!("{err}").contains("500"), "err = {err}");
        task.await.expect("stub drains");
    }

    // ----- service --------------------------------------------------------

    #[tokio::test]
    async fn presence_get_caches_and_set_updates_cached_own() {
        let get_body = serde_json::json!([
            { "mri": "8:orgid:aaaa", "availability": "Busy", "activity": "Busy" }
        ])
        .to_string();
        let (base, _seen, task) =
            http_stub(vec![StubResponse::ok(get_body), StubResponse::ok("{}")]).await;
        let mut presence = Presence::unified("default", &base, "skype-token");
        presence.set_own_mri("8:orgid:aaaa");

        let contacts = presence
            .get(&["8:orgid:aaaa".to_string()])
            .await
            .expect("get");
        assert_eq!(contacts.len(), 1);
        // Cached for an instant repaint.
        assert_eq!(
            presence.cached("8:orgid:aaaa").unwrap().availability,
            Availability::Busy
        );
        assert_eq!(presence.cached_all().len(), 1);
        assert!(presence.cached("8:orgid:zzzz").is_none());

        presence
            .set(Availability::Available, None)
            .await
            .expect("set");
        assert_eq!(
            presence.cached_own().unwrap().availability,
            Availability::Available
        );

        task.await.expect("stub drains");
    }

    #[test]
    fn presence_backend_debug_redacts_the_token() {
        let backend = UnifiedBackend::new("http://127.0.0.1:9", "super-secret-token");
        let dbg = format!("{backend:?}");
        assert!(dbg.contains("base"));
        assert!(!dbg.contains("super-secret-token"), "token leaked: {dbg}");
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn presence_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Presence>();
        assert_send_sync::<PresenceBackend>();
    }
}
