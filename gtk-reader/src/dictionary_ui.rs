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

use std::sync::mpsc;
use std::time::Duration;

use gtk4::{gdk, glib, prelude::*, Align, Box as GtkBox, Button, Label, Orientation, Popover, TextView};

use crate::dictionary::{self, DictionaryEntry};

/// Looks up `word` locally and shows a popover at `rect` (widget
/// coordinates) with the results, offering an online fallback if the local
/// dictionary has nothing.
pub fn show_word_lookup(text_view: &TextView, rect: gdk::Rectangle, word: &str) {
    let entries = dictionary::lookup_word(word).unwrap_or_default();
    let popover = build_lookup_popover(text_view, &rect, word, entries, true);
    popover.popup();
}

/// Builds (but doesn't show) a definitions popover — a fresh `Popover` every
/// time, never a mutated live one (see `annotation_ui.rs`'s
/// `show_comment_composer` notes on why that broke there).
fn build_lookup_popover(text_view: &TextView, rect: &gdk::Rectangle, word: &str, entries: Vec<DictionaryEntry>, offer_online: bool) -> Popover {
    let popover = Popover::new();
    popover.set_parent(text_view);
    popover.set_pointing_to(Some(rect));
    popover.set_size_request(280, -1);

    let container = GtkBox::new(Orientation::Vertical, 6);
    let title = Label::new(Some(word));
    title.set_halign(Align::Start);
    title.add_css_class("heading");
    container.append(&title);

    if entries.is_empty() {
        let empty = Label::new(Some("No local definition found."));
        empty.set_halign(Align::Start);
        empty.add_css_class("dim-label");
        container.append(&empty);

        if offer_online {
            let online_btn = Button::with_label("Look up online");
            let text_view_c = text_view.clone();
            let rect_c = rect.clone();
            let popover_c = popover.clone();
            let word_c = word.to_string();
            online_btn.connect_clicked(move |_| {
                popover_c.popdown();
                spawn_online_lookup(&text_view_c, rect_c.clone(), &word_c);
            });
            container.append(&online_btn);
        }
    } else {
        for entry in entries.iter().take(5) {
            let def = Label::new(Some(&entry.definition));
            def.set_wrap(true);
            def.set_halign(Align::Start);
            container.append(&def);
        }
    }

    popover.set_child(Some(&container));
    popover
}

/// Runs the online lookup on a background thread (blocking HTTP call — this
/// crate has no async runtime) and polls for the result, matching the
/// std::thread + mpsc + glib::timeout_add_local pattern already used for
/// library import.
fn spawn_online_lookup(text_view: &TextView, rect: gdk::Rectangle, word: &str) {
    let (tx, rx) = mpsc::channel();
    let word_for_thread = word.to_string();
    std::thread::spawn(move || {
        let result = dictionary::lookup_word_online(&word_for_thread);
        let _ = tx.send(result);
    });

    let text_view = text_view.clone();
    let word = word.to_string();
    glib::timeout_add_local(Duration::from_millis(150), move || match rx.try_recv() {
        Ok(result) => {
            let entries = result.unwrap_or_default();
            let popover = build_lookup_popover(&text_view, &rect, &word, entries, false);
            popover.popup();
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
    });
}
