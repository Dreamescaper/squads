//! Keyboard shortcut mapping (milestone **M11**).
//!
//! Shortcuts are a **pure function** of a key and the modifier state
//! ([`resolve`]), so the whole mapping is unit-tested without a display or an
//! `egui::Context`. [`crate::app::TeamTrimApp`] reads the raw key events from
//! egui's input and calls [`crate::app::TeamTrimApp::apply_shortcut`] for each
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
    /// Open the global message-search page (`Ctrl/Cmd+Shift+F`) — backlog item
    /// B1. The in-conversation filter keeps `Ctrl/Cmd+F`; the global search gets
    /// the `Shift` variant so the two meanings stay distinct (ADR-036).
    GlobalSearch,
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
    /// Jump to a numbered page (`Ctrl/Cmd+1`…`Ctrl/Cmd+5`). The app maps the
    /// slot to a [`crate::app::Page`] (1 = Chats, 2 = Calendar, 3 = Teams,
    /// 4 = Files, 5 = Calls).
    PageSlot(u8),
}

impl Shortcut {
    /// A short, stable label (for logs and tests).
    pub fn label(self) -> &'static str {
        match self {
            Shortcut::QuickSwitcher => "quick_switcher",
            Shortcut::SearchInChat => "search_in_chat",
            Shortcut::GlobalSearch => "global_search",
            Shortcut::CloseOverlay => "close_overlay",
            Shortcut::OpenSettings => "open_settings",
            Shortcut::ToggleMute => "toggle_mute",
            Shortcut::NextChat => "next_chat",
            Shortcut::PrevChat => "prev_chat",
            Shortcut::Send => "send",
            Shortcut::PageSlot(1) => "page_1",
            Shortcut::PageSlot(2) => "page_2",
            Shortcut::PageSlot(3) => "page_3",
            Shortcut::PageSlot(4) => "page_4",
            Shortcut::PageSlot(5) => "page_5",
            Shortcut::PageSlot(_) => "page",
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
        // `Ctrl/Cmd+Shift+F` searches *all* messages; plain `Ctrl/Cmd+F` keeps
        // its M11 meaning (filter the open conversation).
        Key::F if cmd && shift => Some(Shortcut::GlobalSearch),
        Key::F if cmd => Some(Shortcut::SearchInChat),
        // Page slots: Ctrl/Cmd+1…5 (see `PageSlot`).
        Key::Num1 if cmd => Some(Shortcut::PageSlot(1)),
        Key::Num2 if cmd => Some(Shortcut::PageSlot(2)),
        Key::Num3 if cmd => Some(Shortcut::PageSlot(3)),
        Key::Num4 if cmd => Some(Shortcut::PageSlot(4)),
        Key::Num5 if cmd => Some(Shortcut::PageSlot(5)),
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
    fn command_shift_maps_mute_and_global_search() {
        assert_eq!(resolve(Key::M, cmd_shift()), Some(Shortcut::ToggleMute));
        // Without shift, `M` is unbound.
        assert_eq!(resolve(Key::M, cmd()), None);
        // `Ctrl/Cmd+Shift+F` is the global message search (B1); plain
        // `Ctrl/Cmd+F` stays the in-conversation filter.
        assert_eq!(resolve(Key::F, cmd_shift()), Some(Shortcut::GlobalSearch));
        assert_eq!(resolve(Key::F, cmd()), Some(Shortcut::SearchInChat));
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
        assert_eq!(Shortcut::GlobalSearch.label(), "global_search");
        assert!(Shortcut::ToggleMute.is_placeholder());
        assert!(!Shortcut::Send.is_placeholder());
    }

    #[test]
    fn command_digit_maps_to_page_slots() {
        assert_eq!(resolve(Key::Num1, cmd()), Some(Shortcut::PageSlot(1)));
        assert_eq!(resolve(Key::Num2, cmd()), Some(Shortcut::PageSlot(2)));
        assert_eq!(resolve(Key::Num5, cmd()), Some(Shortcut::PageSlot(5)));
        // The slots are not placeholders and carry a stable label.
        assert_eq!(Shortcut::PageSlot(2).label(), "page_2");
        assert!(!Shortcut::PageSlot(2).is_placeholder());
        // A plain digit is unbound (no hijacking of text editing), and digits
        // beyond the bound range are unbound too.
        assert_eq!(resolve(Key::Num2, Modifiers::NONE), None);
        assert_eq!(resolve(Key::Num9, cmd()), None);
    }
}
