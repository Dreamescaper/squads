//! Keyboard shortcut mapping (milestone **M11**).
//!
//! Shortcuts are a **pure function** of a key and the modifier state
//! ([`resolve`]), so the whole mapping is unit-tested without a display or an
//! `egui::Context`. [`crate::app::SquadsApp`] reads the raw key events from
//! egui's input and calls [`crate::app::SquadsApp::apply_shortcut`] for each
//! resolved [`Shortcut`]; the app owns the *dispatch* (turning a shortcut into a
//! queued [`crate::app::Action`]), and this module owns only the *mapping*.
//!
//! ## The `command` modifier
//!
//! egui sets [`egui::Modifiers::command`] to `ctrl` on Linux/Windows and to
//! `mac_cmd` (⌘) on macOS, so checking `command` alone gives a shortcut that is
//! `Ctrl+…` off macOS and `Cmd+…` on it — exactly the cross-platform behaviour we
//! want. A couple of shortcuts additionally require `Shift`.
//!
//! ## Unimplemented features
//!
//! Two mapped shortcuts are **documented placeholders**: `ToggleMute` (there is
//! no media device in this build, so it toggles an app-level flag and shows a
//! toast) and, on the send side, `Send` when no conversation is open (a no-op).
//! The mapping and dispatch exist and are tested regardless.

use egui::{Key, Modifiers};

/// A resolved user intent (the mapping's output).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    /// Open/close the quick chat switcher (`Ctrl/Cmd+K`).
    QuickSwitcher,
    /// Show the in-conversation message search (`Ctrl/Cmd+F`).
    SearchInChat,
    /// Close overlays and clear the active search (`Esc`).
    CloseOverlay,
    /// Jump to the settings page (`Ctrl/Cmd+,`).
    OpenSettings,
    /// Mute/unmute — a placeholder with no media device (`Ctrl/Cmd+Shift+M`).
    ToggleMute,
    /// Select the next chat (`j` or `Down`).
    NextChat,
    /// Select the previous chat (`k` or `Up`).
    PrevChat,
    /// Send the current draft (`Ctrl/Cmd+Enter`).
    Send,
}

impl Shortcut {
    /// A short, stable label (for logs and tests).
    pub fn label(self) -> &'static str {
        match self {
            Shortcut::QuickSwitcher => "quick_switcher",
            Shortcut::SearchInChat => "search_in_chat",
            Shortcut::CloseOverlay => "close_overlay",
            Shortcut::OpenSettings => "open_settings",
            Shortcut::ToggleMute => "toggle_mute",
            Shortcut::NextChat => "next_chat",
            Shortcut::PrevChat => "prev_chat",
            Shortcut::Send => "send",
        }
    }

    /// Whether this shortcut is a documented no-op / placeholder (kept so the
    /// UI can label it honestly).
    pub fn is_placeholder(self) -> bool {
        matches!(self, Shortcut::ToggleMute)
    }
}

/// Map a key press to a [`Shortcut`], or `None` when the key is not bound.
///
/// Pure and total: it only looks at `key` and `modifiers`, never at app state.
/// The caller decides whether a plain `j`/`k` should act (e.g. not while a text
/// field has focus).
pub fn resolve(key: Key, modifiers: Modifiers) -> Option<Shortcut> {
    let cmd = modifiers.command;
    let shift = modifiers.shift;
    match key {
        // Overlay / search control.
        Key::Escape => Some(Shortcut::CloseOverlay),
        // Command shortcuts. Order matters: the `cmd` arms precede the plain ones.
        Key::Comma if cmd => Some(Shortcut::OpenSettings),
        Key::M if cmd && shift => Some(Shortcut::ToggleMute),
        Key::Enter if cmd => Some(Shortcut::Send),
        Key::K if cmd => Some(Shortcut::QuickSwitcher),
        Key::F if cmd => Some(Shortcut::SearchInChat),
        // Plain navigation.
        Key::J | Key::ArrowDown if !cmd => Some(Shortcut::NextChat),
        Key::K | Key::ArrowUp if !cmd => Some(Shortcut::PrevChat),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd() -> Modifiers {
        Modifiers::COMMAND
    }

    fn cmd_shift() -> Modifiers {
        Modifiers::COMMAND | Modifiers::SHIFT
    }

    #[test]
    fn command_shortcuts_map() {
        assert_eq!(resolve(Key::K, cmd()), Some(Shortcut::QuickSwitcher));
        assert_eq!(resolve(Key::F, cmd()), Some(Shortcut::SearchInChat));
        assert_eq!(resolve(Key::Comma, cmd()), Some(Shortcut::OpenSettings));
        assert_eq!(resolve(Key::Enter, cmd()), Some(Shortcut::Send));
    }

    #[test]
    fn command_shift_maps_mute_and_not_the_plain_ones() {
        assert_eq!(resolve(Key::M, cmd_shift()), Some(Shortcut::ToggleMute));
        // Without shift, `M` is unbound.
        assert_eq!(resolve(Key::M, cmd()), None);
        // Shift does not change the other command shortcuts.
        assert_eq!(resolve(Key::K, cmd_shift()), Some(Shortcut::QuickSwitcher));
    }

    #[test]
    fn escape_always_closes() {
        assert_eq!(
            resolve(Key::Escape, Modifiers::NONE),
            Some(Shortcut::CloseOverlay)
        );
        assert_eq!(resolve(Key::Escape, cmd()), Some(Shortcut::CloseOverlay));
    }

    #[test]
    fn plain_navigation_maps_and_command_does_not() {
        assert_eq!(resolve(Key::J, Modifiers::NONE), Some(Shortcut::NextChat));
        assert_eq!(
            resolve(Key::ArrowDown, Modifiers::NONE),
            Some(Shortcut::NextChat)
        );
        assert_eq!(resolve(Key::K, Modifiers::NONE), Some(Shortcut::PrevChat));
        assert_eq!(
            resolve(Key::ArrowUp, Modifiers::NONE),
            Some(Shortcut::PrevChat)
        );

        // `Ctrl+J` / `Ctrl+Down` are not bound (no hijacking of text editing).
        assert_eq!(resolve(Key::J, cmd()), None);
        assert_eq!(resolve(Key::ArrowDown, cmd()), None);
        assert_eq!(resolve(Key::ArrowUp, cmd()), None);
    }

    #[test]
    fn unbound_keys_are_none() {
        for key in [Key::A, Key::Z, Key::Space, Key::Tab, Key::Backspace] {
            assert_eq!(resolve(key, Modifiers::NONE), None, "key = {key:?}");
            assert_eq!(resolve(key, cmd()), None, "key = {key:?}");
        }
    }

    #[test]
    fn labels_and_placeholders() {
        assert_eq!(Shortcut::QuickSwitcher.label(), "quick_switcher");
        assert!(Shortcut::ToggleMute.is_placeholder());
        assert!(!Shortcut::Send.is_placeholder());
    }
}
