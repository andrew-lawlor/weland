//! Reading-pane keyboard shortcuts: a small, fixed set of navigation
//! actions, each bound to one key (plus optional Ctrl/Alt/Shift), remappable
//! from the settings popover (see `settings_ui.rs`) and persisted through
//! `persistence::Settings` the same read-modify-write way as every other
//! reading setting.
//!
//! Deliberately one binding per action, not a list of bindings — keeps both
//! the remap UI (one button per action) and the "does this keypress match my
//! one action" check trivial. Deliberately app-level shortcuts, not a
//! `gtk::ShortcutController`/action-based system — the remap UI needs to
//! capture an arbitrary raw keypress and immediately show/persist it, which
//! is simpler to reason about as one `EventControllerKey` doing a direct
//! keyval+modifier comparison than as GTK's action/trigger indirection.
//!
//! Steam Deck note: this app has no gamepad/controller code of its own.
//! Steam Input's own "Desktop Configuration" already lets a Deck user map
//! controller buttons to keyboard keys system-wide, so remapping these
//! shortcuts to convenient keys (arrows for scroll/chapter nav, bumpers as
//! Page Up/Down, a face button as Escape) is enough to make a full Deck
//! control layout work with zero extra code here.

use std::collections::HashMap;
use std::path::Path;

use gtk4::gdk;
use gtk4::glib::translate::{FromGlib, IntoGlib};

use crate::persistence::{self, KeyBinding};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    ScrollDown,
    ScrollUp,
    PageDown,
    PageUp,
    NextChapter,
    PrevChapter,
    ToggleSidebar,
    BackToLibrary,
}

impl Action {
    pub const ALL: [Action; 8] = [
        Action::ScrollDown,
        Action::ScrollUp,
        Action::PageDown,
        Action::PageUp,
        Action::NextChapter,
        Action::PrevChapter,
        Action::ToggleSidebar,
        Action::BackToLibrary,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Action::ScrollDown => "scroll_down",
            Action::ScrollUp => "scroll_up",
            Action::PageDown => "page_down",
            Action::PageUp => "page_up",
            Action::NextChapter => "next_chapter",
            Action::PrevChapter => "prev_chapter",
            Action::ToggleSidebar => "toggle_sidebar",
            Action::BackToLibrary => "back_to_library",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Action::ScrollDown => "Scroll down",
            Action::ScrollUp => "Scroll up",
            Action::PageDown => "Page down",
            Action::PageUp => "Page up",
            Action::NextChapter => "Next chapter",
            Action::PrevChapter => "Previous chapter",
            Action::ToggleSidebar => "Toggle sidebar",
            Action::BackToLibrary => "Back to library",
        }
    }

    fn default_binding(self) -> KeyBinding {
        let (key, modifiers) = match self {
            Action::ScrollDown => (gdk::Key::Down, gdk::ModifierType::empty()),
            Action::ScrollUp => (gdk::Key::Up, gdk::ModifierType::empty()),
            Action::PageDown => (gdk::Key::Page_Down, gdk::ModifierType::empty()),
            Action::PageUp => (gdk::Key::Page_Up, gdk::ModifierType::empty()),
            // Left/Right double as chapter navigation rather than horizontal
            // scroll -- this reading pane never scrolls horizontally, and
            // Left/Right map naturally onto a D-pad, which is exactly the
            // Steam Deck layout this module's doc comment has in mind.
            Action::NextChapter => (gdk::Key::Right, gdk::ModifierType::empty()),
            Action::PrevChapter => (gdk::Key::Left, gdk::ModifierType::empty()),
            Action::ToggleSidebar => (gdk::Key::F9, gdk::ModifierType::empty()),
            Action::BackToLibrary => (gdk::Key::Escape, gdk::ModifierType::empty()),
        };
        binding_for_key(key, modifiers)
    }
}

/// Only Ctrl/Alt/Shift are ever stored or compared — Caps/Num Lock and
/// mouse-button-drag bits ride along in a key event's modifier state but
/// aren't meaningful for a keyboard shortcut, and comparing them raw would
/// make a binding silently stop matching the moment Caps Lock is on.
fn relevant_modifiers(modifiers: gdk::ModifierType) -> gdk::ModifierType {
    modifiers & (gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SHIFT_MASK)
}

/// Builds the persisted/compared form of a keypress from the raw GDK values
/// `EventControllerKey::connect_key_pressed` hands back.
pub fn binding_for_key(key: gdk::Key, modifiers: gdk::ModifierType) -> KeyBinding {
    KeyBinding { keyval: key.into_glib(), modifiers: relevant_modifiers(modifiers).bits() }
}

/// Raw keyvals that are pure modifiers on their own — pressing just Shift,
/// or just Ctrl, is never a real shortcut, so the remap capture in
/// `settings_ui.rs` keeps listening rather than binding to one of these.
pub fn is_pure_modifier(key: gdk::Key) -> bool {
    matches!(
        key,
        gdk::Key::Shift_L
            | gdk::Key::Shift_R
            | gdk::Key::Control_L
            | gdk::Key::Control_R
            | gdk::Key::Alt_L
            | gdk::Key::Alt_R
            | gdk::Key::Super_L
            | gdk::Key::Super_R
            | gdk::Key::Meta_L
            | gdk::Key::Meta_R
            | gdk::Key::Caps_Lock
            | gdk::Key::ISO_Level3_Shift
    )
}

/// A short label for the settings row's button — e.g. "Ctrl+Right", "F9".
pub fn display(binding: KeyBinding) -> String {
    let modifiers = gdk::ModifierType::from_bits_truncate(binding.modifiers);
    let mut parts = Vec::new();
    if modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
        parts.push("Ctrl".to_string());
    }
    if modifiers.contains(gdk::ModifierType::ALT_MASK) {
        parts.push("Alt".to_string());
    }
    if modifiers.contains(gdk::ModifierType::SHIFT_MASK) {
        parts.push("Shift".to_string());
    }
    // Reconstructing a `Key` from a raw keyval is the same conversion
    // `EventControllerKey::connect_key_pressed` performs internally on every
    // event -- not a real safety hazard, just a newtype rewrap, but the
    // trait that does it is marked `unsafe` generically for FFI reasons.
    let key: gdk::Key = unsafe { FromGlib::from_glib(binding.keyval) };
    parts.push(key.name().map(|n| n.to_string()).unwrap_or_else(|| format!("Key {}", binding.keyval)));
    parts.join("+")
}

/// Every action bound to its compiled-in default — the fallback when no
/// config directory is available at all (settings just won't persist for
/// that session, same degraded behavior every other setting already has).
pub fn defaults() -> HashMap<Action, KeyBinding> {
    Action::ALL.into_iter().map(|a| (a, a.default_binding())).collect()
}

/// Every action's current binding — persisted overrides layered on top of
/// the built-in defaults, so a settings.json from before this feature
/// existed (or one that only overrides a couple of actions) still resolves
/// every other action to something sensible.
pub fn load(config_dir: &Path) -> HashMap<Action, KeyBinding> {
    let saved = persistence::read_settings(config_dir).keybindings.unwrap_or_default();
    defaults().into_iter().map(|(action, default)| (action, saved.get(action.id()).copied().unwrap_or(default))).collect()
}

/// Persists `action`'s new binding. At most one action may own a given key
/// combo: if another action currently holds this exact binding, its
/// override is dropped (falling it back to its own compiled-in default)
/// rather than leaving two actions racing for the same keypress —
/// `HashMap` iteration order is unspecified, so an actual collision would
/// resolve differently from run to run.
pub fn save(config_dir: &Path, action: Action, binding: KeyBinding) {
    let mut settings = persistence::read_settings(config_dir);
    let mut map = settings.keybindings.take().unwrap_or_default();
    for other in Action::ALL {
        if other != action {
            let other_current = map.get(other.id()).copied().unwrap_or_else(|| other.default_binding());
            if other_current == binding {
                map.remove(other.id());
            }
        }
    }
    map.insert(action.id().to_string(), binding);
    settings.keybindings = Some(map);
    let _ = persistence::write_settings(config_dir, &settings);
}

/// Sets `action`'s binding in `bindings`, stealing it from whichever other
/// action currently holds it (see `save`'s doc comment for why) — the
/// in-memory mirror of what `save` does to the persisted map, used by the
/// settings popover so a remap shows correctly immediately, even before (or
/// without) a successful disk write.
pub fn apply(bindings: &mut HashMap<Action, KeyBinding>, action: Action, binding: KeyBinding) {
    for other in Action::ALL {
        if other != action && bindings.get(&other).copied() == Some(binding) {
            bindings.insert(other, other.default_binding());
        }
    }
    bindings.insert(action, binding);
}

/// The action (if any) bound to this raw keypress.
pub fn action_for_key(bindings: &HashMap<Action, KeyBinding>, key: gdk::Key, modifiers: gdk::ModifierType) -> Option<Action> {
    let pressed = binding_for_key(key, modifiers);
    bindings.iter().find(|(_, b)| **b == pressed).map(|(a, _)| *a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn every_action_has_a_distinct_default_binding() {
        let defaults = defaults();
        assert_eq!(defaults.len(), Action::ALL.len());
        let mut seen = std::collections::HashSet::new();
        for binding in defaults.values() {
            assert!(seen.insert(*binding), "two actions must not share a default binding");
        }
    }

    #[test]
    fn action_for_key_finds_the_bound_action_and_ignores_unbound_keys() {
        let bindings = defaults();
        assert_eq!(action_for_key(&bindings, gdk::Key::Escape, gdk::ModifierType::empty()), Some(Action::BackToLibrary));
        assert_eq!(action_for_key(&bindings, gdk::Key::a, gdk::ModifierType::empty()), None);
    }

    #[test]
    fn save_persists_and_load_merges_with_defaults() {
        let dir = tempdir().unwrap();
        let custom = binding_for_key(gdk::Key::j, gdk::ModifierType::empty());
        save(dir.path(), Action::ScrollDown, custom);

        let loaded = load(dir.path());
        assert_eq!(loaded[&Action::ScrollDown], custom);
        // Every other action must still resolve to its default -- saving
        // one action's override must not disturb the rest.
        assert_eq!(loaded[&Action::ScrollUp], Action::ScrollUp.default_binding());
    }

    #[test]
    fn save_steals_a_binding_from_whichever_action_previously_held_it() {
        let dir = tempdir().unwrap();
        let escape = Action::BackToLibrary.default_binding();
        save(dir.path(), Action::ToggleSidebar, escape);

        let loaded = load(dir.path());
        assert_eq!(loaded[&Action::ToggleSidebar], escape, "the action just rebound must have the new key");
        assert_eq!(
            loaded[&Action::BackToLibrary],
            Action::BackToLibrary.default_binding(),
            "the action that lost the key must fall back to its own default, not keep dangling"
        );
    }

    #[test]
    fn apply_steals_a_binding_in_memory_without_touching_disk() {
        let mut bindings = defaults();
        let escape = Action::BackToLibrary.default_binding();
        apply(&mut bindings, Action::ToggleSidebar, escape);

        assert_eq!(bindings[&Action::ToggleSidebar], escape);
        assert_eq!(bindings[&Action::BackToLibrary], Action::BackToLibrary.default_binding());
    }

    #[test]
    fn relevant_modifiers_ignores_lock_and_button_bits() {
        let noisy = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::LOCK_MASK | gdk::ModifierType::BUTTON1_MASK;
        assert_eq!(relevant_modifiers(noisy), gdk::ModifierType::CONTROL_MASK);
    }
}
