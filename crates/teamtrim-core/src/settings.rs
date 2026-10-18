//! User settings, persisted as JSON under [`AppDirs::settings_file`].
//!
//! This grew from the M3 placeholder into the real preferences the M11 settings
//! screen edits: the colour [`Theme`] and [`Accent`], a UI font scale, a compact
//! density toggle, notification switches and a download directory. Every field
//! is `#[serde(default)]` **at the container level**, so an old file missing the
//! new keys loads with the struct's [`Default`] for each missing field (the
//! forward-compatibility property the M11 tests pin). A hand-edited file with
//! extra fields also loads.
//!
//! Writes are atomic (unique temp file in the target directory, then `rename`),
//! so a crash mid-save can never leave a truncated `settings.json`. Unlike the
//! token store, settings are not secret and are written with the process default
//! permissions.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::paths::AppDirs;

/// Smallest accepted [`Settings::font_scale`].
pub const FONT_SCALE_MIN: f32 = 0.8;
/// Largest accepted [`Settings::font_scale`].
pub const FONT_SCALE_MAX: f32 = 1.6;
/// The default UI font scale.
pub const FONT_SCALE_DEFAULT: f32 = 1.0;

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

/// Accent colour used for selection, links and the send button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Accent {
    /// Teams-like blue (the default).
    #[default]
    Blue,
    /// Teal.
    Teal,
    /// Purple.
    Purple,
    /// Orange.
    Orange,
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
///
/// `#[serde(default)]` at the **container** level means a missing key is filled
/// from [`Settings::default`] — so a file written before M11 (no `font_scale`,
/// no `notifications_enabled`, …) still loads and gets today's defaults rather
/// than a type-default `0.0`/`false`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Colour theme.
    pub theme: Theme,
    /// Accent colour.
    pub accent: Accent,
    /// Initial window size.
    pub window: WindowSettings,
    /// UI font scale (`1.0` = 100%).
    pub font_scale: f32,
    /// Compact density (tighter spacing / shorter rows).
    pub compact_mode: bool,
    /// Whether notifications are delivered at all.
    pub notifications_enabled: bool,
    /// Whether only mentions (and not every message) notify.
    pub notify_only_mentions: bool,
    /// Where downloads are written; `None` means the app's media directory.
    pub download_dir: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            accent: Accent::default(),
            window: WindowSettings::default(),
            font_scale: FONT_SCALE_DEFAULT,
            compact_mode: false,
            notifications_enabled: true,
            notify_only_mentions: false,
            download_dir: None,
        }
    }
}

impl Settings {
    /// The file these settings live in, for `dirs`.
    pub fn path(dirs: &AppDirs) -> PathBuf {
        dirs.settings_file()
    }

    /// [`Settings::font_scale`] clamped to the accepted range.
    ///
    /// A hand-edited or future file can carry an out-of-range value; the app
    /// applies this rather than trusting the file blindly.
    pub fn effective_font_scale(&self) -> f32 {
        if self.font_scale.is_finite() {
            self.font_scale.clamp(FONT_SCALE_MIN, FONT_SCALE_MAX)
        } else {
            FONT_SCALE_DEFAULT
        }
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
            std::env::temp_dir().join(format!("teamtrim-settings-{tag}-{}", uuid::Uuid::new_v4()));
        (base.clone(), AppDirs::rooted_at(base))
    }

    #[test]
    fn defaults_are_sane() {
        let settings = Settings::default();
        assert_eq!(settings.theme, Theme::System);
        assert_eq!(settings.accent, Accent::Blue);
        assert_eq!(settings.window.width, 1080.0);
        assert_eq!(settings.window.height, 720.0);
        assert_eq!(settings.font_scale, FONT_SCALE_DEFAULT);
        assert!(!settings.compact_mode);
        assert!(settings.notifications_enabled);
        assert!(!settings.notify_only_mentions);
        assert_eq!(settings.download_dir, None);
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
            font_scale: 1.25,
            compact_mode: true,
            notifications_enabled: false,
            notify_only_mentions: true,
            download_dir: Some(PathBuf::from("/tmp/teamtrim-downloads")),
            ..Settings::default()
        };
        settings.save(&dirs).unwrap();
        assert!(dirs.settings_file().is_file());
        assert_eq!(Settings::load(&dirs).unwrap(), settings);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn json_round_trip_and_enum_encoding() {
        let settings = Settings {
            theme: Theme::Light,
            accent: Accent::Teal,
            window: WindowSettings {
                width: 1024.0,
                height: 768.0,
            },
            font_scale: 1.1,
            compact_mode: true,
            notifications_enabled: true,
            notify_only_mentions: true,
            download_dir: Some(PathBuf::from("/data/dl")),
        };
        let json = serde_json::to_string(&settings).unwrap();
        assert!(
            json.contains("\"light\""),
            "theme should be snake_case: {json}"
        );
        assert!(
            json.contains("\"teal\""),
            "accent should be snake_case: {json}"
        );
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, settings);
    }

    #[test]
    fn forward_compatible_load_of_an_old_file_missing_new_keys() {
        // A file written before M11: only `theme` and `window` exist. Every new
        // field must come from `Settings::default`, not the type default
        // (`font_scale` must be 1.0, not 0.0; notifications must be on).
        let parsed: Settings = serde_json::from_str(
            r#"{"theme":"dark","window":{"width":640,"height":480},"future_field":42}"#,
        )
        .unwrap();
        assert_eq!(parsed.theme, Theme::Dark);
        assert_eq!(parsed.window.width, 640.0);
        assert_eq!(parsed.window.height, 480.0);
        assert_eq!(parsed.accent, Accent::Blue);
        assert_eq!(parsed.font_scale, FONT_SCALE_DEFAULT);
        assert!(!parsed.compact_mode);
        assert!(parsed.notifications_enabled);
        assert!(!parsed.notify_only_mentions);
        assert_eq!(parsed.download_dir, None);
    }

    #[test]
    fn partial_fields_keep_their_defaults() {
        // Only one window field present: the other keeps its default.
        let parsed: Settings = serde_json::from_str(r#"{"window":{"width":640}}"#).unwrap();
        assert_eq!(parsed.window.width, 640.0);
        assert_eq!(parsed.window.height, 720.0);
    }

    #[test]
    fn font_scale_is_clamped() {
        let too_big = Settings {
            font_scale: 12.0,
            ..Settings::default()
        };
        assert_eq!(too_big.effective_font_scale(), FONT_SCALE_MAX);
        let too_small = Settings {
            font_scale: 0.1,
            ..Settings::default()
        };
        assert_eq!(too_small.effective_font_scale(), FONT_SCALE_MIN);
        let nan = Settings {
            font_scale: f32::NAN,
            ..Settings::default()
        };
        assert_eq!(nan.effective_font_scale(), FONT_SCALE_DEFAULT);
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
