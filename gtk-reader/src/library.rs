//! Library "home" screen: a searchable card grid built from Phase 1's
//! `library.json`, plus EPUB/folder import. This builds a page (not a
//! window) that `main.rs` swaps into a shared `ApplicationWindow`'s
//! `gtk::Stack` — single-window navigation (Phase 10), better suited to a
//! small/handheld display than the one-window-per-book behavior this
//! replaced. Opening a card calls back into `main.rs`'s `on_open` rather
//! than building a reader window directly.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use gtk4::{
    self as gtk, gdk_pixbuf, gio, glib, prelude::*, Align, Box as GtkBox, Button, FileDialog, FileFilter, FlowBox,
    Label, Orientation, Picture, PolicyType, ProgressBar, ScrolledWindow, SearchEntry, SelectionMode,
};
use libadwaita::ApplicationWindow;
use rusqlite::{Connection, OpenFlags};

use crate::{persistence, persistence::LibraryEntry};

const COVER_WIDTH: i32 = 110;
const COVER_HEIGHT: i32 = 160;

type CoverCache = Rc<RefCell<HashMap<String, gdk_pixbuf::Pixbuf>>>;

/// One book whose cover hasn't been decoded (or wasn't in the cache) yet.
struct PendingCover {
    path: String,
    picture: Picture,
}

/// Builds the library page's root widget and returns it alongside a
/// `refresh` closure the caller should invoke whenever this page becomes
/// visible again (e.g. returning from the reader) — reading position/
/// progress may have changed since it was last shown. `window` is the
/// shared top-level window, needed only to parent the import file dialogs.
/// `on_open` is called with a book's `.wld` path when its card is clicked.
pub fn build_library_page(window: &ApplicationWindow, on_open: Rc<dyn Fn(&str)>) -> Result<(GtkBox, Rc<dyn Fn()>)> {
    let config_dir = Rc::new(persistence::config_dir()?);
    let data_dir = persistence::data_dir()?;
    let books_dir = Rc::new(data_dir.join("books"));

    let entries: Rc<RefCell<Vec<LibraryEntry>>> = Rc::new(RefCell::new(persistence::read_library(&config_dir)?));
    let cover_cache: CoverCache = Rc::new(RefCell::new(HashMap::new()));
    let query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

    let flowbox = FlowBox::new();
    // Unlike a plain Box, GtkScrolledWindow doesn't stretch its child to
    // fill the viewport by default (its whole job is letting a child be
    // *smaller* than the window and scroll) -- without this, FlowBox only
    // ever saw its own minimal natural width and never reflowed into extra
    // columns no matter how wide the window got.
    flowbox.set_hexpand(true);
    flowbox.set_selection_mode(SelectionMode::None);
    flowbox.set_homogeneous(true);
    flowbox.set_row_spacing(16);
    flowbox.set_column_spacing(16);
    flowbox.set_min_children_per_line(1);
    // 8 was a hard cap regardless of window width -- on a wide/maximized
    // desktop window there was room for plenty more columns than that, but
    // the grid just left the extra width sitting empty instead of using it.
    // FlowBox only ever fills a row up to however many *actually* fit at
    // the card's natural width, so this is a ceiling for very wide windows,
    // not a target it forces on smaller ones.
    flowbox.set_max_children_per_line(20);
    flowbox.set_margin_top(16);
    flowbox.set_margin_bottom(16);
    flowbox.set_margin_start(16);
    flowbox.set_margin_end(16);

    let search_entry = SearchEntry::builder().placeholder_text("Search title or author\u{2026}").hexpand(true).build();

    let import_book_btn = Button::with_label("Import Book\u{2026}");
    let import_folder_btn = Button::with_label("Import Folder\u{2026}");
    let import_row = GtkBox::new(Orientation::Horizontal, 0);
    import_row.add_css_class("linked");
    import_row.append(&import_book_btn);
    import_row.append(&import_folder_btn);

    let status_spinner = gtk::Spinner::new();
    status_spinner.set_visible(false);
    let status_label = Label::new(None);
    status_label.set_visible(false);
    // Built once here (before the empty-state view it's also placed into
    // below) so `ImportUi` can toggle its visibility on import start/finish.
    let empty_gif = crate::branding::forge_anvil_picture();
    let import_ui = ImportUi {
        spinner: status_spinner.clone(),
        label: status_label.clone(),
        book_btn: import_book_btn.clone(),
        folder_btn: import_folder_btn.clone(),
        empty_gif: empty_gif.clone(),
    };

    let toolbar = GtkBox::new(Orientation::Horizontal, 8);
    toolbar.set_margin_top(12);
    toolbar.set_margin_bottom(4);
    toolbar.set_margin_start(16);
    toolbar.set_margin_end(16);
    toolbar.append(&search_entry);
    toolbar.append(&status_spinner);
    toolbar.append(&status_label);
    toolbar.append(&import_row);

    let scroller =
        ScrolledWindow::builder().child(&flowbox).hscrollbar_policy(PolicyType::Never).hexpand(true).vexpand(true).build();

    // Shown instead of the (otherwise blank) grid when the library has zero
    // books — the same forge-anvil branding the old Tauri reader used here,
    // carried over on request.
    let empty_state = GtkBox::new(Orientation::Vertical, 12);
    empty_state.set_valign(Align::Center);
    empty_state.set_halign(Align::Center);
    empty_state.set_vexpand(true);
    // Only shown while an import is actually running (`ImportUi::start`/
    // `finish` toggle it) — an empty library that *isn't* loading anything
    // is just empty, not "in progress," so the animation would be
    // misleading sitting there with nothing happening.
    if let Some(gif) = &empty_gif {
        // The source GIF is a fairly small 200x199 — rendered at that native
        // size on a HiDPI display it both looks oversized next to the
        // one-line label next to it *and* pixelated (upscaled by the
        // display's own scale factor with no filtering to hide it).
        // Constraining it to a smaller box and letting `Picture` downscale
        // into it renders cleanly instead.
        gif.set_can_shrink(true);
        gif.set_content_fit(gtk::ContentFit::Contain);
        gif.set_size_request(96, 96);
        gif.set_visible(false);
        empty_state.append(gif);
    }
    let empty_label = Label::new(Some("Your library is empty \u{2014} import an EPUB to start reading."));
    empty_label.add_css_class("dim-label");
    empty_state.append(&empty_label);

    let content_stack = gtk::Stack::new();
    content_stack.add_named(&scroller, Some("grid"));
    content_stack.add_named(&empty_state, Some("empty"));

    let root = GtkBox::new(Orientation::Vertical, 0);
    root.append(&toolbar);
    root.append(&content_stack);

    rebuild_flowbox(&flowbox, &entries.borrow(), &query.borrow(), &cover_cache, &on_open, &content_stack);

    {
        let flowbox = flowbox.clone();
        let entries = entries.clone();
        let cover_cache = cover_cache.clone();
        let query = query.clone();
        let on_open = on_open.clone();
        let content_stack = content_stack.clone();
        search_entry.connect_changed(move |entry| {
            *query.borrow_mut() = entry.text().to_lowercase();
            rebuild_flowbox(&flowbox, &entries.borrow(), &query.borrow(), &cover_cache, &on_open, &content_stack);
        });
    }

    {
        let window = window.clone();
        let config_dir = config_dir.clone();
        let books_dir = books_dir.clone();
        let flowbox = flowbox.clone();
        let entries = entries.clone();
        let cover_cache = cover_cache.clone();
        let query = query.clone();
        let on_open = on_open.clone();
        let import_ui = import_ui.clone();
        let content_stack = content_stack.clone();
        import_book_btn.connect_clicked(move |_| {
            let filter = FileFilter::new();
            filter.set_name(Some("EPUB books"));
            filter.add_suffix("epub");
            let filters = gio::ListStore::new::<FileFilter>();
            filters.append(&filter);

            let dialog = FileDialog::builder().title("Import EPUB").accept_label("Import").build();
            dialog.set_filters(Some(&filters));

            let config_dir = config_dir.clone();
            let books_dir = books_dir.clone();
            let flowbox = flowbox.clone();
            let entries = entries.clone();
            let cover_cache = cover_cache.clone();
            let query = query.clone();
            let on_open = on_open.clone();
            let import_ui = import_ui.clone();
            let content_stack = content_stack.clone();
            dialog.open(Some(&window), gio::Cancellable::NONE, move |result| {
                let Ok(file) = result else { return };
                let Some(path) = file.path() else { return };
                spawn_import_one(config_dir, books_dir, path, flowbox, entries, cover_cache, query, on_open, import_ui, content_stack);
            });
        });
    }

    {
        let window = window.clone();
        let config_dir = config_dir.clone();
        let books_dir = books_dir.clone();
        let flowbox = flowbox.clone();
        let entries = entries.clone();
        let cover_cache = cover_cache.clone();
        let query = query.clone();
        let on_open = on_open.clone();
        let import_ui = import_ui.clone();
        let content_stack = content_stack.clone();
        import_folder_btn.connect_clicked(move |_| {
            let dialog = FileDialog::builder().title("Import Folder of EPUBs").accept_label("Import").build();

            let config_dir = config_dir.clone();
            let books_dir = books_dir.clone();
            let flowbox = flowbox.clone();
            let entries = entries.clone();
            let cover_cache = cover_cache.clone();
            let query = query.clone();
            let on_open = on_open.clone();
            let import_ui = import_ui.clone();
            let content_stack = content_stack.clone();
            dialog.select_folder(Some(&window), gio::Cancellable::NONE, move |result| {
                let Ok(file) = result else { return };
                let Some(path) = file.path() else { return };
                spawn_import_folder(config_dir, books_dir, path, flowbox, entries, cover_cache, query, on_open, import_ui, content_stack);
            });
        });
    }

    let refresh: Rc<dyn Fn()> = {
        let flowbox = flowbox.clone();
        let entries = entries.clone();
        let cover_cache = cover_cache.clone();
        let query = query.clone();
        let on_open = on_open.clone();
        let config_dir = config_dir.clone();
        let content_stack = content_stack.clone();
        Rc::new(move || {
            if let Ok(fresh) = persistence::read_library(&config_dir) {
                *entries.borrow_mut() = fresh;
            }
            rebuild_flowbox(&flowbox, &entries.borrow(), &query.borrow(), &cover_cache, &on_open, &content_stack);
        })
    };

    Ok((root, refresh))
}

fn filtered<'a>(entries: &'a [LibraryEntry], query: &str) -> Vec<&'a LibraryEntry> {
    if query.is_empty() {
        return entries.iter().collect();
    }
    entries
        .iter()
        .filter(|e| {
            e.title.to_lowercase().contains(query)
                || e.author.as_deref().map(|a| a.to_lowercase().contains(query)).unwrap_or(false)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn rebuild_flowbox(
    flowbox: &FlowBox,
    entries: &[LibraryEntry],
    query: &str,
    cover_cache: &CoverCache,
    on_open: &Rc<dyn Fn(&str)>,
    content_stack: &gtk::Stack,
) {
    content_stack.set_visible_child_name(if entries.is_empty() { "empty" } else { "grid" });

    while let Some(child) = flowbox.first_child() {
        flowbox.remove(&child);
    }

    let mut pending = Vec::new();

    for entry in filtered(entries, query) {
        let picture = Picture::new();
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_size_request(COVER_WIDTH, COVER_HEIGHT);
        picture.set_halign(Align::Center);

        if let Some(pixbuf) = cover_cache.borrow().get(&entry.path) {
            picture.set_pixbuf(Some(pixbuf));
        } else {
            pending.push(PendingCover { path: entry.path.clone(), picture: picture.clone() });
        }

        let title = Label::new(Some(&entry.title));
        title.set_wrap(true);
        title.set_lines(2);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.set_justify(gtk::Justification::Center);
        // `max-width-chars` alone turned out not to be enough either — a
        // live measurement (`Widget::measure`) showed one card still
        // reporting a *natural* width of ~1490px, wider than the whole
        // window, and homogeneous FlowBox sizes every column to the widest
        // natural request. The actual missing piece: GTK4 Labels default to
        // `NaturalWrapMode::None`, meaning the natural-size request is still
        // computed from the *unwrapped* one-line text regardless of `wrap`/
        // `max-width-chars` — wrapping only ever kicked in once something
        // *else* forced a smaller allocation. `Word` makes the natural
        // request itself reflect the wrapped size.
        title.set_natural_wrap_mode(gtk::NaturalWrapMode::Word);
        title.set_max_width_chars(14);
        title.add_css_class("heading");

        let author = Label::new(entry.author.as_deref());
        author.set_ellipsize(gtk::pango::EllipsizeMode::End);
        author.set_max_width_chars(14);
        author.add_css_class("dim-label");

        let card = GtkBox::new(Orientation::Vertical, 4);
        card.set_width_request(COVER_WIDTH + 20);
        card.append(&picture);
        card.append(&title);
        card.append(&author);

        if let Some(percent) = entry.last_position_percent {
            let progress = ProgressBar::new();
            progress.set_fraction(percent);
            progress.set_margin_top(2);
            card.append(&progress);
        }

        let button = Button::builder().child(&card).has_frame(false).build();
        let path = entry.path.clone();
        let on_open = on_open.clone();
        button.connect_clicked(move |_| {
            on_open(&path);
        });

        flowbox.append(&button);
    }

    if !pending.is_empty() {
        spawn_lazy_cover_decode(pending, cover_cache.clone());
    }
}

/// Decodes covers a couple at a time on `glib` idle callbacks, same pattern
/// as the reading pane's lazy image decode (Phase 3) — opening a fresh
/// read-only connection per book plus a `PixbufLoader` decode for every
/// visible card up front would stall the library window's first paint on a
/// large library.
fn spawn_lazy_cover_decode(pending: Vec<PendingCover>, cover_cache: CoverCache) {
    const BATCH_PER_TICK: usize = 2;
    let mut queue: std::collections::VecDeque<PendingCover> = pending.into();

    glib::idle_add_local(move || {
        for _ in 0..BATCH_PER_TICK {
            let Some(PendingCover { path, picture }) = queue.pop_front() else {
                return glib::ControlFlow::Break;
            };
            if let Some(pixbuf) = decode_cover(&path) {
                picture.set_pixbuf(Some(&pixbuf));
                cover_cache.borrow_mut().insert(path, pixbuf);
            }
        }
        if queue.is_empty() {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

fn decode_cover(path: &str) -> Option<gdk_pixbuf::Pixbuf> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let metadata = weland::db::load_metadata(&conn).ok()?;
    let cover_id: i64 = metadata.get("cover_asset_id")?.parse().ok()?;
    let (_, data) = weland::db::load_asset(&conn, cover_id).ok()?;

    let loader = gdk_pixbuf::PixbufLoader::new();
    loader.write(&data).ok()?;
    loader.close().ok()?;
    let pixbuf = loader.pixbuf()?;

    // The real bug behind "grid won't go past one column, no matter the
    // window width or any FlowBox/label setting": `Picture`'s *natural*
    // size request reflects the pixbuf's own source resolution, not
    // `set_size_request`'s (minimum-only) COVER_WIDTH/HEIGHT — and source
    // covers vary wildly, some 1000px+ on a side. In a homogeneous FlowBox
    // that means the single widest cover in the whole library forces every
    // column to match it, collapsing the grid to ~1 column — confirmed via
    // a live `Widget::measure()` dump showing natural widths of 491-1492px
    // that tracked cover size, not the card's text. Scaling down to
    // roughly the card's own display size (2x, for a crisp render on
    // HiDPI — `Picture` still displays it at COVER_WIDTH/HEIGHT logical
    // size) fixes the natural-size request at the source.
    let (w, h) = (pixbuf.width(), pixbuf.height());
    let target_h = (COVER_HEIGHT * 2).min(h.max(1));
    let target_w = (w as i64 * target_h as i64 / h.max(1) as i64) as i32;
    pixbuf.scale_simple(target_w.max(1), target_h.max(1), gdk_pixbuf::InterpType::Bilinear)
}

/// Result of a background import, marshaled back to the main thread through
/// a plain `mpsc` channel polled on a short local timeout — `glib`'s old
/// cross-thread `MainContext::channel` is gone in this glib version, and
/// nothing here touches a GTK object off the main thread, so a bare
/// `std::thread::spawn` + poll is simpler than pulling in an async runtime.
/// `Done`/`Failed` always carry a human-readable summary ("Imported 5
/// books", "No EPUBs found in that folder") — a background import that
/// finishes with no visible result otherwise reads as "nothing happened,"
/// which is exactly the confusion this is meant to head off. `Progress` is
/// folder-import-only (`spawn_import_one` never sends it, since a single
/// file has no meaningful partial state): each book is compiled and
/// registered into `library.json` one at a time (see `import_one`'s own
/// `upsert_library_entry` call), so refreshing the grid after *each* one
/// lands, instead of only once the whole folder is done, is what actually
/// makes a big import look alive rather than stalled for however long the
/// rest of the folder takes.
enum ImportMsg {
    Progress { done: usize, total: usize },
    Done(String),
    Failed(String),
}

/// Shows `text` in `status_label`, then hides it again after a few seconds
/// — long enough to actually read, short enough not to linger as clutter.
fn show_status_then_fade(status_label: &Label, text: &str) {
    status_label.set_text(text);
    status_label.set_visible(true);
    let status_label = status_label.clone();
    glib::timeout_add_local_once(Duration::from_secs(4), move || {
        status_label.set_visible(false);
    });
}

/// The toolbar widgets an in-progress import needs to touch: a spinner (the
/// only *animated* sign anything is happening — a static "Importing…" label
/// alone reads as stalled/dead for a long folder import, especially with no
/// per-file progress) and both import buttons, disabled for the duration so
/// a second import can't be started concurrently with the first.
#[derive(Clone)]
struct ImportUi {
    spinner: gtk::Spinner,
    label: Label,
    book_btn: Button,
    folder_btn: Button,
    // Only actually visible when the empty-state view is also showing (that
    // view only shows when the library has zero books) — see the "empty AND
    // loading" comment on `empty_gif`'s construction.
    empty_gif: Option<Picture>,
}

impl ImportUi {
    fn start(&self, text: &str) {
        self.spinner.start();
        self.spinner.set_visible(true);
        self.label.set_text(text);
        self.label.set_visible(true);
        self.book_btn.set_sensitive(false);
        self.folder_btn.set_sensitive(false);
        if let Some(gif) = &self.empty_gif {
            gif.set_visible(true);
        }
    }

    fn finish(&self, summary: &str) {
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.book_btn.set_sensitive(true);
        self.folder_btn.set_sensitive(true);
        if let Some(gif) = &self.empty_gif {
            gif.set_visible(false);
        }
        show_status_then_fade(&self.label, summary);
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_import_one(
    config_dir: Rc<PathBuf>,
    books_dir: Rc<PathBuf>,
    input: PathBuf,
    flowbox: FlowBox,
    entries: Rc<RefCell<Vec<LibraryEntry>>>,
    cover_cache: CoverCache,
    query: Rc<RefCell<String>>,
    on_open: Rc<dyn Fn(&str)>,
    import_ui: ImportUi,
    content_stack: gtk::Stack,
) {
    let (tx, rx) = mpsc::channel::<ImportMsg>();
    import_ui.start("Importing\u{2026} (you can keep browsing or open a book meanwhile)");

    let config_dir_owned = (*config_dir).clone();
    let books_dir_owned = (*books_dir).clone();
    let input_display = input.display().to_string();
    std::thread::spawn(move || {
        let _ = std::fs::create_dir_all(&books_dir_owned);
        let outcome = import_one(&config_dir_owned, &books_dir_owned, &input);
        let _ = tx.send(match outcome {
            Ok(()) => ImportMsg::Done(format!("Imported {input_display}")),
            Err(e) => ImportMsg::Failed(format!("{input_display}: {e}")),
        });
    });

    poll_import(rx, |_, _| {}, move |msg| {
        let text = match &msg {
            ImportMsg::Progress { .. } => unreachable!("spawn_import_one never sends Progress"),
            ImportMsg::Done(summary) => summary.clone(),
            ImportMsg::Failed(err) => {
                eprintln!("import failed: {err}");
                format!("Import failed: {err}")
            }
        };
        import_ui.finish(&text);
        if let Ok(fresh) = persistence::read_library(&config_dir) {
            *entries.borrow_mut() = fresh;
        }
        rebuild_flowbox(&flowbox, &entries.borrow(), &query.borrow(), &cover_cache, &on_open, &content_stack);
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_import_folder(
    config_dir: Rc<PathBuf>,
    books_dir: Rc<PathBuf>,
    root: PathBuf,
    flowbox: FlowBox,
    entries: Rc<RefCell<Vec<LibraryEntry>>>,
    cover_cache: CoverCache,
    query: Rc<RefCell<String>>,
    on_open: Rc<dyn Fn(&str)>,
    import_ui: ImportUi,
    content_stack: gtk::Stack,
) {
    let (tx, rx) = mpsc::channel::<ImportMsg>();
    import_ui.start("Importing folder\u{2026} (you can keep browsing or open a book meanwhile)");

    let config_dir_owned = (*config_dir).clone();
    let books_dir_owned = (*books_dir).clone();
    std::thread::spawn(move || {
        let _ = std::fs::create_dir_all(&books_dir_owned);
        let epubs = find_epubs_recursive(&root);
        let total = epubs.len();
        let mut ok_count = 0;
        let mut failures = Vec::new();
        for (done, input) in epubs.into_iter().enumerate() {
            match import_one(&config_dir_owned, &books_dir_owned, &input) {
                Ok(()) => ok_count += 1,
                Err(e) => {
                    eprintln!("[import-folder] failed: {}: {e:#}", input.display());
                    failures.push(format!("{}: {e}", input.display()));
                }
            }
            // `import_one` already wrote this book into library.json before
            // returning (success or not doesn't change that count, so this
            // fires either way) — sending progress now, not just at the very
            // end, is what lets the UI show it in the grid immediately
            // instead of only after the whole folder finishes.
            let _ = tx.send(ImportMsg::Progress { done: done + 1, total });
        }
        let summary = match (ok_count, failures.len()) {
            (0, 0) => "No EPUBs found in that folder".to_string(),
            (n, 0) => format!("Imported {n} book{}", if n == 1 { "" } else { "s" }),
            (0, f) => format!("{f} import{} failed", if f == 1 { "" } else { "s" }),
            (n, f) => format!("Imported {n}, {f} failed"),
        };
        let _ = tx.send(if failures.is_empty() { ImportMsg::Done(summary) } else { ImportMsg::Failed(summary) });
    });

    poll_import(
        rx,
        {
            let flowbox = flowbox.clone();
            let entries = entries.clone();
            let cover_cache = cover_cache.clone();
            let query = query.clone();
            let on_open = on_open.clone();
            let config_dir = config_dir.clone();
            let content_stack = content_stack.clone();
            let import_ui = import_ui.clone();
            move |done, total| {
                import_ui.label.set_text(&format!(
                    "Importing folder\u{2026} {done}/{total} (you can keep browsing or open a book meanwhile)"
                ));
                if let Ok(fresh) = persistence::read_library(&config_dir) {
                    *entries.borrow_mut() = fresh;
                }
                rebuild_flowbox(&flowbox, &entries.borrow(), &query.borrow(), &cover_cache, &on_open, &content_stack);
            }
        },
        move |msg| {
            let text = match &msg {
                ImportMsg::Progress { .. } => unreachable!("handled by on_progress, not on_done"),
                ImportMsg::Done(summary) => summary.clone(),
                ImportMsg::Failed(summary) => {
                    eprintln!("folder import had failures: {summary}");
                    summary.clone()
                }
            };
            import_ui.finish(&text);
            if let Ok(fresh) = persistence::read_library(&config_dir) {
                *entries.borrow_mut() = fresh;
            }
            rebuild_flowbox(&flowbox, &entries.borrow(), &query.borrow(), &cover_cache, &on_open, &content_stack);
        },
    );
}

/// Polls `rx` until a terminal (`Done`/`Failed`) message arrives, calling
/// `on_progress` for every `Progress` message along the way without
/// stopping — `spawn_import_one` never sends `Progress`, so `on_done` is the
/// only thing that ever fires for it. Drains every message currently queued
/// each tick (not just one), so a burst of several `Progress` messages
/// landing between two 150ms polls (fast compiles, already-cached books)
/// doesn't get throttled down to one flowbox refresh per tick.
fn poll_import(rx: mpsc::Receiver<ImportMsg>, on_progress: impl Fn(usize, usize) + 'static, on_done: impl Fn(ImportMsg) + 'static) {
    glib::timeout_add_local(Duration::from_millis(150), move || loop {
        match rx.try_recv() {
            Ok(ImportMsg::Progress { done, total }) => on_progress(done, total),
            Ok(msg) => {
                on_done(msg);
                return glib::ControlFlow::Break;
            }
            Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => return glib::ControlFlow::Break,
        }
    });
}

/// Compiles (if not already compiled, keyed by the source EPUB's hashed
/// canonical path) and registers one EPUB into the library. Runs entirely
/// off the main thread — no GTK objects touched.
fn import_one(config_dir: &std::path::Path, books_dir: &std::path::Path, input: &std::path::Path) -> Result<()> {
    let output = persistence::sandboxed_wld_output_path(books_dir, input)?;

    if !output.exists() {
        let options = weland::compiler::CompileOptions { quiet: true, verbose: false };
        weland::compiler::compile_epub(input, &output, &options)?;
    }

    let conn = Connection::open_with_flags(&output, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let metadata = weland::db::load_metadata(&conn)?;
    let title = metadata.get("title").cloned().unwrap_or_else(|| {
        input.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Untitled".to_string())
    });
    persistence::upsert_library_entry(config_dir, &output.to_string_lossy(), &title, metadata.get("author").map(|s| s.as_str()))?;
    Ok(())
}

fn find_epubs_recursive(root: &std::path::Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read_dir) = std::fs::read_dir(&dir) else { continue };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let hidden = path.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with('.')).unwrap_or(false);
                if !hidden {
                    stack.push(path);
                }
            } else if path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("epub")).unwrap_or(false) {
                found.push(path);
            }
        }
    }
    found
}
