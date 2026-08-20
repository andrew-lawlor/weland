//! Reading-settings UI: font, size, leading, verse spacing, verse numbers —
//! backed by Phase 1's `Settings` persistence (read-modify-write, per
//! CLAUDE.md's merge-safety requirement).
//!
//! Font family + size apply as `GtkTextTag` properties (`family`,
//! `size-points`) on one lowest-priority tag spanning the whole buffer,
//! *not* CSS on the text view — CSS `font-family`/`font-size` on
//! `textview text` provably has no effect on rendered buffer content in
//! this GTK version (tried first, verified with debug logging that the
//! provider loaded and applied without error, yet nothing changed
//! on-screen); the `TextTag` `family` property, by contrast, is already
//! proven working elsewhere in this codebase (`document.rs`'s `table` tag
//! uses it for its monospace rendering). Priority 0 keeps it below every
//! other tag document.rs creates, so heading weight/scale and the table
//! tag's own `family` override still win where they apply. Leading and
//! verse spacing approximate the web reader's CSS-multiplier behavior
//! (`--read-leading`, `--verse-stanza-gap`) by scaling other `GtkTextTag`
//! pixel-based line-spacing properties instead: GtkTextView doesn't respect
//! CSS `line-height`, and the closest available primitives are per-tag
//! absolute-pixel spacing, not a relative multiplier — a real, working
//! knob, just not a pixel-identical port of the CSS behavior. Verse numbers
//! toggle the shared `dim` tag's `invisible` property, since verse
//! line-number spans are the only thing that tag is used for.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::{gdk, glib, prelude::*, Align, Box as GtkBox, Button, EventControllerKey, Label, Orientation, PropagationPhase, TextTag};
use libadwaita::{self as adw, prelude::*};

use crate::document::Tags;
use crate::fonts;
use crate::keybindings::{self, Action};
use crate::persistence::{self, KeyBinding, Settings};

const BASE_PARAGRAPH_SPACING: f64 = 10.0;
const BASE_BLOCKQUOTE_SPACING: f64 = 8.0;
const BASE_LEADING: f64 = 1.75;
const BASE_VERSE_SPACING: f64 = 2.0; // matches the web reader's default rem value
const LEADING_MIN: f64 = 1.3;
const LEADING_MAX: f64 = 2.2;
const SIZE_MIN: f64 = 14.0;
const SIZE_MAX: f64 = 24.0;
const VERSE_SPACING_MIN: f64 = 0.5;
const VERSE_SPACING_MAX: f64 = 6.0;

/// Creates the whole-buffer base font tag at the lowest tag-table priority
/// and applies it over the buffer's current full extent. Call once, after
/// `document::build_document` has finished (so "current full extent" really
/// is the whole document).
pub fn install_base_font_tag(buffer: &gtk4::TextBuffer) -> TextTag {
    let tag = buffer.create_tag(Some("base_font"), &[]).expect("create base_font tag");
    tag.set_priority(0);
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    buffer.apply_tag(&tag, &start, &end);
    tag
}

/// Applies every field in `settings` to the live reading pane — called once
/// at startup with whatever was loaded from disk, and again after every
/// change from the settings panel.
pub fn apply_settings(base_font: &TextTag, tags: &Tags, settings: &Settings) {
    let family = fonts::family_for(settings.reading_font.as_deref().unwrap_or("literata"));
    let size = settings.reading_size_px.unwrap_or(17.0).clamp(SIZE_MIN, SIZE_MAX);
    base_font.set_family(Some(family));
    base_font.set_size_points(size);

    let leading = settings.reading_leading.unwrap_or(BASE_LEADING).clamp(LEADING_MIN, LEADING_MAX);
    let leading_scale = leading / BASE_LEADING;
    tags.paragraph.set_pixels_below_lines((BASE_PARAGRAPH_SPACING * leading_scale).round() as i32);
    tags.blockquote.set_pixels_below_lines((BASE_BLOCKQUOTE_SPACING * leading_scale).round() as i32);
    tags.verse.set_pixels_below_lines((2.0 * leading_scale).round() as i32);

    let verse_spacing = settings.reading_verse_spacing.unwrap_or(BASE_VERSE_SPACING).clamp(VERSE_SPACING_MIN, VERSE_SPACING_MAX);
    // 1rem ~ 16px as a reasonable, if approximate, conversion from the web
    // reader's rem-based --verse-stanza-gap to GTK's pixel-only spacing.
    tags.verse.set_pixels_above_lines((verse_spacing * 16.0).round() as i32);

    let show_numbers = settings.reading_show_verse_numbers.unwrap_or(true);
    tags.dim.set_invisible(!show_numbers);
}

/// Builds the reading settings as a proper `AdwPreferencesDialog` — it
/// outgrew a popover once keyboard shortcuts joined font/size/leading/verse
/// settings, and this is the idiomatic GNOME shape for "a general settings
/// menu" rather than a bigger popover: grouped `AdwPreferencesGroup`s of
/// `AdwActionRow`s across two pages (`Reading`, `Shortcuts`), with sidebar
/// navigation between them for free. Every change still writes through
/// `persistence::write_settings` (read-modify-write) and immediately
/// re-applies to `base_font`/`tags` — no explicit "Save" step. Call
/// `.present(Some(parent))` on the result to show it.
pub fn build_settings_dialog(base_font: TextTag, tags: Rc<Tags>) -> adw::PreferencesDialog {
    let config_dir = persistence::config_dir().ok();
    let settings = Rc::new(RefCell::new(config_dir.as_ref().map(|d| persistence::read_settings(d)).unwrap_or_default()));

    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Reading Settings");

    let reading_page = adw::PreferencesPage::new();
    reading_page.set_title("Reading");
    reading_page.set_icon_name(Some("preferences-desktop-font-symbolic"));

    let font_group = adw::PreferencesGroup::new();
    font_group.set_title("Font");
    let font_grid = GtkBox::new(Orientation::Vertical, 2);
    // `ToggleButton` + `set_group` instead of plain `Button`s -- radio-style
    // exclusivity gives the currently-selected font a pressed/active visual
    // state for free, which plain buttons never showed at all (the gap the
    // font list had until now).
    let current_font_id = settings.borrow().reading_font.clone().unwrap_or_else(|| "literata".to_string());
    let mut first_toggle: Option<gtk4::ToggleButton> = None;
    for font in fonts::READING_FONTS {
        let btn = gtk4::ToggleButton::builder().label(font.label).build();
        match &first_toggle {
            Some(first) => btn.set_group(Some(first)),
            None => first_toggle = Some(btn.clone()),
        }
        if current_font_id == font.id {
            btn.set_active(true);
        }

        let base_font_c = base_font.clone();
        let tags_c = tags.clone();
        let settings_c = settings.clone();
        let config_dir_c = config_dir.clone();
        let font_id = font.id.to_string();
        btn.connect_toggled(move |btn| {
            // `set_group` fires `toggled` on both the button losing the
            // selection and the one gaining it -- only the latter should
            // persist/apply anything.
            if !btn.is_active() {
                return;
            }
            settings_c.borrow_mut().reading_font = Some(font_id.clone());
            persist_and_apply(&config_dir_c, &settings_c, &base_font_c, &tags_c);
        });
        font_grid.append(&btn);
    }
    font_group.add(&font_grid);
    reading_page.add(&font_group);

    let layout_group = adw::PreferencesGroup::new();
    layout_group.set_title("Layout");
    layout_group.add(&stepper_action_row(
        "Size",
        &base_font,
        &tags,
        &settings,
        &config_dir,
        |s| s.reading_size_px.unwrap_or(17.0),
        |s, v| s.reading_size_px = Some(v.clamp(SIZE_MIN, SIZE_MAX)),
        1.0,
        |v| format!("{v:.0}px"),
    ));
    layout_group.add(&stepper_action_row(
        "Line spacing",
        &base_font,
        &tags,
        &settings,
        &config_dir,
        |s| s.reading_leading.unwrap_or(BASE_LEADING),
        |s, v| s.reading_leading = Some(v.clamp(LEADING_MIN, LEADING_MAX)),
        0.05,
        |v| format!("{v:.2}"),
    ));
    layout_group.add(&stepper_action_row(
        "Verse spacing",
        &base_font,
        &tags,
        &settings,
        &config_dir,
        |s| s.reading_verse_spacing.unwrap_or(BASE_VERSE_SPACING),
        |s, v| s.reading_verse_spacing = Some(v.clamp(VERSE_SPACING_MIN, VERSE_SPACING_MAX)),
        0.25,
        |v| format!("{v:.2}rem"),
    ));

    let verse_numbers_row = adw::SwitchRow::new();
    verse_numbers_row.set_title("Verse line numbers");
    verse_numbers_row.set_active(settings.borrow().reading_show_verse_numbers.unwrap_or(true));
    {
        let base_font_c = base_font.clone();
        let tags_c = tags.clone();
        let settings_c = settings.clone();
        let config_dir_c = config_dir.clone();
        verse_numbers_row.connect_active_notify(move |row| {
            settings_c.borrow_mut().reading_show_verse_numbers = Some(row.is_active());
            persist_and_apply(&config_dir_c, &settings_c, &base_font_c, &tags_c);
        });
    }
    layout_group.add(&verse_numbers_row);
    reading_page.add(&layout_group);

    dialog.add(&reading_page);
    dialog.add(&build_shortcuts_page(&dialog, &config_dir));

    apply_settings(&base_font, &tags, &settings.borrow());
    dialog
}

/// Builds the "Shortcuts" page: one `AdwActionRow` per `keybindings::Action`,
/// each showing its current key and, when clicked, capturing the next
/// keypress as its new binding. One `EventControllerKey` (Capture phase, so
/// it sees the key before the just-clicked button's own Space/Enter-
/// activates-me handling does), attached to `dialog` itself so it fires
/// regardless of which row's button was clicked — `remapping` tracks which
/// action, if any, is currently listening.
fn build_shortcuts_page(dialog: &adw::PreferencesDialog, config_dir: &Option<PathBuf>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.set_title("Shortcuts");
    page.set_icon_name(Some("input-keyboard-symbolic"));

    let group = adw::PreferencesGroup::new();
    group.set_title("Keyboard Shortcuts");
    group.set_description(Some(
        "Click a shortcut, then press a key to change it. Steam Deck: map controller buttons to these keys from Steam's own Desktop Configuration overlay.",
    ));

    let bindings: Rc<RefCell<HashMap<Action, KeyBinding>>> =
        Rc::new(RefCell::new(config_dir.as_ref().map(|d| keybindings::load(d)).unwrap_or_else(keybindings::defaults)));
    let buttons: Rc<RefCell<HashMap<Action, Button>>> = Rc::new(RefCell::new(HashMap::new()));
    let remapping: Rc<Cell<Option<Action>>> = Rc::new(Cell::new(None));

    for action in Action::ALL {
        let row = adw::ActionRow::new();
        row.set_title(action.label());

        let current = bindings.borrow()[&action];
        let key_btn = Button::with_label(&keybindings::display(current));
        key_btn.set_valign(Align::Center);
        {
            let remapping_c = remapping.clone();
            let btn_c = key_btn.clone();
            key_btn.connect_clicked(move |_| {
                remapping_c.set(Some(action));
                btn_c.set_label("Press a key\u{2026}");
            });
        }

        row.add_suffix(&key_btn);
        row.set_activatable_widget(Some(&key_btn));
        group.add(&row);
        buttons.borrow_mut().insert(action, key_btn);
    }
    page.add(&group);

    let key_controller = EventControllerKey::new();
    key_controller.set_propagation_phase(PropagationPhase::Capture);
    {
        let remapping_c = remapping.clone();
        let bindings_c = bindings.clone();
        let buttons_c = buttons.clone();
        let config_dir_c = config_dir.clone();
        key_controller.connect_key_pressed(move |_, keyval, _keycode, state| {
            let Some(action) = remapping_c.get() else { return glib::Propagation::Proceed };

            if keyval == gdk::Key::Escape {
                // Cancel the remap instead of binding Escape itself -- Escape
                // stays reserved as "cancel, leave the current binding
                // alone," matching common remap-UI convention (and it's
                // already the default Back-to-library key, so there's no
                // real loss).
                remapping_c.set(None);
                if let Some(btn) = buttons_c.borrow().get(&action) {
                    btn.set_label(&keybindings::display(bindings_c.borrow()[&action]));
                }
                return glib::Propagation::Stop;
            }
            if keybindings::is_pure_modifier(keyval) {
                return glib::Propagation::Stop;
            }

            let binding = keybindings::binding_for_key(keyval, state);
            keybindings::apply(&mut bindings_c.borrow_mut(), action, binding);
            if let Some(dir) = &config_dir_c {
                keybindings::save(dir, action, binding);
            }
            // A remap can steal the key from whichever other action held it
            // (see `keybindings::apply`'s doc comment) -- refresh every
            // row's label, not just the one just changed, so that other
            // action's button reflects its fallback-to-default too.
            for (a, b) in bindings_c.borrow().iter() {
                if let Some(btn) = buttons_c.borrow().get(a) {
                    btn.set_label(&keybindings::display(*b));
                }
            }
            remapping_c.set(None);
            glib::Propagation::Stop
        });
    }
    dialog.add_controller(key_controller);

    page
}

#[allow(clippy::too_many_arguments)]
fn stepper_action_row(
    label_text: &str,
    base_font: &TextTag,
    tags: &Rc<Tags>,
    settings: &Rc<RefCell<Settings>>,
    config_dir: &Option<PathBuf>,
    get: impl Fn(&Settings) -> f64 + 'static,
    set: impl Fn(&mut Settings, f64) + 'static,
    step: f64,
    format: impl Fn(f64) -> String + 'static,
) -> adw::ActionRow {
    let row = adw::ActionRow::new();
    row.set_title(label_text);

    let suffix = GtkBox::new(Orientation::Horizontal, 6);
    suffix.set_valign(Align::Center);
    let value_label = Label::new(Some(&format(get(&settings.borrow()))));
    let down_btn = Button::with_label("\u{2212}");
    let up_btn = Button::with_label("+");

    let get = Rc::new(get);
    let set = Rc::new(set);
    let format = Rc::new(format);

    for (btn, delta) in [(&down_btn, -step), (&up_btn, step)] {
        let base_font = base_font.clone();
        let tags = tags.clone();
        let settings = settings.clone();
        let config_dir = config_dir.clone();
        let value_label = value_label.clone();
        let get = get.clone();
        let set = set.clone();
        let format = format.clone();
        btn.connect_clicked(move |_| {
            let current = get(&settings.borrow());
            set(&mut settings.borrow_mut(), current + delta);
            value_label.set_label(&format(get(&settings.borrow())));
            persist_and_apply(&config_dir, &settings, &base_font, &tags);
        });
    }

    suffix.append(&down_btn);
    suffix.append(&value_label);
    suffix.append(&up_btn);
    row.add_suffix(&suffix);
    row
}

fn persist_and_apply(config_dir: &Option<std::path::PathBuf>, settings: &Rc<RefCell<Settings>>, base_font: &TextTag, tags: &Tags) {
    apply_settings(base_font, tags, &settings.borrow());
    if let Some(dir) = config_dir {
        let _ = persistence::write_settings(dir, &settings.borrow());
    }
}
