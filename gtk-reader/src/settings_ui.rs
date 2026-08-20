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

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::{prelude::*, Align, Box as GtkBox, Button, Label, Orientation, Popover, TextTag};

use crate::document::Tags;
use crate::fonts;
use crate::persistence::{self, Settings};

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

/// Builds the settings popover contents: a font grid, three +/- steppers,
/// and a verse-numbers toggle. Every change writes through
/// `persistence::write_settings` (read-modify-write) and immediately
/// re-applies to `base_font`/`tags` — no explicit "Save" step.
pub fn build_settings_popover(parent: &impl IsA<gtk4::Widget>, base_font: TextTag, tags: Rc<Tags>) -> Popover {
    let config_dir = persistence::config_dir().ok();
    let settings = Rc::new(RefCell::new(config_dir.as_ref().map(|d| persistence::read_settings(d)).unwrap_or_default()));

    let popover = Popover::new();
    popover.set_parent(parent);
    let container = GtkBox::new(Orientation::Vertical, 10);
    container.set_margin_top(10);
    container.set_margin_bottom(10);
    container.set_margin_start(10);
    container.set_margin_end(10);
    container.set_width_request(260);

    let font_label = Label::new(Some("Font"));
    font_label.set_halign(Align::Start);
    font_label.add_css_class("heading");
    container.append(&font_label);

    let font_grid = GtkBox::new(Orientation::Vertical, 2);
    for font in fonts::READING_FONTS {
        let btn = Button::with_label(font.label);
        let base_font_c = base_font.clone();
        let tags_c = tags.clone();
        let settings_c = settings.clone();
        let config_dir_c = config_dir.clone();
        let font_id = font.id.to_string();
        btn.connect_clicked(move |_| {
            settings_c.borrow_mut().reading_font = Some(font_id.clone());
            persist_and_apply(&config_dir_c, &settings_c, &base_font_c, &tags_c);
        });
        font_grid.append(&btn);
    }
    container.append(&font_grid);

    container.append(&stepper_row(
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
    container.append(&stepper_row(
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
    container.append(&stepper_row(
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

    let verse_numbers_row = GtkBox::new(Orientation::Horizontal, 6);
    let verse_numbers_label = Label::new(Some("Verse line numbers"));
    verse_numbers_label.set_halign(Align::Start);
    verse_numbers_label.set_hexpand(true);
    let verse_numbers_toggle = Button::with_label(if settings.borrow().reading_show_verse_numbers.unwrap_or(true) { "On" } else { "Off" });
    {
        let base_font_c = base_font.clone();
        let tags_c = tags.clone();
        let settings_c = settings.clone();
        let config_dir_c = config_dir.clone();
        let toggle_c = verse_numbers_toggle.clone();
        verse_numbers_toggle.connect_clicked(move |_| {
            let new_value = !settings_c.borrow().reading_show_verse_numbers.unwrap_or(true);
            settings_c.borrow_mut().reading_show_verse_numbers = Some(new_value);
            toggle_c.set_label(if new_value { "On" } else { "Off" });
            persist_and_apply(&config_dir_c, &settings_c, &base_font_c, &tags_c);
        });
    }
    verse_numbers_row.append(&verse_numbers_label);
    verse_numbers_row.append(&verse_numbers_toggle);
    container.append(&verse_numbers_row);

    popover.set_child(Some(&container));
    apply_settings(&base_font, &tags, &settings.borrow());
    popover
}

#[allow(clippy::too_many_arguments)]
fn stepper_row(
    label_text: &str,
    base_font: &TextTag,
    tags: &Rc<Tags>,
    settings: &Rc<RefCell<Settings>>,
    config_dir: &Option<std::path::PathBuf>,
    get: impl Fn(&Settings) -> f64 + 'static,
    set: impl Fn(&mut Settings, f64) + 'static,
    step: f64,
    format: impl Fn(f64) -> String + 'static,
) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 6);
    let label = Label::new(Some(label_text));
    label.set_halign(Align::Start);
    label.set_hexpand(true);

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

    row.append(&label);
    row.append(&down_btn);
    row.append(&value_label);
    row.append(&up_btn);
    row
}

fn persist_and_apply(config_dir: &Option<std::path::PathBuf>, settings: &Rc<RefCell<Settings>>, base_font: &TextTag, tags: &Tags) {
    apply_settings(base_font, tags, &settings.borrow());
    if let Some(dir) = config_dir {
        let _ = persistence::write_settings(dir, &settings.borrow());
    }
}
