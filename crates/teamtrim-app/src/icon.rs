//! The app icon.
//!
//! The artwork is embedded in the binary as a 256×256 RGBA PNG
//! (`assets/icon.png`, a copy of `packaging/icons/dev.teamtrim.teamtrim-256.png`)
//! and handed to eframe via [`egui::ViewportBuilder::with_icon`]. That sets the
//! window/taskbar icon on X11 and Windows; Wayland ignores window icons and uses
//! the installed `.desktop` entry's app id instead, and macOS takes its icon from
//! the `.app` bundle's `.icns`. The files under `packaging/` are otherwise only
//! for launchers and installers — the running binary never reads them.
//!
//! Keep the source of truth in `packaging/icons/`; regenerate `assets/icon.png`
//! from it if the artwork changes (see `packaging/README.md`).

use eframe::egui;

/// The icon bitmap eframe expects: `width × height` RGBA8 pixels.
const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

/// Decode [`ICON_PNG`] into the RGBA bitmap eframe expects.
///
/// Returns `None` (and logs) if the embedded asset cannot be decoded, so a
/// damaged asset costs the icon but never the launch.
pub fn app_icon() -> Option<egui::IconData> {
    let image = match image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png) {
        Ok(image) => image.to_rgba8(),
        Err(error) => {
            tracing::warn!(%error, "could not decode the embedded app icon");
            return None;
        }
    };
    let (width, height) = image.dimensions();
    Some(egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    })
}
