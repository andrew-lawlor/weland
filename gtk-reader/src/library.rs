//! Library "home" screen: a searchable card grid built from Phase 1's
//! `library.json`, plus EPUB/folder import. Opening a card launches the
//! Phase 2-4 reader (`app::build_ui`) in its own window — this app has no
//! single-window navigation stack, each book just gets its own window, the
//! same way e.g. a file manager opens a new window per location.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use gtk4::{
    self as gtk, gdk_pixbuf, gio, glib, prelude::*, Align, Application, ApplicationWindow, Box as GtkBox, Button,
    Entry, FileDialog, FileFilter, FlowBox, Label, Orientation, Picture, PolicyType, ProgressBar, ScrolledWindow,
    SelectionMode,
};
use rusqlite::{Connection, OpenFlags};

use crate::{app, persistence, persistence::LibraryEntry};

const COVER_WIDTH: i32 = 130;
const COVER_HEIGHT: i32 = 190;

type CoverCache = Rc<RefCell<HashMap<String, gdk_pixbuf::Pixbuf>>>;

/// One book whose cover hasn't been decoded (or wasn't in the cache) yet.
struct PendingCover {
    path: String,
    picture: Picture,
}

pub fn build_library_window(app: &Application) -> Result<()> {
    let config_dir = Rc::new(persistence::config_dir()?);
    let data_dir = persistence::data_dir()?;
    let books_dir = Rc::new(data_dir.join("books"));

    let entries: Rc<RefCell<Vec<LibraryEntry>>> = Rc::new(RefCell::new(persistence::read_library(&config_dir)?));
    let cover_cache: CoverCache = Rc::new(RefCell::new(HashMap::new()));
    let query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

    let flowbox = FlowBox::new();
    flowbox.set_selection_mode(SelectionMode::None);
    flowbox.set_homogeneous(true);
    flowbox.set_row_spacing(16);
    flowbox.set_column_spacing(16);
    flowbox.set_min_children_per_line(1);
    flowbox.set_max_children_per_line(8);
    flowbox.set_margin_top(16);
    flowbox.set_margin_bottom(16);
    flowbox.set_margin_start(16);
    flowbox.set_margin_end(16);

    let search_entry = Entry::builder().placeholder_text("Search title or author\u{2026}").hexpand(true).build();

    let import_book_btn = Button::with_label("Import Book\u{2026}");
    let import_folder_btn = Button::with_label("Import Folder\u{2026}");
    let status_label = Label::new(None);
    status_label.set_visible(false);

    let toolbar = GtkBox::new(Orientation::Horizontal, 8);
    toolbar.set_margin_top(12);
    toolbar.set_margin_bottom(4);
    toolbar.set_margin_start(16);
    toolbar.set_margin_end(16);
    toolbar.append(&search_entry);
    toolbar.append(&status_label);
    toolbar.append(&import_book_btn);
    toolbar.append(&import_folder_btn);

    let scroller = ScrolledWindow::builder().child(&flowbox).hscrollbar_policy(PolicyType::Never).vexpand(true).build();

    let root = GtkBox::new(Orientation::Vertical, 0);
    root.append(&toolbar);
    root.append(&scroller);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Weland Library")
        .default_width(920)
        .default_height(720)
        .child(&root)
        .build();

    rebuild_flowbox(&flowbox, &entries.borrow(), &query.borrow(), &cover_cache, app);

    {
        let flowbox = flowbox.clone();
        let entries = entries.clone();
        let cover_cache = cover_cache.clone();
        let query = query.clone();
        let app = app.clone();
        search_entry.connect_changed(move |entry| {
            *query.borrow_mut() = entry.text().to_lowercase();
            rebuild_flowbox(&flowbox, &entries.borrow(), &query.borrow(), &cover_cache, &app);
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
        let app = app.clone();
        let status_label = status_label.clone();
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
            let app = app.clone();
            let status_label = status_label.clone();
            dialog.open(Some(&window), gio::Cancellable::NONE, move |result| {
                let Ok(file) = result else { return };
                let Some(path) = file.path() else { return };
                spawn_import_one(
                    config_dir, books_dir, path, flowbox, entries, cover_cache, query, app, status_label,
                );
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
        let app = app.clone();
        let status_label = status_label.clone();
        import_folder_btn.connect_clicked(move |_| {
            let dialog = FileDialog::builder().title("Import Folder of EPUBs").accept_label("Import").build();

            let config_dir = config_dir.clone();
            let books_dir = books_dir.clone();
            let flowbox = flowbox.clone();
            let entries = entries.clone();
            let cover_cache = cover_cache.clone();
            let query = query.clone();
            let app = app.clone();
            let status_label = status_label.clone();
            dialog.select_folder(Some(&window), gio::Cancellable::NONE, move |result| {
                let Ok(file) = result else { return };
                let Some(path) = file.path() else { return };
                spawn_import_folder(
                    config_dir, books_dir, path, flowbox, entries, cover_cache, query, app, status_label,
                );
            });
        });
    }

    window.present();
    Ok(())
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

fn rebuild_flowbox(flowbox: &FlowBox, entries: &[LibraryEntry], query: &str, cover_cache: &CoverCache, app: &Application) {
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
        title.add_css_class("heading");

        let author = Label::new(entry.author.as_deref());
        author.set_ellipsize(gtk::pango::EllipsizeMode::End);
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
        let app = app.clone();
        button.connect_clicked(move |_| {
            if let Err(e) = app::build_ui(&app, &path) {
                eprintln!("error opening {path}: {e}");
            }
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
    loader.pixbuf()
}

/// Result of a background import, marshaled back to the main thread through
/// a plain `mpsc` channel polled on a short local timeout — `glib`'s old
/// cross-thread `MainContext::channel` is gone in this glib version, and
/// nothing here touches a GTK object off the main thread, so a bare
/// `std::thread::spawn` + poll is simpler than pulling in an async runtime.
enum ImportMsg {
    Done,
    Failed(String),
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
    app: Application,
    status_label: Label,
) {
    let (tx, rx) = mpsc::channel::<ImportMsg>();
    status_label.set_text("Importing\u{2026}");
    status_label.set_visible(true);

    let config_dir_owned = (*config_dir).clone();
    let books_dir_owned = (*books_dir).clone();
    std::thread::spawn(move || {
        let _ = std::fs::create_dir_all(&books_dir_owned);
        let outcome = import_one(&config_dir_owned, &books_dir_owned, &input);
        let _ = tx.send(match outcome {
            Ok(()) => ImportMsg::Done,
            Err(e) => ImportMsg::Failed(format!("{}: {e}", input.display())),
        });
    });

    poll_import(rx, move |msg| {
        status_label.set_visible(false);
        if let ImportMsg::Failed(err) = &msg {
            eprintln!("import failed: {err}");
        }
        if let Ok(fresh) = persistence::read_library(&config_dir) {
            *entries.borrow_mut() = fresh;
        }
        rebuild_flowbox(&flowbox, &entries.borrow(), &query.borrow(), &cover_cache, &app);
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
    app: Application,
    status_label: Label,
) {
    let (tx, rx) = mpsc::channel::<ImportMsg>();
    status_label.set_text("Importing folder\u{2026}");
    status_label.set_visible(true);

    let config_dir_owned = (*config_dir).clone();
    let books_dir_owned = (*books_dir).clone();
    std::thread::spawn(move || {
        let _ = std::fs::create_dir_all(&books_dir_owned);
        let epubs = find_epubs_recursive(&root);
        let mut failures = Vec::new();
        for input in epubs {
            if let Err(e) = import_one(&config_dir_owned, &books_dir_owned, &input) {
                failures.push(format!("{}: {e}", input.display()));
            }
        }
        let _ = tx.send(if failures.is_empty() {
            ImportMsg::Done
        } else {
            ImportMsg::Failed(failures.join("; "))
        });
    });

    poll_import(rx, move |msg| {
        status_label.set_visible(false);
        if let ImportMsg::Failed(err) = &msg {
            eprintln!("folder import had failures: {err}");
        }
        if let Ok(fresh) = persistence::read_library(&config_dir) {
            *entries.borrow_mut() = fresh;
        }
        rebuild_flowbox(&flowbox, &entries.borrow(), &query.borrow(), &cover_cache, &app);
    });
}

fn poll_import(rx: mpsc::Receiver<ImportMsg>, on_done: impl Fn(ImportMsg) + 'static) {
    glib::timeout_add_local(Duration::from_millis(150), move || match rx.try_recv() {
        Ok(msg) => {
            on_done(msg);
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
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
