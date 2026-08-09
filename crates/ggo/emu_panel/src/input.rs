//! Keyboard -> GemdropGo button mask, ported from
//! `ggo-emu/src/native.rs`'s `KEY_MAP` + `refresh_input`.
//!
//! Same 18 bits, same keys, one deliberate difference: `native.rs` binds
//! SELECT to the RIGHT shift key (`winit::keyboard::KeyCode::ShiftRight`,
//! a *physical* key code). gpui does not expose left/right sidedness --
//! modifier presses arrive as [`gpui::Modifiers`], which has a single
//! `shift: bool` -- so here SELECT is EITHER shift key. See
//! [`InputState::set_select`].
//!
//! The other structural difference is polling vs. events. `native.rs`
//! keeps a `HashSet<KeyCode>` of held keys and rebuilds the whole mask
//! once per frame from the window; a gpui panel gets discrete key-down /
//! key-up events instead, so [`InputState`] maintains the mask
//! incrementally and the emulator thread reads the latest value at each
//! frame boundary. Net effect on the cart is identical: a level-triggered
//! 18-bit mask latched once per presented frame.

/// Number of buttons in the mask (`gemdrop_sdk::button::*` bits 0..=17).
pub const BUTTON_COUNT: u32 = 18;

/// SELECT -- the top bit (17), driven by the shift modifier rather than
/// by a character key (see the module doc).
pub const SELECT_BIT: u32 = 1 << (BUTTON_COUNT - 1);

/// gpui keystroke name -> button bit. Order and bit assignment copy
/// `ggo-emu/src/native.rs`'s `KEY_MAP` exactly: face buttons in bits
/// 0..=15, START at bit 16. Bit 17 (SELECT) is absent on purpose -- it
/// comes from the modifier path, not from a key name.
pub const KEY_MAP: &[(&str, u32)] = &[
    ("z", 1 << 0),
    ("x", 1 << 1),
    ("a", 1 << 2),
    ("s", 1 << 3),
    ("up", 1 << 4),
    ("down", 1 << 5),
    ("left", 1 << 6),
    ("right", 1 << 7),
    ("q", 1 << 8),
    ("w", 1 << 9),
    ("e", 1 << 10),
    ("r", 1 << 11),
    ("t", 1 << 12),
    ("y", 1 << 13),
    ("u", 1 << 14),
    ("i", 1 << 15),
    ("enter", 1 << 16), // START
];

/// The button bit `key` drives, or `None` for a key the pad doesn't use.
/// `key` is a [`gpui::Keystroke::key`] value, which is already lowercased
/// for character keys and spelled `"up"`/`"down"`/`"left"`/`"right"`/
/// `"enter"` for the named ones.
pub fn button_bit(key: &str) -> Option<u32> {
    KEY_MAP
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, bit)| *bit)
}

/// The latched button mask, driven by discrete key events.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InputState {
    mask: u32,
}

impl InputState {
    /// Apply a key-down (`down = true`) or key-up. Returns whether the
    /// mask actually changed, so the caller can skip a redundant publish
    /// on auto-repeat (gpui redelivers key-down while a key is held).
    pub fn key(&mut self, key: &str, down: bool) -> bool {
        let Some(bit) = button_bit(key) else {
            return false;
        };
        let next = if down {
            self.mask | bit
        } else {
            self.mask & !bit
        };
        let changed = next != self.mask;
        self.mask = next;
        changed
    }

    /// Apply the shift modifier as SELECT. Called from
    /// `on_modifiers_changed` rather than from a key event because a
    /// modifier-only press produces no keystroke.
    pub fn set_select(&mut self, held: bool) -> bool {
        let next = if held {
            self.mask | SELECT_BIT
        } else {
            self.mask & !SELECT_BIT
        };
        let changed = next != self.mask;
        self.mask = next;
        changed
    }

    pub fn mask(&self) -> u32 {
        self.mask
    }

    /// Release everything. Used when the pane loses focus and when a run
    /// stops -- without it, a key held while the user clicks away stays
    /// latched forever and the cart sees a stuck button.
    pub fn clear(&mut self) -> bool {
        let changed = self.mask != 0;
        self.mask = 0;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The port's load-bearing invariant: same 18 bits, each used once.
    /// Mirrors `ggo-emu/src/native.rs`'s
    /// `key_map_covers_all_18_buttons_uniquely`, with SELECT contributed
    /// by the modifier path instead of by a key name.
    #[test]
    fn key_map_plus_select_covers_all_18_buttons_uniquely() {
        let mut seen = 0u32;
        for (_, bit) in KEY_MAP {
            assert_eq!(seen & bit, 0, "duplicate button bit {bit:#x}");
            seen |= bit;
        }
        assert_eq!(seen & SELECT_BIT, 0, "SELECT must not be a key-name bit");
        seen |= SELECT_BIT;
        assert_eq!(seen, (1 << BUTTON_COUNT) - 1, "bits 0..=17 all mapped");
    }

    /// Pins the exact assignment against `ggo-emu/src/native.rs` so the
    /// pane and the standalone binary can't drift into disagreeing about
    /// what "A" is.
    #[test]
    fn key_map_matches_the_standalone_binarys_assignment() {
        assert_eq!(button_bit("z"), Some(1 << 0));
        assert_eq!(button_bit("x"), Some(1 << 1));
        assert_eq!(button_bit("a"), Some(1 << 2));
        assert_eq!(button_bit("s"), Some(1 << 3));
        assert_eq!(button_bit("up"), Some(1 << 4));
        assert_eq!(button_bit("down"), Some(1 << 5));
        assert_eq!(button_bit("left"), Some(1 << 6));
        assert_eq!(button_bit("right"), Some(1 << 7));
        assert_eq!(button_bit("i"), Some(1 << 15));
        assert_eq!(button_bit("enter"), Some(1 << 16));
    }

    #[test]
    fn unmapped_keys_are_ignored() {
        assert_eq!(button_bit("k"), None);
        assert_eq!(button_bit("escape"), None);
        assert_eq!(button_bit("shift"), None, "SELECT rides the modifier");

        let mut state = InputState::default();
        assert!(!state.key("k", true), "an unmapped key changes nothing");
        assert_eq!(state.mask(), 0);
    }

    #[test]
    fn presses_accumulate_and_releases_clear_only_their_own_bit() {
        let mut state = InputState::default();
        assert!(state.key("z", true));
        assert!(state.key("left", true));
        assert_eq!(state.mask(), (1 << 0) | (1 << 6));

        assert!(state.key("z", false));
        assert_eq!(state.mask(), 1 << 6, "releasing Z leaves LEFT held");
    }

    /// gpui redelivers key-down while a key is held; the mask must not
    /// report a change for those, so the panel can skip the publish.
    #[test]
    fn auto_repeat_reports_no_change() {
        let mut state = InputState::default();
        assert!(state.key("x", true));
        assert!(!state.key("x", true), "repeat is not a change");
        assert!(state.key("x", false));
        assert!(!state.key("x", false), "double release is not a change");
    }

    #[test]
    fn shift_drives_select_independently_of_the_key_bits() {
        let mut state = InputState::default();
        state.key("enter", true);
        assert!(state.set_select(true));
        assert_eq!(state.mask(), (1 << 16) | SELECT_BIT);
        assert!(!state.set_select(true), "still held is not a change");
        assert!(state.set_select(false));
        assert_eq!(state.mask(), 1 << 16, "START survives SELECT release");
    }

    #[test]
    fn clear_releases_everything_once() {
        let mut state = InputState::default();
        state.key("up", true);
        state.set_select(true);
        assert!(state.clear());
        assert_eq!(state.mask(), 0);
        assert!(!state.clear(), "clearing an empty mask is not a change");
    }
}
