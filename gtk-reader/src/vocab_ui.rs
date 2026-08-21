//! Vocab-builder UI: every word saved via the dictionary popover's "Add to
//! Vocab" button (see `dictionary_ui.rs`), across every book — read from
//! `persistence::VocabEntry`, app-level JSON rather than per-book SQLite
//! like annotations, since the whole point of a vocab list is browsing it
//! independent of which book you're currently in.
//!
//! Two surfaces share the same list-rendering code (`build_vocab_panel`/
//! `refresh_vocab_list`): the reader page's sidebar panel (registered on the
//! open book's `AnnotationState` via `VocabListHandle`, since the dictionary
//! popover's "Add to Vocab" button needs to trigger a refresh from
//! `dictionary_ui.rs`) and the library page's standalone dialog
//! (`build_vocab_window`, a `VocabListHandle` of its own — vocab data isn't
//! book-scoped, so this view needs no book/`AnnotationState` open at all),
//! which additionally offers Markdown/JSON export of the whole list.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::{
    self as gtk, gio, glib, prelude::*, Align, Box as GtkBox, Button, Label, Orientation, PolicyType, ScrolledWindow, SearchEntry,
    Separator,
};
use libadwaita::{self as adw, prelude::*};

use crate::persistence::{self, VocabEntry};

/// The list container + current search filter a vocab list view needs —
/// split out from `AnnotationState` (which the library-page standalone
/// dialog has no reason to construct: no book, no SQLite connection, no
/// `NodeIndex`) so both call sites can share the same rendering code.
pub struct VocabListHandle {
    container: RefCell<Option<GtkBox>>,
    filter: RefCell<String>,
}

impl VocabListHandle {
    pub fn new() -> Rc<Self> {
        Rc::new(Self { container: RefCell::new(None), filter: RefCell::new(String::new()) })
    }
}

/// Builds a "Vocab" panel (a search box plus the word list) and registers
/// its list container on `vocab` so `refresh_vocab_list` (called after every
/// add/remove) knows what to rebuild.
pub fn build_vocab_panel(vocab: &Rc<VocabListHandle>) -> GtkBox {
    let search_entry = SearchEntry::builder().placeholder_text("Search vocab\u{2026}").build();
    search_entry.set_margin_top(8);
    search_entry.set_margin_start(8);
    search_entry.set_margin_end(8);

    let list = GtkBox::new(Orientation::Vertical, 10);
    list.set_margin_top(8);
    list.set_margin_bottom(8);
    list.set_margin_start(8);
    list.set_margin_end(8);

    *vocab.container.borrow_mut() = Some(list.clone());
    refresh_vocab_list(vocab);

    {
        let vocab = vocab.clone();
        search_entry.connect_changed(move |entry| {
            *vocab.filter.borrow_mut() = entry.text().to_lowercase();
            refresh_vocab_list(&vocab);
        });
    }

    let scroller = ScrolledWindow::builder().child(&list).hscrollbar_policy(PolicyType::Never).vexpand(true).build();

    let panel = GtkBox::new(Orientation::Vertical, 0);
    panel.set_width_request(220);
    panel.append(&search_entry);
    panel.append(&scroller);
    panel
}

/// Builds a standalone "Vocabulary" window for the library page, where no
/// book (and so no `AnnotationState`) is open — the same searchable list as
/// the reader's sidebar panel, plus Markdown/JSON export of the full,
/// unfiltered list.
pub fn build_vocab_window(parent: &impl IsA<gtk::Widget>) -> adw::Dialog {
    let vocab = VocabListHandle::new();
    let panel = build_vocab_panel(&vocab);
    panel.set_width_request(360);

    let export_md_btn = Button::with_label("Export as Markdown\u{2026}");
    let export_json_btn = Button::with_label("Export as JSON\u{2026}");
    let export_popover = gtk::Popover::new();
    let export_menu = GtkBox::new(Orientation::Vertical, 4);
    export_menu.set_margin_top(8);
    export_menu.set_margin_bottom(8);
    export_menu.set_margin_start(8);
    export_menu.set_margin_end(8);
    export_menu.append(&export_md_btn);
    export_menu.append(&export_json_btn);
    export_popover.set_child(Some(&export_menu));

    let export_btn = gtk::MenuButton::builder().label("Export").popover(&export_popover).build();

    let header = adw::HeaderBar::new();
    header.pack_end(&export_btn);
    header.set_title_widget(Some(&Label::new(Some("Vocabulary"))));

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&panel));

    let dialog = adw::Dialog::new();
    dialog.set_presentation_mode(adw::DialogPresentationMode::Floating);
    dialog.set_content_width(420);
    dialog.set_content_height(600);
    dialog.set_child(Some(&toolbar_view));

    let root = parent.clone().upcast::<gtk::Widget>();
    {
        let root = root.clone();
        let export_popover = export_popover.clone();
        export_md_btn.connect_clicked(move |_| {
            export_popover.popdown();
            run_export(&root, "vocabulary.md", "Markdown files", "md", export_markdown);
        });
    }
    {
        let export_popover = export_popover.clone();
        export_json_btn.connect_clicked(move |_| {
            export_popover.popdown();
            run_export(&root, "vocabulary.json", "JSON files", "json", export_json);
        });
    }

    dialog.present(Some(parent));
    dialog
}

fn run_export(parent: &gtk::Widget, initial_name: &str, filter_label: &str, suffix: &str, render: impl Fn(&[VocabEntry]) -> String + 'static) {
    let Ok(config_dir) = persistence::config_dir() else { return };
    let entries = persistence::read_vocab(&config_dir).unwrap_or_default();

    let filter = gtk::FileFilter::new();
    filter.set_name(Some(filter_label));
    filter.add_suffix(suffix);
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);

    let dialog = gtk::FileDialog::builder().title("Export Vocabulary").accept_label("Export").initial_name(initial_name).build();
    dialog.set_filters(Some(&filters));

    let root = parent.clone().downcast::<gtk::Window>().ok().or_else(|| parent.root().and_then(|r| r.downcast::<gtk::Window>().ok()));
    dialog.save(root.as_ref(), gio::Cancellable::NONE, move |result| {
        let Ok(file) = result else { return };
        let Some(path) = file.path() else { return };
        let _ = std::fs::write(&path, render(&entries));
    });
}

/// Alphabetical by word (case-insensitive) rather than the in-app newest-
/// first order -- an export is a reference document meant to be read/looked
/// up later, not a log of what was just looked at.
fn export_markdown(entries: &[VocabEntry]) -> String {
    let mut sorted: Vec<&VocabEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.word.to_lowercase().cmp(&b.word.to_lowercase()));

    let mut out = String::from("# Vocabulary\n\n");
    for entry in sorted {
        out.push_str(&format!("## {}\n\n", entry.word));
        out.push_str(&format!("*From:* {}\n\n", entry.book_title));
        if !entry.context_before.is_empty() || !entry.context_after.is_empty() {
            out.push_str(&format!("> \u{2026}{} **{}** {}\u{2026}\n\n", entry.context_before, entry.word, entry.context_after));
        }
        out.push_str(&entry.definition);
        out.push_str("\n\n---\n\n");
    }
    out
}

fn export_json(entries: &[VocabEntry]) -> String {
    serde_json::to_string_pretty(entries).unwrap_or_default()
}

/// Rebuilds the vocab list from disk. Cheap enough to just do in full on
/// every add/remove (matches `annotation_ui.rs`'s own list panel) rather
/// than tracking incremental diffs — vocab lists are a handful to a few
/// hundred entries, not thousands.
pub(crate) fn refresh_vocab_list(vocab: &Rc<VocabListHandle>) {
    let Some(list) = vocab.container.borrow().clone() else { return };
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }

    let Ok(config_dir) = persistence::config_dir() else { return };
    let mut entries = persistence::read_vocab(&config_dir).unwrap_or_default();

    let filter = vocab.filter.borrow();
    if !filter.is_empty() {
        entries.retain(|e| {
            e.word.to_lowercase().contains(filter.as_str())
                || e.book_title.to_lowercase().contains(filter.as_str())
                || e.definition.to_lowercase().contains(filter.as_str())
        });
    }

    if entries.is_empty() {
        let message = if filter.is_empty() {
            "No saved words yet. Double-click a word, then \u{201c}Add to Vocab\u{201d} in its definition popup."
        } else {
            "No saved words match your search."
        };
        let empty = Label::new(Some(message));
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
        let vocab_c = vocab.clone();
        let id = entry.id;
        remove_btn.connect_clicked(move |_| {
            if let Ok(dir) = persistence::config_dir() {
                let _ = persistence::remove_vocab_entry(&dir, id);
            }
            refresh_vocab_list(&vocab_c);
        });
        card.append(&remove_btn);

        list.append(&card);
        list.append(&Separator::new(Orientation::Horizontal));
    }
}
