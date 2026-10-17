//! The app's colour palette and theme installation (milestone **M10**).
//!
//! [`Settings::theme`] selects between the OS default ([`Theme::System`]), a
//! forced light or dark theme. The app defaults to **dark** (a chat client is
//! usually used in a dark shell) and re-applies the theme whenever the user
//! toggles it in the settings view.
//!
//! The [`Palette`] is deliberately tiny: the handful of semantic colours the
//! views need (own vs. other bubbles, pending/failed markers, presence dots)
//! plus the accent used for selection. It is derived from the chosen theme so a
//! view can ask for "the bubble colour" without knowing light vs. dark.

use egui::{Color32, Context, Visuals};
use squads_core::Theme;

/// Semantic colours the views draw with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Accent used for selection and the send button.
    pub accent: Color32,
    /// Background of a message bubble we authored.
    pub own_bubble: Color32,
    /// Background of a message bubble someone else authored.
    pub other_bubble: Color32,
    /// "Sending…" / pending marker.
    pub pending: Color32,
    /// "Failed" marker and error text.
    pub failed: Color32,
    /// Online / available presence dot.
    pub presence_online: Color32,
    /// Busy / do-not-disturb presence dot.
    pub presence_busy: Color32,
    /// Away / offline presence dot.
    pub presence_away: Color32,
}

impl Palette {
    /// The palette for a resolved (non-`System`) theme.
    pub fn for_theme(theme: Theme) -> Self {
        match theme {
            Theme::Light => Self {
                accent: Color32::from_rgb(0x4d, 0xa6, 0xff),
                own_bubble: Color32::from_rgb(0xd6, 0xe9, 0xff),
                other_bubble: Color32::from_rgb(0xf0, 0xf0, 0xf0),
                pending: Color32::from_rgb(0x8a, 0x8a, 0x8a),
                failed: Color32::from_rgb(0xc0, 0x2a, 0x2a),
                presence_online: Color32::from_rgb(0x2e, 0xa0, 0x43),
                presence_busy: Color32::from_rgb(0xc0, 0x2a, 0x2a),
                presence_away: Color32::from_rgb(0xc8, 0x9a, 0x00),
            },
            // `System` is resolved by the caller; treat it as dark here.
            Theme::Dark | Theme::System => Self {
                accent: Color32::from_rgb(0x4d, 0xa6, 0xff),
                own_bubble: Color32::from_rgb(0x1f, 0x3a, 0x5f),
                other_bubble: Color32::from_rgb(0x2b, 0x2b, 0x2b),
                pending: Color32::from_rgb(0x9a, 0x9a, 0x9a),
                failed: Color32::from_rgb(0xe0, 0x4f, 0x4f),
                presence_online: Color32::from_rgb(0x40, 0xc0, 0x57),
                presence_busy: Color32::from_rgb(0xe0, 0x4f, 0x4f),
                presence_away: Color32::from_rgb(0xd8, 0xb0, 0x20),
            },
        }
    }
}

/// Install the theme on the context and return the palette the views should use.
///
/// `System` follows egui's default (which tracks the OS), so no explicit
/// `Visuals` are set for it.
pub fn install(ctx: &Context, theme: Theme) -> Palette {
    apply(ctx, theme);
    Palette::for_theme(resolve(theme))
}

/// Re-apply a theme to a live context (used by the settings toggle).
pub fn apply(ctx: &Context, theme: Theme) {
    match theme {
        Theme::System => {}
        Theme::Light => ctx.set_visuals(Visuals::light()),
        Theme::Dark => ctx.set_visuals(Visuals::dark()),
    }
}

/// Resolve `System` to the concrete theme the palette should use. We cannot ask
/// egui which way it resolved (it may still be following the OS), so a `System`
/// setting is treated as dark — the same default egui ships.
pub fn resolve(theme: Theme) -> Theme {
    match theme {
        Theme::System => Theme::Dark,
        other => other,
    }
}
