//! The app's colour palette and theme installation (milestone **M10**, extended
//! in **M11** with accents, a font scale and a compact density).
//!
//! [`Settings::theme`] selects between the OS default ([`Theme::System`]), a
//! forced light or dark theme; [`Settings::accent`] picks the accent colour.
//! [`Settings::font_scale`] zooms the whole UI and [`Settings::compact_mode`]
//! tightens spacing. The app re-applies all of this whenever the user changes a
//! value in the settings view, and the choices persist through [`Settings`].
//!
//! The [`Palette`] is deliberately tiny: the handful of semantic colours the
//! views need (own vs. other bubbles, pending/failed markers, presence dots)
//! plus the accent used for selection. It is derived from the chosen theme and
//! accent so a view can ask for "the bubble colour" without knowing light vs.
//! dark.

use egui::{Color32, Context, Visuals};
use squads_core::{Accent, Settings, Theme};

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

/// The accent colour for an [`Accent`] choice.
pub fn accent_color(accent: Accent) -> Color32 {
    match accent {
        Accent::Blue => Color32::from_rgb(0x4d, 0xa6, 0xff),
        Accent::Teal => Color32::from_rgb(0x2f, 0xb3, 0xa3),
        Accent::Purple => Color32::from_rgb(0x9a, 0x6c, 0xf5),
        Accent::Orange => Color32::from_rgb(0xf2, 0x8b, 0x2b),
    }
}

impl Palette {
    /// The palette for a resolved (non-`System`) theme and an accent choice.
    pub fn for_theme(theme: Theme, accent: Accent) -> Self {
        let accent = accent_color(accent);
        match theme {
            Theme::Light => Self {
                accent,
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
                accent,
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

/// Install every themed setting on the context and return the palette the views
/// should use.
///
/// `System` follows egui's default (which tracks the OS), so no explicit
/// `Visuals` are set for it; the accent is still applied on top.
pub fn install(ctx: &Context, settings: &Settings) -> Palette {
    // M13: register the egui_extras image loaders so `egui::Image::from_uri`
    // can decode the `file://…` inline-media cache. Guarded so re-applying the
    // theme (a settings change) does not stack duplicate loaders; the loaders
    // are content-sniffed, not extension-bound.
    install_image_loaders_once(ctx);
    apply(ctx, settings);
    Palette::for_theme(resolve(settings.theme), settings.accent)
}

/// Install the egui_extras image loaders on `ctx` exactly once.
fn install_image_loaders_once(ctx: &Context) {
    let id = egui::Id::unique("squads::image_loaders_installed");
    if ctx.data(|data| data.get_temp::<bool>(id)).unwrap_or(false) {
        return;
    }
    egui_extras::install_image_loaders(ctx);
    ctx.data_mut(|data| data.insert_temp(id, true));
}

/// Re-apply every themed setting to a live context (used by the settings view).
pub fn apply(ctx: &Context, settings: &Settings) {
    match settings.theme {
        Theme::System => {}
        Theme::Light => ctx.set_visuals(Visuals::light()),
        Theme::Dark => ctx.set_visuals(Visuals::dark()),
    }

    let accent = accent_color(settings.accent);
    ctx.all_styles_mut(|style| {
        style.visuals.selection.bg_fill = accent;
        style.visuals.hyperlink_color = accent;
        if settings.compact_mode {
            style.spacing.item_spacing.y = 2.0;
            style.spacing.button_padding.y = 1.0;
        } else {
            let default = egui::style::Spacing::default();
            style.spacing.item_spacing = default.item_spacing;
            style.spacing.button_padding = default.button_padding;
        }
    });

    ctx.set_zoom_factor(settings.effective_font_scale());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accents_are_distinct() {
        let colors = [
            accent_color(Accent::Blue),
            accent_color(Accent::Teal),
            accent_color(Accent::Purple),
            accent_color(Accent::Orange),
        ];
        for (i, a) in colors.iter().enumerate() {
            for b in &colors[i + 1..] {
                assert_ne!(a, b, "accent colours must differ");
            }
        }
    }

    #[test]
    fn palette_uses_the_chosen_accent_and_resolves_system_as_dark() {
        let light = Palette::for_theme(Theme::Light, Accent::Teal);
        assert_eq!(light.accent, accent_color(Accent::Teal));
        let system = Palette::for_theme(resolve(Theme::System), Accent::Blue);
        let dark = Palette::for_theme(Theme::Dark, Accent::Blue);
        assert_eq!(system, dark);
    }
}
