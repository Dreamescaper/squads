//! User settings, persisted as JSON under [`AppDirs::settings_file`].
//!
//! This is a **placeholder** the UI will grow (M10/M11). M3 intentionally keeps
//! it tiny: a theme choice and an initial window size. Every field is
//! `#[serde(default)]`, so an old file missing new fields — or a hand-edited one
//! with extra fields — still loads.
//!
//! Writes are atomic (unique temp file in the target directory, then `rename`),
//! so a crash mid-save can never leave a truncated `settings.json`. Unlike the
//! token store, settings are not secret and are written with the process default
//! permissions.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::paths::AppDirs;

/// Colour theme selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    /// Follow the operating system.
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

/// Initial window size in logical pixels.
///
/// Not a live window binding (that is the app's concern); it is the size the UI
/// should open at and persist back when the user resizes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowSettings {
    /// Width in logical pixels.
    pub width: f32,
    /// Height in logical pixels.
    pub height: f32,
}

impl Default for WindowSettings {
    fn default() -> Self {
        Self {
            width: 1080.0,
            height: 720.0,
        }
    }
}

/// Persisted user settings.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Colour theme.
    pub theme: Theme,
    /// Initial window size.
    pub window: WindowSettings,
}

impl Settings {
    /// The file these settings live in, for `dirs`.
    pub fn path(dirs: &AppDirs) -> PathBuf {
        dirs.settings_file()
    }

    /// Load settings from `dirs`.
    ///
    /// A **missing** file yields [`Settings::default`] (first run). A file that
    /// exists but cannot be parsed is an error, so a developer notices a typo
    /// rather than silently losing their settings — callers that prefer
    /// resilience can `unwrap_or_default()`.
    pub fn load(dirs: &AppDirs) -> Result<Self> {
        let path = dirs.settings_file();
        match std::fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(CoreError::io(&path, e)),
        }
    }

    /// Write settings atomically to `dirs`, creating the config directory.
    pub fn save(&self, dirs: &AppDirs) -> Result<()> {
        std::fs::create_dir_all(&dirs.config).map_err(|e| CoreError::io(&dirs.config, e))?;
        let json = serde_json::to_vec_pretty(self)?;
        write_atomic(&dirs.settings_file(), &json)
    }
}

/// Write `bytes` to `path` via a unique temp file + `rename` (atomic on one
/// filesystem). The temp file is removed if anything fails.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(CoreError::io(&tmp, e));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(CoreError::io(path, e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dirs(tag: &str) -> (PathBuf, AppDirs) {
        let base =
            std::env::temp_dir().join(format!("squads-settings-{tag}-{}", uuid::Uuid::new_v4()));
        (base.clone(), AppDirs::rooted_at(base))
    }

    #[test]
    fn defaults_are_sane() {
        let settings = Settings::default();
        assert_eq!(settings.theme, Theme::System);
        assert_eq!(settings.window.width, 1080.0);
        assert_eq!(settings.window.height, 720.0);
    }

    #[test]
    fn missing_file_yields_defaults_and_save_creates_it() {
        let (base, dirs) = temp_dirs("missing");
        assert_eq!(Settings::load(&dirs).unwrap(), Settings::default());

        let settings = Settings {
            theme: Theme::Dark,
            window: WindowSettings {
                width: 800.0,
                height: 600.0,
            },
        };
        settings.save(&dirs).unwrap();
        assert!(dirs.settings_file().is_file());
        assert_eq!(Settings::load(&dirs).unwrap(), settings);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn json_round_trip_and_theme_encoding() {
        let settings = Settings {
            theme: Theme::Light,
            window: WindowSettings {
                width: 1024.0,
                height: 768.0,
            },
        };
        let json = serde_json::to_string(&settings).unwrap();
        assert!(
            json.contains("\"light\""),
            "theme should be snake_case: {json}"
        );
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, settings);
    }

    #[test]
    fn partial_and_unknown_fields_use_defaults() {
        // A file written by an older build (no `window`) plus a field this build
        // does not know: both must load.
        let parsed: Settings =
            serde_json::from_str(r#"{"theme":"dark","future_field":42}"#).unwrap();
        assert_eq!(parsed.theme, Theme::Dark);
        assert_eq!(parsed.window, WindowSettings::default());

        // Only one window field present: the other keeps its default.
        let parsed: Settings = serde_json::from_str(r#"{"window":{"width":640}}"#).unwrap();
        assert_eq!(parsed.window.width, 640.0);
        assert_eq!(parsed.window.height, 720.0);
    }

    #[test]
    fn save_replaces_atomically_without_leaving_temp_files() {
        let (base, dirs) = temp_dirs("atomic");
        let first = Settings {
            theme: Theme::Dark,
            ..Settings::default()
        };
        first.save(&dirs).unwrap();
        // Overwrite with a different value: readers only ever see one or the
        // other, and no `.tmp` file survives.
        let second = Settings {
            theme: Theme::Light,
            ..Settings::default()
        };
        second.save(&dirs).unwrap();

        assert_eq!(Settings::load(&dirs).unwrap(), second);

        let leftovers: Vec<_> = std::fs::read_dir(&dirs.config)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn corrupt_file_is_an_error() {
        let (base, dirs) = temp_dirs("corrupt");
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(dirs.settings_file(), b"{ not json").unwrap();
        assert!(Settings::load(&dirs).is_err());
        std::fs::remove_dir_all(&base).ok();
    }
}
