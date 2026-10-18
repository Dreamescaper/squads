//! Token persistence.
//!
//! [`TokenStore`] is the abstraction the sign-in flow persists through. Three
//! implementations exist:
//!
//! - [`FileTokenStore`] — **the default**. One JSON file per account profile
//!   under `AppDirs::config`, written atomically (temp file + rename) with mode
//!   `0600` on Unix.
//! - [`MemoryTokenStore`] — for tests and ephemeral sessions.
//! - `KeyringTokenStore` — **deferred** (see the TODO below); only worth adding
//!   behind the `keyring` feature so the headless/CI build needs no keyring
//!   backend.
//!
//! Token *values* are never logged by any implementation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::auth::tokens::TokenSet;
use crate::error::{CoreError, Result};
use crate::paths::AppDirs;

/// The default (single-account) profile name.
pub const DEFAULT_PROFILE: &str = "default";

// TODO(M1/M3): implement `KeyringTokenStore` behind the `keyring` cargo feature
// (already declared in Cargo.toml, default off). It must:
//   * use `keyring::Entry::new("dev.teamtrim.teamtrim", &format!("tokens::{profile}"))`,
//   * store the serialized `TokenSet`,
//   * and fall back to `FileTokenStore` when no OS backend is available.
// The dependency is intentionally not declared yet so the default build and CI
// stay free of platform keyring/DBus requirements; the file store is the
// default.

/// Persists a [`TokenSet`] per account profile.
///
/// Implementations must be `Send + Sync` (the sign-in flow runs on the tokio
/// runtime) and must never log token contents.
pub trait TokenStore: Send + Sync {
    /// Load the stored set for `profile`, or `None` if the profile is unknown.
    fn load(&self, profile: &str) -> Result<Option<TokenSet>>;
    /// Write (replace) the stored set for `profile`.
    fn save(&self, profile: &str, tokens: &TokenSet) -> Result<()>;
    /// Remove any stored set for `profile`. Missing files are not an error.
    fn delete(&self, profile: &str) -> Result<()>;
}

/// JSON-file token store: one private file per profile.
///
/// File naming: the [`DEFAULT_PROFILE`] maps to `tokens.json`; any other profile
/// to `tokens-<sanitized>.json`. Writes go through a temporary file in the same
/// directory that is created with (and forced to) mode `0600` before being
/// renamed over the target, so a reader never sees a partially written file and
/// the permissions are never widened by the process umask.
pub struct FileTokenStore {
    dir: PathBuf,
    /// Serializes writes so two concurrent saves cannot race on temp files.
    write_lock: Mutex<()>,
}

impl FileTokenStore {
    /// Store under the config directory of `dirs`.
    pub fn new(dirs: &AppDirs) -> Self {
        Self::at(&dirs.config)
    }

    /// Store under an explicit directory (used by tests).
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            write_lock: Mutex::new(()),
        }
    }

    /// The directory holding the token files.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file path for `profile`.
    pub fn path_for(&self, profile: &str) -> PathBuf {
        self.dir.join(file_name_for(profile))
    }
}

/// `tokens.json` for the default profile, `tokens-<sanitized>.json` otherwise.
pub fn file_name_for(profile: &str) -> String {
    if profile.is_empty() || profile == DEFAULT_PROFILE {
        "tokens.json".to_string()
    } else {
        format!("tokens-{}.json", sanitize_profile(profile))
    }
}

/// Reduce a profile name to a safe filename component: keep ASCII alphanumerics
/// and `-_.`; replace everything else with `_`. Collisions (e.g. `a/b` vs
/// `a_b`) are possible but harmless for account nicknames.
fn sanitize_profile(profile: &str) -> String {
    let mut out: String = profile
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("profile");
    }
    out
}

impl TokenStore for FileTokenStore {
    fn load(&self, profile: &str) -> Result<Option<TokenSet>> {
        let path = self.path_for(profile);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(CoreError::io(&path, e)),
        };
        let set = serde_json::from_slice(&bytes).map_err(|e| {
            // Do not echo the file contents; only the path and parse error.
            CoreError::Auth(format!("token file {} is corrupt: {e}", path.display()))
        })?;
        Ok(Some(set))
    }

    fn save(&self, profile: &str, tokens: &TokenSet) -> Result<()> {
        std::fs::create_dir_all(&self.dir).map_err(|e| CoreError::io(&self.dir, e))?;
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        // Pretty-print: tokens are secrets, but the file is 0600 and this keeps
        // it readable for a developer debugging a local profile.
        let json = serde_json::to_vec_pretty(tokens)?;
        let path = self.path_for(profile);
        write_private(&path, &json)
    }

    fn delete(&self, profile: &str) -> Result<()> {
        let path = self.path_for(profile);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CoreError::io(&path, e)),
        }
    }
}

/// Write `bytes` to `path` atomically with mode `0600` on Unix.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;

    // Unique temp name in the same directory (rename is only atomic within a
    // filesystem).
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // Requested at creation; umask may reduce it, so we also set it below.
        opts.mode(0o600);
    }

    let write_result = (|| -> std::io::Result<()> {
        let mut file = opts.open(&tmp)?;
        file.write_all(bytes)?;
        let _ = file.sync_all();
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(CoreError::io(&tmp, e));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // Force exactly 0600 regardless of umask (mode() above is masked).
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| CoreError::io(&tmp, e))?;
    }

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(CoreError::io(path, e));
    }
    Ok(())
}

/// In-memory token store for tests and ephemeral sessions.
#[derive(Default)]
pub struct MemoryTokenStore {
    map: Mutex<HashMap<String, TokenSet>>,
}

impl MemoryTokenStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored profiles.
    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether no profiles are stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `profile` has a stored set.
    pub fn contains(&self, profile: &str) -> bool {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(profile)
    }
}

impl TokenStore for MemoryTokenStore {
    fn load(&self, profile: &str) -> Result<Option<TokenSet>> {
        Ok(self
            .map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(profile)
            .cloned())
    }

    fn save(&self, profile: &str, tokens: &TokenSet) -> Result<()> {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(profile.to_string(), tokens.clone());
        Ok(())
    }

    fn delete(&self, profile: &str) -> Result<()> {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(profile);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::tokens::{StoredToken, TokenKind};

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("teamtrim-store-{tag}-{}", uuid::Uuid::new_v4()))
    }

    fn sample(tenant: &str) -> TokenSet {
        let t = StoredToken::with_expiry("tok", 4_000_000_000);
        TokenSet {
            refresh_token: "refresh".into(),
            tenant: tenant.into(),
            region_gtms: serde_json::json!({"region": "emea"}),
            personal: false,
            spaces: t.clone(),
            skype: t.clone(),
            graph: t.clone(),
            ic3: t.clone(),
            recorder: t,
        }
    }

    #[test]
    fn default_profile_filename() {
        assert_eq!(file_name_for("default"), "tokens.json");
        assert_eq!(file_name_for(""), "tokens.json");
        assert_eq!(file_name_for("work"), "tokens-work.json");
        assert_eq!(file_name_for("weird/../name"), "tokens-weird_.._name.json");
    }

    #[test]
    fn file_store_round_trip_and_delete() {
        let dir = temp_dir("roundtrip");
        let store = FileTokenStore::at(&dir);

        assert!(store.load(DEFAULT_PROFILE).unwrap().is_none());

        let set = sample("common");
        store.save(DEFAULT_PROFILE, &set).unwrap();
        let back = store.load(DEFAULT_PROFILE).unwrap().expect("present");
        assert_eq!(back.tenant, "common");
        assert_eq!(back.refresh_token, "refresh");
        assert_eq!(back.region_gtms, set.region_gtms);

        store.delete(DEFAULT_PROFILE).unwrap();
        assert!(store.load(DEFAULT_PROFILE).unwrap().is_none());
        // Deleting a missing profile is fine.
        store.delete(DEFAULT_PROFILE).unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_store_isolates_profiles() {
        let dir = temp_dir("profiles");
        let store = FileTokenStore::at(&dir);
        store.save("default", &sample("common")).unwrap();
        store.save("personal", &sample("consumers")).unwrap();

        assert_eq!(store.load("default").unwrap().unwrap().tenant, "common");
        assert_eq!(store.load("personal").unwrap().unwrap().tenant, "consumers");
        assert!(store.path_for("default").ends_with("tokens.json"));
        assert!(store.path_for("personal").ends_with("tokens-personal.json"));

        // Removing one leaves the other intact.
        store.delete("personal").unwrap();
        assert!(store.load("default").unwrap().is_some());
        assert!(store.load("personal").unwrap().is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn file_store_is_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir("perms");
        let store = FileTokenStore::at(&dir);
        store.save(DEFAULT_PROFILE, &sample("common")).unwrap();

        let path = store.path_for(DEFAULT_PROFILE);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "token file must be user-read/write only");

        // Random other profiles too.
        store.save("work", &sample("common")).unwrap();
        let mode = std::fs::metadata(store.path_for("work"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_store_reports_corrupt_file_without_leaking_contents() {
        let dir = temp_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = FileTokenStore::at(&dir).path_for(DEFAULT_PROFILE);
        std::fs::write(&path, b"{ this is not json, and the secret is 'oops' }").unwrap();

        let err = FileTokenStore::at(&dir).load(DEFAULT_PROFILE).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("corrupt"));
        assert!(
            !msg.contains("oops"),
            "error must not echo file contents: {msg}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn memory_store_behaves_like_the_trait() {
        let store = MemoryTokenStore::new();
        assert!(store.is_empty());
        assert!(store.load("a").unwrap().is_none());

        store.save("a", &sample("common")).unwrap();
        store.save("b", &sample("consumers")).unwrap();
        assert_eq!(store.len(), 2);
        assert!(store.contains("a"));
        assert_eq!(store.load("b").unwrap().unwrap().tenant, "consumers");

        store.delete("a").unwrap();
        assert!(!store.contains("a"));
        assert!(store.load("a").unwrap().is_none());

        // The stored set survives by value, not by sharing.
        let mut s = sample("x");
        store.save("c", &s).unwrap();
        s.token_mut(TokenKind::Graph).token = "changed".into();
        assert_eq!(store.load("c").unwrap().unwrap().graph.token(), "tok");
    }
}
