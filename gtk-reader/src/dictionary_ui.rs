//! Dictionary lookup popover. Kept separate from `dictionary.rs`'s data
//! layer for the same reason `annotation_ui.rs` is split from
//! `annotations.rs` — a bug here is either "wrong lookup logic" or "wrong
//! GTK wiring," never both.
//!
//! No click handling of its own: `annotation_ui.rs`'s existing release
//! handler detects when a selection exactly spans one whole word — which is
//! exactly what GTK's own double-click-to-select-word produces, natively
//! and pixel-accurately — and calls `show_word_lookup` directly instead of
//! opening the highlight/note/record popover. An earlier version of this
//! module did its own click-position-to-word-boundary math on a
//! right-click, which needed the same `TextIter` word-boundary handling
//! `annotation_ui.rs` already gets right for free by asking the buffer for
//! its current selection instead of re-deriving word boundaries from a
//! pixel position.

use std::rc::Rc;

use gtk4::{gdk, prelude::*, Align, Box as GtkBox, Button, Label, Orientation, PolicyType, Popover, ScrolledWindow, TextBuffer, TextView};

use crate::annotation_ui::{self, AnnotationState};
use crate::dictionary::{self, DictionaryEntry};
use crate::{persistence, vocab_ui};

/// How much buffer text to pull in on each side of the looked-up word for
/// the vocab-builder's saved context — enough for a clause or short
/// sentence without dragging in unrelated surrounding paragraphs.
const VOCAB_CONTEXT_RADIUS: i32 = 100;

/// Looks up `word` locally (no online fallback — the bundled dataset is the
/// only source now, per CLAUDE.md's "no runtime network calls" preference)
/// and shows a popover at `rect` (widget coordinates) with the results.
/// `word_start`/`word_end` are the word's buffer offsets — only needed for
/// the "Add to Vocab" button, to pull surrounding context out of `buffer` at
/// save time. `state` registers this popover in the same dismiss-tracking
/// `annotation_ui.rs` uses for its own popovers — without it, this popover
/// doesn't get the `set_autohide(false)` + manual-dismiss treatment either,
/// and the same "can't close it by clicking outside" GTK-autohide-timing
/// race that hit the annotation popovers hits this one too (confirmed live:
/// it did, since this module never got that fix).
pub fn show_word_lookup(text_view: &TextView, buffer: &TextBuffer, rect: gdk::Rectangle, word: &str, word_start: i32, word_end: i32, state: &Rc<AnnotationState>) {
    let entries = dictionary::lookup_word(word).unwrap_or_default();
    let popover = build_lookup_popover(text_view, buffer, &rect, word, word_start, word_end, entries, state);
    popover.popup();
    annotation_ui::track_popover(state, &popover);
}

/// Builds (but doesn't show) a definitions popover — a fresh `Popover` every
/// time, never a mutated live one (see `annotation_ui.rs`'s
/// `show_comment_composer` notes on why that broke there).
#[allow(clippy::too_many_arguments)]
fn build_lookup_popover(
    text_view: &TextView,
    buffer: &TextBuffer,
    rect: &gdk::Rectangle,
    word: &str,
    word_start: i32,
    word_end: i32,
    entries: Vec<DictionaryEntry>,
    state: &Rc<AnnotationState>,
) -> Popover {
    let popover = Popover::new();
    popover.set_parent(text_view);
    // See `annotation_ui.rs`'s identical call for why: GTK's own
    // autohide-on-outside-click reacts to button *press*, a full event
    // cycle before `handle_release`'s own dismiss-on-release logic ever
    // runs, so leaving it on raced with our own tracking.
    popover.set_autohide(false);
    popover.set_pointing_to(Some(rect));
    popover.set_size_request(320, -1);

    let container = GtkBox::new(Orientation::Vertical, 6);
    let title = Label::new(Some(word));
    title.set_halign(Align::Start);
    title.add_css_class("heading");
    container.append(&title);

    if entries.is_empty() {
        let empty = Label::new(Some("No definition found."));
        empty.set_halign(Align::Start);
        empty.add_css_class("dim-label");
        container.append(&empty);
    } else {
        // Entries can be long (a headword's full entry — every part of
        // speech, every sense, etymology, quotations — is one row here,
        // unlike the old dictionary's short one-sense-per-row entries), so
        // this scrolls with a cap instead of letting one popover grow to
        // fill the screen.
        let defs_box = GtkBox::new(Orientation::Vertical, 10);
        for entry in entries.iter().take(5) {
            let def = Label::new(Some(&html_to_display_text(&entry.definition)));
            def.set_wrap(true);
            def.set_halign(Align::Start);
            def.set_justify(gtk4::Justification::Left);
            defs_box.append(&def);
        }
        let scroller = ScrolledWindow::builder().child(&defs_box).hscrollbar_policy(PolicyType::Never).max_content_height(360).propagate_natural_height(true).build();
        container.append(&scroller);

        // Checked fresh on every popover build (not cached) — cheap next to
        // everything else here, and it's the only way a second lookup of a
        // word already saved shows the right state instead of always
        // offering "Add to Vocab" and letting duplicates pile up.
        let already_saved = word_already_in_vocab(word);
        let add_btn = Button::with_label(if already_saved { "Added \u{2713}" } else { "Add to Vocab" });
        add_btn.set_sensitive(!already_saved);
        add_btn.set_halign(Align::Start);
        let definition = html_to_display_text(&entries[0].definition);
        let word_owned = word.to_string();
        let buffer_c = buffer.clone();
        let state_c = state.clone();
        add_btn.connect_clicked(move |btn| {
            let (before, after) = context_around(&buffer_c, word_start, word_end);
            if let Ok(dir) = persistence::config_dir() {
                let _ = persistence::add_vocab_entry(&dir, &word_owned, &definition, &before, &after, &state_c.title);
                vocab_ui::refresh_vocab_list(&state_c);
            }
            btn.set_label("Added \u{2713}");
            btn.set_sensitive(false);
        });
        container.append(&add_btn);
    }

    popover.set_child(Some(&container));
    popover
}

/// Whether `word` is already on the saved vocab list, checked
/// case-insensitively (a fresh lookup, not cached — the list is a handful to
/// a few hundred entries, cheap to scan on every popover build).
fn word_already_in_vocab(word: &str) -> bool {
    let Ok(dir) = persistence::config_dir() else { return false };
    let entries = persistence::read_vocab(&dir).unwrap_or_default();
    entries.iter().any(|e| e.word.eq_ignore_ascii_case(word))
}

/// Pulls up to `VOCAB_CONTEXT_RADIUS` characters of buffer text on each side
/// of `[word_start, word_end)`, trimmed to whole-word boundaries (so a
/// snippet doesn't open or close mid-word) for the vocab-builder's saved
/// context.
fn context_around(buffer: &TextBuffer, word_start: i32, word_end: i32) -> (String, String) {
    let buf_start = buffer.start_iter().offset();
    let buf_end = buffer.end_iter().offset();
    let before_from = (word_start - VOCAB_CONTEXT_RADIUS).max(buf_start);
    let after_to = (word_end + VOCAB_CONTEXT_RADIUS).min(buf_end);

    let mut before = buffer.text(&buffer.iter_at_offset(before_from), &buffer.iter_at_offset(word_start), false).to_string();
    let mut after = buffer.text(&buffer.iter_at_offset(word_end), &buffer.iter_at_offset(after_to), false).to_string();

    if before_from > buf_start {
        if let Some(idx) = before.find(char::is_whitespace) {
            before = before[idx + 1..].to_string();
        }
    }
    if after_to < buf_end {
        if let Some(idx) = after.rfind(char::is_whitespace) {
            after = after[..idx].to_string();
        }
    }

    (before.trim().to_string(), after.trim().to_string())
}

/// Converts one `entries.definition` HTML blob (`<p>`/`<b>`/`<i>`/`<ol>`/
/// `<li>`/HTML entities — see `dictionary.rs`) into plain display text.
/// Deliberately plain text, not Pango markup: `Label::set_markup()` would
/// crash/fail on anything malformed, and this dataset has 849k entries —
/// nowhere near feasible to validate all of them by hand. Stripping tags
/// (keeping just paragraph breaks and list-item bullets for structure) is
/// the safe choice; losing bold/italic emphasis is a minor cosmetic
/// trade-off next to that.
fn html_to_display_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut list_depth: u32 = 0;
    let chars: Vec<char> = html.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '<' => {
                if let Some(rel_end) = chars[i..].iter().position(|&c| c == '>') {
                    let tag: String = chars[i + 1..i + rel_end].iter().collect();
                    let closing = tag.starts_with('/');
                    let name = tag.trim_start_matches('/').split_whitespace().next().unwrap_or("").to_ascii_lowercase();
                    match name.as_str() {
                        "p" | "div" if !closing => {
                            if !out.trim_end_matches(' ').is_empty() {
                                out.push('\n');
                                out.push('\n');
                            }
                        }
                        "li" if !closing => {
                            if !out.is_empty() && !out.ends_with('\n') {
                                out.push('\n');
                            }
                            for _ in 1..list_depth {
                                out.push_str("  ");
                            }
                            out.push_str("\u{2022} ");
                        }
                        "ol" | "ul" => {
                            if closing {
                                list_depth = list_depth.saturating_sub(1);
                            } else {
                                list_depth += 1;
                            }
                        }
                        "br" => out.push('\n'),
                        _ => {}
                    }
                    i += rel_end + 1;
                } else {
                    // Unterminated tag -- treat the rest as plain text
                    // rather than silently dropping it.
                    out.extend(&chars[i..]);
                    break;
                }
            }
            '&' => {
                if let Some(rel_end) = chars[i..].iter().position(|&c| c == ';') {
                    let entity: String = chars[i + 1..i + rel_end].iter().collect();
                    out.push_str(&decode_entity(&entity));
                    i += rel_end + 1;
                } else {
                    out.push('&');
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }

    // Collapse the runs of blank lines/spaces that block-tag handling above
    // tends to produce (e.g. adjacent `<p>` tags with no text between them).
    let collapsed: String = out.lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
    let mut result = String::with_capacity(collapsed.len());
    let mut blank_run = 0;
    for line in collapsed.lines() {
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        result.push_str(line);
        result.push('\n');
    }
    result.trim().to_string()
}

fn decode_entity(entity: &str) -> String {
    if let Some(hex) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
        if let Ok(code) = u32::from_str_radix(hex, 16) {
            if let Some(c) = char::from_u32(code) {
                return c.to_string();
            }
        }
    }
    if let Some(dec) = entity.strip_prefix('#') {
        if let Ok(code) = dec.parse::<u32>() {
            if let Some(c) = char::from_u32(code) {
                return c.to_string();
            }
        }
    }
    match entity {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        "nbsp" => " ".to_string(),
        "lsqb" => "[".to_string(),
        "rsqb" => "]".to_string(),
        "lsquo" => "\u{2018}".to_string(),
        "rsquo" => "\u{2019}".to_string(),
        "ldquo" => "\u{201c}".to_string(),
        "rdquo" => "\u{201d}".to_string(),
        "mdash" => "\u{2014}".to_string(),
        "ndash" => "\u{2013}".to_string(),
        "hellip" => "\u{2026}".to_string(),
        // Unknown entity: keep it recognizable rather than silently
        // dropping content (e.g. a typo'd or rare named entity this table
        // doesn't cover).
        other => format!("&{other};"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn strips_simple_inline_tags() {
        assert_eq!(html_to_display_text("<p>A <b>bold</b> word.</p>"), "A bold word.");
    }

    #[test]
    fn converts_list_items_to_bulleted_lines() {
        let html = "<p><b>Noun</b></p><ol><li>First sense.</li><li>Second sense.</li></ol>";
        let text = html_to_display_text(html);
        assert_eq!(text, "Noun\n\u{2022} First sense.\n\u{2022} Second sense.");
    }

    #[test]
    fn decodes_numeric_and_named_entities() {
        assert_eq!(html_to_display_text("computer-assisted&#47;aided"), "computer-assisted/aided");
        assert_eq!(html_to_display_text("&lsqb;from 8th c.&rsqb;"), "[from 8th c.]");
        assert_eq!(html_to_display_text("Q&amp;A"), "Q&A");
    }

    #[test]
    fn collapses_multiple_blank_lines() {
        let text = html_to_display_text("<p>One</p><p></p><p>Two</p>");
        assert_eq!(text, "One\n\nTwo");
    }

    #[test]
    fn handles_real_cat_style_entry_without_panicking() {
        let html = "<p><b>Noun</b></p><ol><li>Terms relating to animals.</li><ol style=\"list-style-type:lower-alpha\"><li>(<i>countable</i>) A mammal.</li></ol></ol>";
        let text = html_to_display_text(html);
        assert!(text.contains("Terms relating to animals."));
        assert!(text.contains("A mammal."));
    }

    // Not a #[test] itself — needs a real GtkTextBuffer, so it runs from
    // the crate's single shared GTK-backed entry point (see the note on
    // `node_index.rs`'s `check_marks_stay_anchored_...`).
    pub(crate) fn check_context_around() {
        let text = "When the harvest was plenteous and his master was kind or careless.";
        let buffer = TextBuffer::new(None);
        let mut iter = buffer.end_iter();
        buffer.insert(&mut iter, text);

        let word_start = text.find("plenteous").unwrap() as i32;
        let word_end = word_start + "plenteous".len() as i32;
        let (before, after) = context_around(&buffer, word_start, word_end);
        assert_eq!(before, "When the harvest was");
        assert_eq!(after, "and his master was kind or careless.");

        // A radius that runs past the buffer's start/end must clamp, not
        // panic or return garbage from an out-of-range iterator.
        let (before, after) = context_around(&buffer, 0, 4);
        assert_eq!(before, "");
        assert!(after.starts_with("the harvest"));
    }
}
