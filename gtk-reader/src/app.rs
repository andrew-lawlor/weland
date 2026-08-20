//! Window/app setup: opens a `.wld`, builds the reading pane, and wires
//! reading-position restore/tracking on top of it.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use gtk4::{self as gtk, gdk_pixbuf, glib, prelude::*, Application, ApplicationWindow, Paned, ScrolledWindow, TextView};
use rusqlite::{Connection, OpenFlags};
use weland::db;

use crate::{
    annotation_ui, annotation_ui::AnnotationState, annotations::AnnotationIndex, document, document::PendingImage,
    node_index::NodeIndex, persistence, search_ui, settings_ui, toc,
};

pub fn build_ui(app: &Application, path: &str) -> Result<()> {
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
    let annotation_state = AnnotationState::new(conn, annotation_tags, nodes, index.clone(), text_view.clone(), annotation_index);
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

    let toc_toggle = gtk::Button::with_label("Contents");
    let annotations_toggle = gtk::Button::with_label("Annotations");
    let search_toggle = gtk::Button::with_label("Search");
    let settings_toggle = gtk::Button::with_label("Aa");
    toc_toggle.add_css_class("suggested-action");

    let toggle_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    toggle_row.set_margin_top(8);
    toggle_row.set_margin_start(8);
    toggle_row.set_margin_end(8);
    toggle_row.append(&toc_toggle);
    toggle_row.append(&annotations_toggle);
    toggle_row.append(&search_toggle);
    toggle_row.append(&settings_toggle);

    wire_sidebar_toggle(&toc_toggle, &sidebar_stack, "toc", &[annotations_toggle.clone(), search_toggle.clone()]);
    wire_sidebar_toggle(&annotations_toggle, &sidebar_stack, "annotations", &[toc_toggle.clone(), search_toggle.clone()]);
    wire_sidebar_toggle(&search_toggle, &sidebar_stack, "search", &[toc_toggle.clone(), annotations_toggle.clone()]);
    {
        let base_font = base_font.clone();
        let tags = tags.clone();
        let settings_toggle_c = settings_toggle.clone();
        settings_toggle.connect_clicked(move |_| {
            let popover = settings_ui::build_settings_popover(&settings_toggle_c, base_font.clone(), tags.clone());
            popover.popup();
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

    let window = ApplicationWindow::builder()
        .application(app)
        .title(&window_title)
        .default_width(1080)
        .default_height(1000)
        .child(&paned)
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
        let _ = persistence::upsert_library_entry(dir, path, &title_text, author_text.as_deref());
    }

    window.present();

    if let Some(node_id) = saved_node_id {
        if let Some(mark) = index.mark_for_node(node_id) {
            let text_view = text_view.clone();
            let mark = mark.clone();
            // A freshly presented window hasn't finished layout on the first
            // main-loop tick — scrolling immediately lands at the wrong
            // position (the same trap hit an image-scroll debug affordance
            // during the rendering spike). A short timeout, not an idle
            // callback, reliably waits long enough for layout to settle.
            glib::timeout_add_local_once(Duration::from_millis(150), move || {
                text_view.scroll_to_mark(&mark, 0.0, true, 0.0, 0.0);
            });
        }
    }

    wire_reading_position_tracking(&scroller, &text_view, &buffer, &index, path);

    let recenter_pictures: Vec<gtk::Picture> = pending_images.iter().map(|p| p.picture.clone()).collect();
    document::wire_image_centering(&text_view, recenter_pictures);

    // A second, read-only connection — the read-write one above is already
    // owned by `AnnotationState` for the window's lifetime.
    let image_conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| format!("failed to open {path}"))?;
    spawn_lazy_image_decode(image_conn, pending_images);

    Ok(())
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

    let (pw, ph) = (pixbuf.width(), pixbuf.height());
    let target_h = 340i32.min(ph.max(1));
    let target_w = (pw as i64 * target_h as i64 / ph.max(1) as i64) as i32;

    picture.set_pixbuf(Some(&pixbuf));
    picture.set_size_request(target_w.max(1), target_h.max(1));
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
