//! Builds the reader page: opens a `.wld`, builds the reading pane, and
//! wires reading-position restore/tracking on top of it. Returns a widget
//! (not a window) for `main.rs` to swap into the shared `ApplicationWindow`'s
//! `gtk::Stack` (Phase 10 single-window navigation).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use gtk4::{self as gtk, gdk_pixbuf, glib, prelude::*, Paned, ScrolledWindow, TextView};
use libadwaita::prelude::*;
use rusqlite::{Connection, OpenFlags};
use weland::db;

use crate::{
    annotation_ui, annotation_ui::AnnotationState, annotations::AnnotationIndex, document, document::PendingImage, keybindings,
    keybindings::Action, node_index::NodeIndex, persistence, search_ui, settings_ui, toc, vocab_ui,
};

/// Builds the reader page's root widget for `path` and returns it alongside
/// the window title it implies, and the sidebar widget itself so `main.rs`
/// can wire a header-bar collapse toggle to it. Back-to-library navigation
/// lives in the shared `AdwHeaderBar` (`main.rs`'s `Nav`), not in this page —
/// a back button is a header-bar-level concept in GNOME/Adwaita apps, not a
/// peer of the Contents/Annotations/Search tabs — but the `BackToLibrary`
/// keyboard shortcut still needs to reach it, hence `on_back`.
pub fn build_reader_page(path: &str, on_back: Rc<dyn Fn()>) -> Result<(Paned, String, gtk::Box)> {
    // Read-write: annotations get created/edited/deleted interactively.
    // Image decode gets its own separate read-only connection below, since
    // this one is moved into `AnnotationState` for the lifetime of the window.
    let conn = Connection::open(path).with_context(|| format!("failed to open {path}"))?;
    let metadata = weland::db::load_metadata(&conn)?;
    let nodes = weland::db::load_ast_nodes(&conn)?;
    let toc_entries = weland::db::load_toc(&conn)?;
    let annotation_index = AnnotationIndex::load(&conn)?;

    let title_text = metadata.get("title").cloned().unwrap_or_else(|| "Untitled".into());
    let author_text = metadata.get("author").cloned();
    let language_text = metadata.get("language").cloned();
    let content_hash_text = metadata.get("source_epub_sha256").cloned();
    let window_title = format!("{title_text} — {}", author_text.clone().unwrap_or_default());

    let text_view = TextView::new();
    text_view.set_editable(false);
    text_view.set_cursor_visible(false);
    text_view.set_wrap_mode(gtk::WrapMode::Word);
    text_view.set_left_margin(48);
    text_view.set_right_margin(48);
    text_view.set_top_margin(24);
    text_view.set_bottom_margin(24);
    text_view.set_pixels_above_lines(2);

    let buffer = text_view.buffer();
    let tags = document::build_tags(&buffer);
    let mut index = NodeIndex::new();
    let mut pending_images = Vec::new();
    document::build_document(&text_view, &buffer, &nodes, &tags, &mut index, &mut pending_images);
    let index = Rc::new(index);
    let tags = Rc::new(tags);

    let base_font = settings_ui::install_base_font_tag(&buffer);
    let reading_settings = persistence::config_dir().ok().map(|d| persistence::read_settings(&d)).unwrap_or_default();
    settings_ui::apply_settings(&base_font, &tags, &reading_settings);

    let annotation_tags = annotation_ui::build_annotation_tags(&buffer);
    annotation_ui::render_existing(&buffer, &annotation_tags, &nodes, &index, &annotation_index);
    let annotation_state =
        AnnotationState::new(conn, annotation_tags, nodes, index.clone(), text_view.clone(), annotation_index, title_text.clone());
    annotation_ui::wire_annotation_interactions(&text_view, &buffer, annotation_state.clone());

    let scroller = ScrolledWindow::builder().child(&text_view).hscrollbar_policy(gtk::PolicyType::Never).build();

    let jump_text_view = text_view.clone();
    let jump_index = index.clone();
    let toc_sidebar = toc::build_toc(&toc_entries, move |node_id| {
        if let Some(mark) = jump_index.mark_for_node(node_id) {
            jump_text_view.scroll_to_mark(mark, 0.0, true, 0.0, 0.0);
        }
    });
    let annotations_sidebar = annotation_ui::build_annotation_list_panel(&annotation_state);

    // Read-only: search never mutates the book, so it gets its own
    // connection rather than sharing the read-write one AnnotationState owns.
    let search_conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| format!("failed to open {path}"))?;
    let search_sidebar = search_ui::build_search_panel(search_conn, annotation_state.clone());
    let vocab_sidebar = vocab_ui::build_vocab_panel(&annotation_state.vocab);

    let sidebar_stack = gtk::Stack::new();
    // The stack is now nested inside a plain vertical Box (for the toggle
    // row above it) instead of being handed straight to Paned as before —
    // Paned always gives its two children the full available height itself,
    // but a plain Box only gives each child its natural size unless told to
    // expand, so without this the whole sidebar collapsed down to whatever
    // its shortest content needed.
    sidebar_stack.set_vexpand(true);
    sidebar_stack.add_named(&toc_sidebar, Some("toc"));
    sidebar_stack.add_named(&annotations_sidebar, Some("annotations"));
    sidebar_stack.add_named(&search_sidebar, Some("search"));
    sidebar_stack.add_named(&vocab_sidebar, Some("vocab"));

    // Icon-only + tooltip, not text labels -- five text buttons ("Contents"
    // / "Annotations" / "Search" / "Vocab" / "Settings") is exactly the same
    // toolbar-width problem the library page's utility row had, just worse
    // here since the sidebar itself is only ~220px wide to begin with.
    // Reuses the same icon names as their library-page counterparts
    // (`library.rs`'s Vocab/Settings buttons) where the same concept
    // applies, for one consistent icon vocabulary across the app.
    let toc_toggle = gtk::Button::from_icon_name("view-list-symbolic");
    toc_toggle.set_tooltip_text(Some("Contents"));
    let annotations_toggle = gtk::Button::from_icon_name("edit-symbolic");
    annotations_toggle.set_tooltip_text(Some("Annotations"));
    let search_toggle = gtk::Button::from_icon_name("edit-find-symbolic");
    search_toggle.set_tooltip_text(Some("Search"));
    let vocab_toggle = gtk::Button::from_icon_name("accessories-dictionary-symbolic");
    vocab_toggle.set_tooltip_text(Some("Vocabulary"));
    let settings_toggle = gtk::Button::from_icon_name("preferences-system-symbolic");
    settings_toggle.set_tooltip_text(Some("Settings"));
    toc_toggle.add_css_class("suggested-action");

    // `.linked` (a GNOME HIG / Adwaita convention) draws these as one
    // joined segmented control instead of separate floating buttons.
    let toggle_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    toggle_row.add_css_class("linked");
    toggle_row.set_margin_top(8);
    toggle_row.set_margin_start(8);
    toggle_row.set_margin_end(8);
    toggle_row.append(&toc_toggle);
    toggle_row.append(&annotations_toggle);
    toggle_row.append(&search_toggle);
    toggle_row.append(&vocab_toggle);
    toggle_row.append(&settings_toggle);

    wire_sidebar_toggle(&toc_toggle, &sidebar_stack, "toc", &[annotations_toggle.clone(), search_toggle.clone(), vocab_toggle.clone()]);
    wire_sidebar_toggle(&annotations_toggle, &sidebar_stack, "annotations", &[toc_toggle.clone(), search_toggle.clone(), vocab_toggle.clone()]);
    wire_sidebar_toggle(&search_toggle, &sidebar_stack, "search", &[toc_toggle.clone(), annotations_toggle.clone(), vocab_toggle.clone()]);
    wire_sidebar_toggle(&vocab_toggle, &sidebar_stack, "vocab", &[toc_toggle.clone(), annotations_toggle.clone(), search_toggle.clone()]);
    {
        let base_font = base_font.clone();
        let tags = tags.clone();
        let settings_toggle_c = settings_toggle.clone();
        settings_toggle.connect_clicked(move |_| {
            let dialog = settings_ui::build_settings_dialog(base_font.clone(), tags.clone(), None);
            dialog.present(Some(&settings_toggle_c));
        });
    }

    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 4);
    sidebar.append(&toggle_row);
    sidebar.append(&sidebar_stack);

    let paned = Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&sidebar)
        .end_child(&scroller)
        .resize_start_child(false)
        .shrink_start_child(false)
        .position(220)
        .build();

    // Read any saved position *before* upserting the library entry (which
    // only bumps last_opened_at/title/author, never the position fields) —
    // same order as the Tauri app's open_book.
    let config_dir = persistence::config_dir().ok();
    let saved_node_id = config_dir
        .as_ref()
        .and_then(|dir| persistence::read_library(dir).ok())
        .and_then(|entries| entries.into_iter().find(|e| e.path == path))
        .and_then(|e| e.last_position_node_id);

    if let Some(dir) = &config_dir {
        let _ = persistence::upsert_library_entry(
            dir,
            path,
            &title_text,
            author_text.as_deref(),
            language_text.as_deref(),
            content_hash_text.as_deref(),
        );
    }

    if let Some(node_id) = saved_node_id {
        if let Some(mark) = index.mark_for_node(node_id) {
            let text_view = text_view.clone();
            let mark = mark.clone();
            // The page hasn't been laid out yet at the moment it's swapped
            // into the stack's visible child — scrolling immediately lands
            // at the wrong position (the same trap hit an image-scroll
            // debug affordance during the rendering spike). A short
            // timeout, not an idle callback, reliably waits long enough for
            // layout to settle.
            glib::timeout_add_local_once(Duration::from_millis(150), move || {
                text_view.scroll_to_mark(&mark, 0.0, true, 0.0, 0.0);
            });
        }
    }

    wire_reading_position_tracking(&scroller, &text_view, &buffer, &index, path);

    let recenter_pictures: Vec<gtk::Picture> = pending_images.iter().map(|p| p.picture.clone()).collect();
    document::wire_image_centering(&text_view, recenter_pictures);

    // A second, read-only connection — the read-write one above is already
    // owned by `AnnotationState` for the page's lifetime.
    let image_conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| format!("failed to open {path}"))?;
    spawn_lazy_image_decode(image_conn, pending_images);

    wire_keyboard_shortcuts(&paned, &scroller, &text_view, &buffer, &index, &toc_entries, &sidebar, on_back);

    Ok((paned, window_title, sidebar))
}

/// Wires the remappable reading-pane shortcuts (`keybindings.rs`) to this
/// page's own widgets. Attached to `paned` (an ancestor of both the text
/// view and every sidebar panel) with `Capture` phase so it sees every
/// keypress before `TextView`'s own built-in arrow/Page-key scrolling can
/// consume it — the only exception is a keypress while a text-entry widget
/// (the search box, the annotation note composer — both plain `gtk::Entry`s)
/// has focus, which is let straight through untouched so typing still works.
fn wire_keyboard_shortcuts(
    paned: &Paned,
    scroller: &ScrolledWindow,
    text_view: &TextView,
    buffer: &gtk::TextBuffer,
    index: &Rc<NodeIndex>,
    toc_entries: &[weland::schema::TocEntry],
    sidebar: &gtk::Box,
    on_back: Rc<dyn Fn()>,
) {
    let config_dir = persistence::config_dir().ok();
    let bindings = config_dir.as_ref().map(|d| keybindings::load(d)).unwrap_or_else(keybindings::defaults);

    // Buffer offsets for every TOC entry that actually jumps somewhere, in
    // document order — computed once up front rather than per-keypress,
    // same "buffer positions never move after the initial build" reasoning
    // as `node_index.rs`'s own `boundaries` cache.
    let mut chapter_offsets: Vec<i32> = toc_entries
        .iter()
        .filter_map(|e| e.target_node_id)
        .filter_map(|node_id| index.mark_for_node(node_id))
        .map(|mark| buffer.iter_at_mark(mark).offset())
        .collect();
    chapter_offsets.sort_unstable();

    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);

    let text_view_c = text_view.clone();
    let buffer_c = buffer.clone();
    let scroller_c = scroller.clone();
    let index_c = index.clone();
    let sidebar_c = sidebar.clone();
    controller.connect_key_pressed(move |_, keyval, _keycode, state| {
        let focused_is_entry = text_view_c.root().and_then(|root| root.focus()).map(|w| w.is::<gtk::Entry>()).unwrap_or(false);
        if focused_is_entry {
            return glib::Propagation::Proceed;
        }

        let Some(action) = keybindings::action_for_key(&bindings, keyval, state) else {
            return glib::Propagation::Proceed;
        };

        match action {
            Action::ScrollDown => scroll_by(&scroller_c, 80.0),
            Action::ScrollUp => scroll_by(&scroller_c, -80.0),
            Action::PageDown => page_by(&scroller_c, 1.0),
            Action::PageUp => page_by(&scroller_c, -1.0),
            Action::NextChapter => jump_chapter(&text_view_c, &buffer_c, &index_c, &chapter_offsets, 1),
            Action::PrevChapter => jump_chapter(&text_view_c, &buffer_c, &index_c, &chapter_offsets, -1),
            Action::ToggleSidebar => sidebar_c.set_visible(!sidebar_c.is_visible()),
            Action::BackToLibrary => on_back(),
        }
        glib::Propagation::Stop
    });
    paned.add_controller(controller);
}

fn scroll_by(scroller: &ScrolledWindow, delta: f64) {
    let adj = scroller.vadjustment();
    let max = (adj.upper() - adj.page_size()).max(adj.lower());
    adj.set_value((adj.value() + delta).clamp(adj.lower(), max));
}

fn page_by(scroller: &ScrolledWindow, direction: f64) {
    // 90% of a page, not a full page, so the last line of the previous page
    // stays on screen as a continuity anchor -- the same reason paginated
    // e-readers commonly overlap slightly rather than cutting exactly at the
    // page boundary.
    let page_size = scroller.vadjustment().page_size();
    scroll_by(scroller, direction * page_size * 0.9);
}

fn jump_chapter(text_view: &TextView, buffer: &gtk::TextBuffer, index: &NodeIndex, chapter_offsets: &[i32], direction: i32) {
    let Some(current_id) = index.topmost_visible_node_id(buffer, text_view) else { return };
    let Some(mark) = index.mark_for_node(current_id) else { return };
    let current_offset = buffer.iter_at_mark(mark).offset();

    let target_offset = if direction > 0 {
        chapter_offsets.iter().copied().find(|o| *o > current_offset)
    } else {
        chapter_offsets.iter().rev().copied().find(|o| *o < current_offset)
    };

    if let Some(offset) = target_offset {
        let mut iter = buffer.iter_at_offset(offset);
        text_view.scroll_to_iter(&mut iter, 0.0, true, 0.0, 0.0);
    }
}

/// Decodes each pending image's bytes a small batch at a time on `glib`
/// idle callbacks instead of all up front — the 125-image cookbook in the
/// spike's test set took several seconds to decode synchronously, during
/// which the window couldn't even paint. `conn` is moved in wholesale since
/// nothing else in `build_ui` needs it once this is set up.
fn spawn_lazy_image_decode(conn: Connection, pending_images: Vec<PendingImage>) {
    const BATCH_PER_TICK: usize = 3;
    let mut queue: VecDeque<PendingImage> = pending_images.into();

    glib::idle_add_local(move || {
        for _ in 0..BATCH_PER_TICK {
            let Some(PendingImage { asset_id, picture }) = queue.pop_front() else {
                return glib::ControlFlow::Break;
            };
            decode_and_apply(&conn, asset_id, &picture);
        }
        if queue.is_empty() {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

fn decode_and_apply(conn: &Connection, asset_id: i64, picture: &gtk::Picture) {
    let Ok((_, data)) = db::load_asset(conn, asset_id) else { return };

    let loader = gdk_pixbuf::PixbufLoader::new();
    if loader.write(&data).is_err() {
        return;
    }
    if loader.close().is_err() {
        return;
    }
    let Some(pixbuf) = loader.pixbuf() else { return };

    // Sizing (max height, clamped to the view's usable width so a wide
    // plate can't force horizontal scroll) is handled per-frame by
    // `document::wire_image_centering` instead of here, since it also has
    // to react to window resizes after this one-time decode.
    picture.set_pixbuf(Some(&pixbuf));
}

/// Wires one sidebar toggle button: clicking it shows `name`'s page in
/// `stack` and marks itself active, clearing the "active" styling off every
/// other toggle in `siblings`.
fn wire_sidebar_toggle(button: &gtk::Button, stack: &gtk::Stack, name: &'static str, siblings: &[gtk::Button]) {
    let stack = stack.clone();
    let button_self = button.clone();
    let siblings = siblings.to_vec();
    button.connect_clicked(move |_| {
        stack.set_visible_child_name(name);
        button_self.add_css_class("suggested-action");
        for sibling in &siblings {
            sibling.remove_css_class("suggested-action");
        }
    });
}

/// Debounced (600ms) save of the topmost visible node as reading position,
/// mirroring the web reader's scroll-tracking. `GtkTextView` shares its
/// scrollable adjustment with the enclosing `ScrolledWindow`.
fn wire_reading_position_tracking(
    scroller: &ScrolledWindow,
    text_view: &TextView,
    buffer: &gtk4::TextBuffer,
    index: &Rc<NodeIndex>,
    path: &str,
) {
    let debounce: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let index = index.clone();
    let text_view = text_view.clone();
    let buffer = buffer.clone();
    let path = path.to_string();

    scroller.vadjustment().connect_value_changed(move |_| {
        if let Some(id) = debounce.borrow_mut().take() {
            id.remove();
        }

        let index = index.clone();
        let text_view = text_view.clone();
        let buffer = buffer.clone();
        let path = path.clone();
        let debounce_inner = debounce.clone();

        let source_id = glib::timeout_add_local(Duration::from_millis(600), move || {
            if let Some(node_id) = index.topmost_visible_node_id(&buffer, &text_view) {
                if let (Some(dir), Some(percent)) = (persistence::config_dir().ok(), index.percent_through(&buffer, node_id)) {
                    let _ = persistence::update_reading_position(&dir, &path, node_id, percent);
                }
            }
            *debounce_inner.borrow_mut() = None;
            glib::ControlFlow::Break
        });
        *debounce.borrow_mut() = Some(source_id);
    });
}
