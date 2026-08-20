//! Vocab-builder sidebar panel: every word saved via the dictionary
//! popover's "Add to Vocab" button (see `dictionary_ui.rs`), across every
//! book — read from `persistence::VocabEntry`, app-level JSON rather than
//! per-book SQLite like annotations, since the whole point of a vocab list
//! is browsing it independent of which book you're currently in.

use std::rc::Rc;

use gtk4::{glib, prelude::*, Align, Box as GtkBox, Button, Label, Orientation, PolicyType, ScrolledWindow, Separator};

use crate::annotation_ui::AnnotationState;
use crate::persistence;

/// Builds the "Vocab" sidebar panel and registers its list container on
/// `state` so `refresh_vocab_list` (called after every add/remove) knows
/// what to rebuild.
pub fn build_vocab_panel(state: &Rc<AnnotationState>) -> ScrolledWindow {
    let list = GtkBox::new(Orientation::Vertical, 10);
    list.set_margin_top(8);
    list.set_margin_bottom(8);
    list.set_margin_start(8);
    list.set_margin_end(8);

    *state.vocab_list_container.borrow_mut() = Some(list.clone());
    refresh_vocab_list(state);

    ScrolledWindow::builder().child(&list).hscrollbar_policy(PolicyType::Never).width_request(220).build()
}

/// Rebuilds the vocab list from disk. Cheap enough to just do in full on
/// every add/remove (matches `annotation_ui.rs`'s own list panel) rather
/// than tracking incremental diffs — vocab lists are a handful to a few
/// hundred entries, not thousands.
pub(crate) fn refresh_vocab_list(state: &Rc<AnnotationState>) {
    let Some(list) = state.vocab_list_container.borrow().clone() else { return };
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }

    let Ok(config_dir) = persistence::config_dir() else { return };
    let mut entries = persistence::read_vocab(&config_dir).unwrap_or_default();
    if entries.is_empty() {
        let empty = Label::new(Some("No saved words yet. Double-click a word, then \u{201c}Add to Vocab\u{201d} in its definition popup."));
        empty.set_wrap(true);
        empty.set_halign(Align::Start);
        empty.add_css_class("dim-label");
        list.append(&empty);
        return;
    }

    // Newest first -- the word you just looked up is the one you want to
    // see landed at the top, not scroll to find.
    entries.sort_by(|a, b| b.added_at.cmp(&a.added_at).then(b.id.cmp(&a.id)));

    for entry in entries {
        let card = GtkBox::new(Orientation::Vertical, 2);

        let word_label = Label::new(Some(&entry.word));
        word_label.set_halign(Align::Start);
        word_label.add_css_class("heading");
        card.append(&word_label);

        let book_label = Label::new(Some(&entry.book_title));
        book_label.set_halign(Align::Start);
        book_label.add_css_class("dim-label");
        card.append(&book_label);

        if !entry.context_before.is_empty() || !entry.context_after.is_empty() {
            // Pango markup, not plain text -- safe here (unlike the raw
            // dictionary HTML in dictionary_ui.rs, which isn't) since this
            // is built entirely from our own stored strings, escaped before
            // embedding so any `<`/`>`/`&` actually in the book's text
            // (e.g. "Smith & Sons") can't break the markup.
            let context_text = format!(
                "\u{2026}{} <b>{}</b> {}\u{2026}",
                glib::markup_escape_text(&entry.context_before),
                glib::markup_escape_text(&entry.word),
                glib::markup_escape_text(&entry.context_after),
            );
            let context_label = Label::new(None);
            context_label.set_markup(&context_text);
            context_label.set_wrap(true);
            context_label.set_halign(Align::Start);
            context_label.set_justify(gtk4::Justification::Left);
            context_label.add_css_class("dim-label");
            card.append(&context_label);
        }

        let def_label = Label::new(Some(&entry.definition));
        def_label.set_wrap(true);
        def_label.set_halign(Align::Start);
        card.append(&def_label);

        let remove_btn = Button::with_label("Remove");
        remove_btn.set_halign(Align::Start);
        let state_c = state.clone();
        let id = entry.id;
        remove_btn.connect_clicked(move |_| {
            if let Ok(dir) = persistence::config_dir() {
                let _ = persistence::remove_vocab_entry(&dir, id);
            }
            refresh_vocab_list(&state_c);
        });
        card.append(&remove_btn);

        list.append(&card);
        list.append(&Separator::new(Orientation::Horizontal));
    }
}
