//! Well-known filesystem locations for TeamTrim.
//!
//! Uses the platform conventions via [`directories`]: `~/.config/teamtrim`,
//! `~/.local/share/teamtrim` (Linux), `~/Library/Application Support/…` (macOS),
//! `%APPDATA%\teamtrim` (Windows).

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

use crate::error::{CoreError, Result};

/// Resolved application directories.
#[derive(Debug, Clone)]
pub struct AppDirs {
    /// Where configuration lives (config file, settings).
    pub config: PathBuf,
    /// Where durable state lives (SQLite store, cache index).
    pub data: PathBuf,
    /// Where disposable caches live (media, avatars, thumbnails).
    pub cache: PathBuf,
}

impl AppDirs {
    /// Resolve the platform directories for the current user.
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("dev", "teamtrim", "teamtrim").ok_or_else(|| {
            CoreError::NotReady("could not determine platform project directories".into())
        })?;
        Ok(Self {
            config: dirs.config_dir().to_path_buf(),
            data: dirs.data_dir().to_path_buf(),
            cache: dirs.cache_dir().to_path_buf(),
        })
    }

    /// Build a set of directories rooted at an explicit base (used by tests).
    pub fn rooted_at(base: impl AsRef<Path>) -> Self {
        let base = base.as_ref();
        Self {
            config: base.join("config"),
            data: base.join("data"),
            cache: base.join("cache"),
        }
    }

    /// Create every directory, returning which of them already existed is not
    /// reported; we only fail on real IO errors.
    pub fn ensure(&self) -> Result<()> {
        for dir in [&self.config, &self.data, &self.cache] {
            std::fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
        }
        Ok(())
    }

    /// Path to the settings/config file.
    pub fn settings_file(&self) -> PathBuf {
        self.config.join("settings.json")
    }

    /// Path to the local SQLite store.
    pub fn store_file(&self) -> PathBuf {
        self.data.join("store.sqlite")
    }

    /// Directory holding cached media, per account.
    pub fn media_dir(&self) -> PathBuf {
        self.cache.join("media")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rooted_layout_is_stable() {
        let dirs = AppDirs::rooted_at("/tmp/x");
        assert_eq!(dirs.config, PathBuf::from("/tmp/x/config"));
        assert_eq!(
            dirs.settings_file(),
            PathBuf::from("/tmp/x/config/settings.json")
        );
        assert_eq!(dirs.store_file(), PathBuf::from("/tmp/x/data/store.sqlite"));
    }

    #[test]
    fn ensure_creates_directories() {
        let tmp = std::env::temp_dir().join(format!("teamtrim-test-{}", uuid::Uuid::new_v4()));
        let dirs = AppDirs::rooted_at(&tmp);
        dirs.ensure().expect("ensure");
        assert!(dirs.config.is_dir());
        assert!(dirs.data.is_dir());
        assert!(dirs.cache.is_dir());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
