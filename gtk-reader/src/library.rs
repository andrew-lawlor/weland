//! Library "home" screen: a searchable card grid built from Phase 1's
//! `library.json`, plus EPUB/folder import. This builds a page (not a
//! window) that `main.rs` swaps into a shared `ApplicationWindow`'s
//! `gtk::Stack` — single-window navigation (Phase 10), better suited to a
//! small/handheld display than the one-window-per-book behavior this
//! replaced. Opening a card calls back into `main.rs`'s `on_open` rather
//! than building a reader window directly.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use gtk4::{
    self as gtk, gdk_pixbuf, gio, glib, prelude::*, Align, Box as GtkBox, Button, DropDown, FileDialog, FileFilter,
    FlowBox, Label, Orientation, Picture, PolicyType, ProgressBar, ScrolledWindow, SearchEntry, SelectionMode,
    Separator, StringList, StringObject, ToggleButton,
};
use libadwaita::{self as adw, prelude::*, ApplicationWindow};
use rusqlite::{Connection, OpenFlags};

use crate::{document, persistence, persistence::LibraryEntry, settings_ui, sharing, vocab_ui};

const COVER_WIDTH: i32 = 110;
const COVER_HEIGHT: i32 = 160;
// A book counts as "finished" once its saved position is at or past this
// fraction -- rarely exactly 1.0 in practice (trailing matter, rounding),
// same reasoning e-readers generally use for a "done" threshold.
const FINISHED_THRESHOLD: f64 = 0.97;

type CoverCache = Rc<RefCell<HashMap<String, gdk_pixbuf::Pixbuf>>>;

/// One book whose cover hasn't been decoded (or wasn't in the cache) yet.
struct PendingCover {
    path: String,
    picture: Picture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadingStatus {
    Unread,
    InProgress,
    Finished,
}

fn reading_status(entry: &LibraryEntry) -> ReadingStatus {
    match entry.last_position_percent {
        None => ReadingStatus::Unread,
        Some(p) if p >= FINISHED_THRESHOLD => ReadingStatus::Finished,
        Some(p) if p > 0.0 => ReadingStatus::InProgress,
        Some(_) => ReadingStatus::Unread,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    All,
    Unread,
    InProgress,
    Finished,
}

impl StatusFilter {
    const ALL: [StatusFilter; 4] = [StatusFilter::All, StatusFilter::Unread, StatusFilter::InProgress, StatusFilter::Finished];

    fn label(self) -> &'static str {
        match self {
            StatusFilter::All => "All",
            StatusFilter::Unread => "Unread",
            StatusFilter::InProgress => "In Progress",
            StatusFilter::Finished => "Finished",
        }
    }

    fn matches(self, entry: &LibraryEntry) -> bool {
        match self {
            StatusFilter::All => true,
            StatusFilter::Unread => reading_status(entry) == ReadingStatus::Unread,
            StatusFilter::InProgress => reading_status(entry) == ReadingStatus::InProgress,
            StatusFilter::Finished => reading_status(entry) == ReadingStatus::Finished,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    RecentlyOpened,
    RecentlyAdded,
    TitleAsc,
    TitleDesc,
    AuthorAsc,
    AuthorDesc,
    Progress,
}

impl SortMode {
    const ALL: [SortMode; 7] = [
        SortMode::RecentlyOpened,
        SortMode::RecentlyAdded,
        SortMode::TitleAsc,
        SortMode::TitleDesc,
        SortMode::AuthorAsc,
        SortMode::AuthorDesc,
        SortMode::Progress,
    ];

    fn label(self) -> &'static str {
        match self {
            SortMode::RecentlyOpened => "Recently Opened",
            SortMode::RecentlyAdded => "Recently Added",
            SortMode::TitleAsc => "Title (A\u{2013}Z)",
            SortMode::TitleDesc => "Title (Z\u{2013}A)",
            SortMode::AuthorAsc => "Author (A\u{2013}Z)",
            SortMode::AuthorDesc => "Author (Z\u{2013}A)",
            SortMode::Progress => "Progress",
        }
    }
}

/// All of the library page's filter/sort state, bundled so most functions
/// take one `&FilterState` instead of four separate `Rc<RefCell<_>>`
/// parameters -- this grew from just `query` once status/sort/language
/// filtering joined it.
#[derive(Clone)]
struct FilterState {
    query: Rc<RefCell<String>>,
    sort: Rc<RefCell<SortMode>>,
    status: Rc<RefCell<StatusFilter>>,
    language: Rc<RefCell<Option<String>>>,
}

impl FilterState {
    fn new() -> Self {
        Self {
            query: Rc::new(RefCell::new(String::new())),
            sort: Rc::new(RefCell::new(SortMode::RecentlyOpened)),
            status: Rc::new(RefCell::new(StatusFilter::All)),
            language: Rc::new(RefCell::new(None)),
        }
    }
}

/// Reduces an author's full display name to a case-insensitive sort key
/// based on surname -- the last whitespace-separated word, which covers the
/// common case ("Steve Sando" -> "sando", "George R. R. Martin" -> "martin")
/// without attempting anything more sophisticated (suffixes like "Jr.",
/// multi-word surnames, "Last, First" input) that library sorting this
/// basic doesn't need to get exactly right.
fn author_sort_key(author: &str) -> String {
    author.split_whitespace().last().unwrap_or(author).to_lowercase()
}

/// Reduces a title to a case-insensitive sort key with a leading English
/// article dropped -- "The Hobbit" sorts under "hobbit" (with "H"), not
/// "the" (with every other "The ..." title in the library, which is what
/// straight string comparison would do), matching the standard library-
/// catalog convention. Only English articles; a title genuinely starting
/// with a word like "A" that isn't the article ("A" the letter grade, say)
/// is a false-positive edge case this doesn't try to distinguish.
fn title_sort_key(title: &str) -> String {
    let lower = title.to_lowercase();
    for article in ["the ", "an ", "a "] {
        if let Some(rest) = lower.strip_prefix(article) {
            return rest.to_string();
        }
    }
    lower
}

fn filtered_and_sorted<'a>(entries: &'a [LibraryEntry], filters: &FilterState) -> Vec<&'a LibraryEntry> {
    let query = filters.query.borrow();
    let status = *filters.status.borrow();
    let language = filters.language.borrow();
    let sort = *filters.sort.borrow();

    let mut result: Vec<&LibraryEntry> = entries
        .iter()
        .filter(|e| {
            (query.is_empty()
                || e.title.to_lowercase().contains(&*query)
                || e.author.as_deref().map(|a| a.to_lowercase().contains(&*query)).unwrap_or(false))
                && status.matches(e)
                && language.as_deref().map(|l| e.language.as_deref() == Some(l)).unwrap_or(true)
        })
        .collect();

    result.sort_by(|a, b| match sort {
        SortMode::TitleAsc => title_sort_key(&a.title).cmp(&title_sort_key(&b.title)),
        SortMode::TitleDesc => title_sort_key(&b.title).cmp(&title_sort_key(&a.title)),
        SortMode::AuthorAsc => author_sort_key(a.author.as_deref().unwrap_or("")).cmp(&author_sort_key(b.author.as_deref().unwrap_or(""))),
        SortMode::AuthorDesc => author_sort_key(b.author.as_deref().unwrap_or("")).cmp(&author_sort_key(a.author.as_deref().unwrap_or(""))),
        SortMode::RecentlyOpened => b.last_opened_at.cmp(&a.last_opened_at),
        SortMode::RecentlyAdded => b.added_at.cmp(&a.added_at),
        SortMode::Progress => {
            b.last_position_percent.unwrap_or(0.0).partial_cmp(&a.last_position_percent.unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal)
        }
    });
    result
}

/// A single "142 books \u{b7} 12 in progress \u{b7} ..." summary line. The
/// status/author/language breakdown is always over the *whole* library, not
/// the currently filtered subset -- an at-a-glance overview should describe
/// everything you have, not just what a filter happens to be showing right
/// now. `visible` (the search/filter/status result count) only changes the
/// leading segment's wording, to "Showing N of M books" when a filter is
/// actually narrowing anything down.
fn library_stats(entries: &[LibraryEntry], visible: usize) -> String {
    let total = entries.len();
    if total == 0 {
        return String::new();
    }

    let unread = entries.iter().filter(|e| reading_status(e) == ReadingStatus::Unread).count();
    let in_progress = entries.iter().filter(|e| reading_status(e) == ReadingStatus::InProgress).count();
    let finished = entries.iter().filter(|e| reading_status(e) == ReadingStatus::Finished).count();
    let authors: HashSet<&str> = entries.iter().filter_map(|e| e.author.as_deref()).collect();
    let languages: HashSet<&str> = entries.iter().filter_map(|e| e.language.as_deref()).collect();

    let mut parts = if visible == total {
        vec![format!("{total} book{}", if total == 1 { "" } else { "s" })]
    } else {
        vec![format!("Showing {visible} of {total} book{}", if total == 1 { "" } else { "s" })]
    };
    if in_progress > 0 {
        parts.push(format!("{in_progress} in progress"));
    }
    if finished > 0 {
        parts.push(format!("{finished} finished"));
    }
    parts.push(format!("{unread} unread"));
    parts.push(format!("{} author{}", authors.len(), if authors.len() == 1 { "" } else { "s" }));
    if languages.len() > 1 {
        parts.push(format!("{} languages", languages.len()));
    }
    parts.join(" \u{b7} ")
}

/// Rebuilds the language dropdown's options from whatever languages are
/// actually present in `entries` -- "All languages" is always index 0.
/// Re-selecting whatever `language` currently holds (or resetting it to
/// "All" if that language no longer appears in the library, e.g. its last
/// book was removed) means this is safe to call on every refresh without
/// the dropdown's selection silently drifting from the filter it's
/// supposed to represent.
/// `handler_id` is the language dropdown's own `selected-notify` handler
/// (filled in once it's connected -- see `build_library_page`) -- swapping
/// in a brand new `StringList` model below turns out to *always* fire
/// `selected-notify`, even when the resolved index ends up unchanged (a
/// model replacement isn't a simple property value change GObject can
/// diff against its old value the way it does for a plain setter). Left
/// unblocked, that fired the connected handler, which called `refresh_ui`,
/// which called back in here, forever -- confirmed live as an app freeze
/// the moment any *other* filter/sort control triggered a refresh after
/// the language dropdown's handler existed. Blocking it for the duration
/// of this resync is what actually breaks the loop.
fn rebuild_language_dropdown(
    dropdown: &DropDown,
    entries: &[LibraryEntry],
    language: &Rc<RefCell<Option<String>>>,
    handler_id: &Rc<RefCell<Option<glib::SignalHandlerId>>>,
) {
    let guard = handler_id.borrow();
    if let Some(id) = guard.as_ref() {
        dropdown.block_signal(id);
    }

    let mut langs: Vec<String> = entries.iter().filter_map(|e| e.language.clone()).collect::<HashSet<_>>().into_iter().collect();
    langs.sort();

    let mut labels: Vec<&str> = vec!["All languages"];
    labels.extend(langs.iter().map(|s| s.as_str()));
    dropdown.set_model(Some(&StringList::new(&labels)));

    let current = language.borrow().clone();
    let selected_index = current.as_deref().and_then(|cur| langs.iter().position(|l| l == cur)).map(|i| i as u32 + 1).unwrap_or(0);
    dropdown.set_selected(selected_index);
    if selected_index == 0 && current.is_some() {
        *language.borrow_mut() = None;
    }

    if let Some(id) = guard.as_ref() {
        dropdown.unblock_signal(id);
    }
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
    let filters = FilterState::new();

    let flowbox = FlowBox::new();
    // Unlike a plain Box, GtkScrolledWindow doesn't stretch its child to
    // fill the viewport by default (its whole job is letting a child be
    // *smaller* than the window and scroll) -- without this, FlowBox only
    // ever saw its own minimal natural width and never reflowed into extra
    // columns no matter how wide the window got.
    flowbox.set_hexpand(true);
    // Default `valign` is `Fill`, which (inside the `vexpand`ing scroller
    // below) stretches the flowbox to the *whole* viewport height whenever
    // its own content is shorter than that -- e.g. a single search result.
    // FlowBox then distributes that leftover space into the last row's own
    // cells rather than leaving it empty, so the hover/click highlight on a
    // lone card ends up covering the entire remaining page, not just the
    // card. `Start` caps it at its natural (content) height instead.
    flowbox.set_valign(Align::Start);
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

    // A single icon `MenuButton` + popover instead of two always-visible
    // text buttons -- less toolbar width for something reached far less
    // often than the search box or the status filters next to it. The two
    // real buttons and their click handlers are unchanged, just relocated
    // into `import_popover`'s child instead of a visible row.
    let import_book_btn = Button::with_label("Import Book\u{2026}");
    let import_folder_btn = Button::with_label("Import Folder\u{2026}");
    let import_menu = GtkBox::new(Orientation::Vertical, 4);
    import_menu.set_margin_top(8);
    import_menu.set_margin_bottom(8);
    import_menu.set_margin_start(8);
    import_menu.set_margin_end(8);
    import_menu.append(&import_book_btn);
    import_menu.append(&import_folder_btn);
    let import_popover = gtk::Popover::new();
    import_popover.set_child(Some(&import_menu));
    let import_menu_btn = gtk::MenuButton::builder().icon_name("list-add-symbolic").popover(&import_popover).build();
    import_menu_btn.set_tooltip_text(Some("Import a book or a folder of books"));

    // A headless buffer/tags/base-font, never attached to a visible
    // TextView -- `settings_ui::build_settings_dialog` needs *a* `TextTag`/
    // `Tags` to apply changes onto (that's how it live-previews inside an
    // open book), but the library page has no book open. Reusing it here
    // just gives the dialog somewhere harmless to write, so every change
    // still goes through the same real `persistence::write_settings`
    // read-modify-write path and takes effect the next time a book opens --
    // there's just no live preview to show from this page.
    let scratch_buffer = gtk4::TextBuffer::new(None);
    let scratch_tags = Rc::new(document::build_tags(&scratch_buffer));
    let scratch_base_font = settings_ui::install_base_font_tag(&scratch_buffer);

    // LAN sharing (see `sharing.rs`) is started once here, at startup, if
    // the user already had it on -- and can be started/stopped later from
    // the Settings dialog's toggle (`on_toggle` below), which is the only
    // other place that ever writes to this cell. `Option` (not just the
    // service itself) is the point: "off" is a real, cheap, common state.
    let share_service: Rc<RefCell<Option<Rc<sharing::ShareService>>>> = Rc::new(RefCell::new(None));
    {
        let startup_settings = persistence::read_settings(&config_dir);
        if startup_settings.lan_sharing_enabled.unwrap_or(false) {
            let device_name = startup_settings.device_name.unwrap_or_else(sharing_device_name);
            match sharing::ShareService::start((*config_dir).clone(), device_name) {
                Ok(service) => *share_service.borrow_mut() = Some(service),
                Err(e) => eprintln!("[sharing] failed to start: {e}"),
            }
        }
    }

    // Icon-only + tooltip, not `AdwButtonContent` icon+label -- these three
    // are occasional actions (unlike search/status filters, used on every
    // visit), and spelling each one out in text was most of what made the
    // toolbar feel crowded. The icons are all standard GNOME/Adwaita
    // symbolic names (dictionary, network, gear), recognizable on their own
    // and backed by a tooltip either way.
    let vocab_btn = Button::from_icon_name("accessories-dictionary-symbolic");
    vocab_btn.set_tooltip_text(Some("Vocabulary"));
    let share_btn = Button::from_icon_name("network-wireless-symbolic");
    share_btn.set_tooltip_text(Some("Share books over LAN"));
    let settings_btn = Button::from_icon_name("preferences-system-symbolic");
    settings_btn.set_tooltip_text(Some("Settings"));
    let utility_row = GtkBox::new(Orientation::Horizontal, 0);
    utility_row.add_css_class("linked");
    utility_row.append(&vocab_btn);
    utility_row.append(&share_btn);
    utility_row.append(&settings_btn);

    {
        let window = window.clone();
        vocab_btn.connect_clicked(move |_| {
            vocab_ui::build_vocab_window(&window);
        });
    }
    {
        let settings_btn_c = settings_btn.clone();
        let share_service = share_service.clone();
        let config_dir = config_dir.clone();
        settings_btn.connect_clicked(move |_| {
            let share_service = share_service.clone();
            let config_dir = config_dir.clone();
            let on_toggle: Rc<dyn Fn(bool)> = Rc::new(move |enabled| {
                if enabled {
                    if share_service.borrow().is_none() {
                        let settings = persistence::read_settings(&config_dir);
                        let device_name = settings.device_name.unwrap_or_else(sharing_device_name);
                        match sharing::ShareService::start((*config_dir).clone(), device_name) {
                            Ok(service) => *share_service.borrow_mut() = Some(service),
                            Err(e) => eprintln!("[sharing] failed to start: {e}"),
                        }
                    }
                } else if let Some(service) = share_service.borrow_mut().take() {
                    service.stop();
                }
            });
            let dialog = settings_ui::build_settings_dialog(scratch_base_font.clone(), scratch_tags.clone(), Some(on_toggle));
            dialog.present(Some(&settings_btn_c));
        });
    }

    let status_spinner = gtk::Spinner::new();
    status_spinner.set_visible(false);
    let status_label = Label::new(None);
    status_label.set_visible(false);
    // Unbounded by default, a long book title (or file path, in an error
    // message) in here would grow the toolbar's -- and so the whole
    // window's -- natural width to fit it, instead of just truncating in
    // place like every other title/author label in this file already does.
    status_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    status_label.set_max_width_chars(40);
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
    toolbar.append(&import_menu_btn);
    toolbar.append(&utility_row);

    let sort_labels: Vec<&str> = SortMode::ALL.iter().map(|s| s.label()).collect();
    let sort_dropdown = DropDown::from_strings(&sort_labels);
    sort_dropdown.set_tooltip_text(Some("Sort by"));

    let status_row = GtkBox::new(Orientation::Horizontal, 0);
    status_row.add_css_class("linked");
    let mut first_status_btn: Option<ToggleButton> = None;
    let mut status_buttons: Vec<(StatusFilter, ToggleButton)> = Vec::new();
    for status in StatusFilter::ALL {
        let btn = ToggleButton::builder().label(status.label()).build();
        match &first_status_btn {
            Some(first) => btn.set_group(Some(first)),
            None => first_status_btn = Some(btn.clone()),
        }
        if status == StatusFilter::All {
            btn.set_active(true);
        }
        status_row.append(&btn);
        status_buttons.push((status, btn));
    }

    // Populated for real by `refresh_ui`'s first run, once the language
    // dropdown has entries to build its options from -- see
    // `rebuild_language_dropdown`.
    let language_dropdown = DropDown::from_strings(&["All languages"]);
    language_dropdown.set_tooltip_text(Some("Filter by language"));

    let filter_row = GtkBox::new(Orientation::Horizontal, 8);
    filter_row.set_margin_start(16);
    filter_row.set_margin_end(16);
    filter_row.set_margin_bottom(4);
    filter_row.append(&sort_dropdown);
    filter_row.append(&status_row);
    filter_row.append(&language_dropdown);

    let stats_label = Label::new(None);
    stats_label.set_halign(Align::Start);
    stats_label.set_margin_start(16);
    stats_label.set_margin_end(16);
    stats_label.set_margin_bottom(8);
    stats_label.add_css_class("dim-label");

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
    root.append(&filter_row);
    root.append(&stats_label);
    root.append(&content_stack);

    // Everything downstream of "the entries list or a filter changed" goes
    // through this one closure -- re-derives the language dropdown's
    // options, re-renders the grid, and refreshes the stats line. Every
    // control below just updates its own bit of `filters`/`entries` and
    // calls this, instead of each one separately threading the same six
    // widgets through its own call to `rebuild_flowbox`.
    // Filled in once the language dropdown's own `selected-notify` handler
    // is connected below -- `rebuild_language_dropdown` needs it to block
    // that handler during its own programmatic model/selection resync (see
    // that function's doc comment for why that's not optional). `None`
    // during this closure's very first call (right below) is fine: no
    // handler exists yet to loop through at that point regardless.
    let language_handler_id: Rc<RefCell<Option<glib::SignalHandlerId>>> = Rc::new(RefCell::new(None));

    // Lets `rebuild_flowbox`'s per-card language-edit button trigger a full
    // `refresh_ui` after saving, without `refresh_ui` needing to pass
    // *itself* into a function it calls (which `Rc`'s ordinary construction
    // can't do directly) -- filled in right after `refresh_ui` exists,
    // before anything user-driven can reach the card that reads it.
    let refresh_ui_cell: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));

    // Everything downstream of "the entries list or a filter changed" goes
    // through this one closure -- re-derives the language dropdown's
    // options, re-renders the grid, and refreshes the stats line. Every
    // control below just updates its own bit of `filters`/`entries` and
    // calls this, instead of each one separately threading the same six
    // widgets through its own call to `rebuild_flowbox`.
    let refresh_ui: Rc<dyn Fn()> = {
        let flowbox = flowbox.clone();
        let entries = entries.clone();
        let filters = filters.clone();
        let cover_cache = cover_cache.clone();
        let on_open = on_open.clone();
        let content_stack = content_stack.clone();
        let search_entry = search_entry.clone();
        let language_dropdown = language_dropdown.clone();
        let language_handler_id = language_handler_id.clone();
        let stats_label = stats_label.clone();
        let config_dir = config_dir.clone();
        let refresh_ui_cell = refresh_ui_cell.clone();
        Rc::new(move || {
            rebuild_language_dropdown(&language_dropdown, &entries.borrow(), &filters.language, &language_handler_id);
            let visible = filtered_and_sorted(&entries.borrow(), &filters).len();
            rebuild_flowbox(
                &flowbox,
                &entries.borrow(),
                &entries,
                &filters,
                &cover_cache,
                &on_open,
                &content_stack,
                &search_entry,
                &config_dir,
                &refresh_ui_cell,
            );
            stats_label.set_text(&library_stats(&entries.borrow(), visible));
        })
    };
    *refresh_ui_cell.borrow_mut() = Some(refresh_ui.clone());
    refresh_ui();
    spawn_language_backfill(config_dir.clone(), entries.clone(), refresh_ui.clone());

    // `refresh_ui` only re-renders the flowbox from whatever's *already* in
    // the in-memory `entries` -- it never reloads `library.json`. Every
    // local-import completion handler below re-reads the file into
    // `entries` itself before calling `refresh_ui()` for exactly that
    // reason. A peer import (`sharing::import_from_peer`) writes straight to
    // `library.json` from a background thread with no access to this page's
    // in-memory `entries` at all, so it needs the same disk-reload step --
    // `refresh` (also returned at the bottom of this function, for
    // `main.rs`'s return-to-library refresh) does that reload-then-render
    // sequence and is what the LAN-sharing dialogs below use instead of the
    // raw `refresh_ui`. Built here (rather than staying in its original
    // spot further down) specifically so it exists before that wiring needs
    // it.
    let refresh: Rc<dyn Fn()> = {
        let entries = entries.clone();
        let config_dir = config_dir.clone();
        let refresh_ui = refresh_ui.clone();
        Rc::new(move || {
            if let Ok(fresh) = persistence::read_library(&config_dir) {
                *entries.borrow_mut() = fresh;
            }
            refresh_ui();
        })
    };

    {
        let window = window.clone();
        let share_service = share_service.clone();
        let config_dir = config_dir.clone();
        let books_dir = books_dir.clone();
        let refresh = refresh.clone();
        share_btn.connect_clicked(move |_| {
            build_nearby_devices_dialog(&window, share_service.clone(), config_dir.clone(), books_dir.clone(), refresh.clone());
        });
    }

    {
        let filters = filters.clone();
        let refresh_ui = refresh_ui.clone();
        search_entry.connect_changed(move |entry| {
            *filters.query.borrow_mut() = entry.text().to_lowercase();
            refresh_ui();
        });
    }

    {
        let filters = filters.clone();
        let refresh_ui = refresh_ui.clone();
        sort_dropdown.connect_selected_notify(move |dd| {
            if let Some(mode) = SortMode::ALL.get(dd.selected() as usize) {
                *filters.sort.borrow_mut() = *mode;
                refresh_ui();
            }
        });
    }

    for (status, btn) in status_buttons {
        let filters = filters.clone();
        let refresh_ui = refresh_ui.clone();
        btn.connect_toggled(move |b| {
            if b.is_active() {
                *filters.status.borrow_mut() = status;
                refresh_ui();
            }
        });
    }

    {
        let filters = filters.clone();
        let refresh_ui = refresh_ui.clone();
        let id = language_dropdown.connect_selected_notify(move |dd| {
            let selected = if dd.selected() == 0 {
                None
            } else {
                dd.selected_item().and_then(|o| o.downcast::<StringObject>().ok()).map(|s| s.string().to_string())
            };
            *filters.language.borrow_mut() = selected;
            // Deferred to the next main-loop iteration, not called inline:
            // `refresh_ui` (via `rebuild_language_dropdown`) replaces *this
            // same dropdown's* model, and doing that synchronously from
            // inside its own `selected-notify` handler raced with GTK's own
            // still-unwinding internal click handling for the popover list
            // item just selected -- confirmed live as a
            // `g_object_notify_by_pspec: assertion 'G_IS_OBJECT (object)'
            // failed` crash. Letting that unwind fully first (one
            // `idle_add_local_once` hop) fixed it.
            let refresh_ui = refresh_ui.clone();
            glib::idle_add_local_once(move || refresh_ui());
        });
        // See `rebuild_language_dropdown`'s doc comment: this handler must
        // be blocked while that function resyncs the dropdown's model, or
        // the two call each other forever.
        *language_handler_id.borrow_mut() = Some(id);
    }

    {
        let window = window.clone();
        let config_dir = config_dir.clone();
        let books_dir = books_dir.clone();
        let entries = entries.clone();
        let import_ui = import_ui.clone();
        let refresh_ui = refresh_ui.clone();
        let import_popover = import_popover.clone();
        import_book_btn.connect_clicked(move |_| {
            import_popover.popdown();
            let epub_filter = FileFilter::new();
            epub_filter.set_name(Some("EPUB books"));
            epub_filter.add_suffix("epub");
            let file_filters = gio::ListStore::new::<FileFilter>();
            file_filters.append(&epub_filter);

            let dialog = FileDialog::builder().title("Import EPUB").accept_label("Import").build();
            dialog.set_filters(Some(&file_filters));

            let config_dir = config_dir.clone();
            let books_dir = books_dir.clone();
            let entries = entries.clone();
            let import_ui = import_ui.clone();
            let refresh_ui = refresh_ui.clone();
            dialog.open(Some(&window), gio::Cancellable::NONE, move |result| {
                let Ok(file) = result else { return };
                let Some(path) = file.path() else { return };
                spawn_import_one(config_dir, books_dir, path, entries, import_ui, refresh_ui);
            });
        });
    }

    {
        let window = window.clone();
        let config_dir = config_dir.clone();
        let books_dir = books_dir.clone();
        let entries = entries.clone();
        let import_ui = import_ui.clone();
        let refresh_ui = refresh_ui.clone();
        let import_popover = import_popover.clone();
        import_folder_btn.connect_clicked(move |_| {
            import_popover.popdown();
            let dialog = FileDialog::builder().title("Import Folder of EPUBs").accept_label("Import").build();

            let config_dir = config_dir.clone();
            let books_dir = books_dir.clone();
            let entries = entries.clone();
            let import_ui = import_ui.clone();
            let refresh_ui = refresh_ui.clone();
            dialog.select_folder(Some(&window), gio::Cancellable::NONE, move |result| {
                let Ok(file) = result else { return };
                let Some(path) = file.path() else { return };
                spawn_import_folder(config_dir, books_dir, path, entries, import_ui, refresh_ui);
            });
        });
    }

    Ok((root, refresh))
}

/// `search_entry` is only needed for the author button's click handler
/// (jumps to that author by populating the free-text search, see the note
/// on that below) -- everything else about *which* entries render and in
/// what order comes from `filters`. `entries_rc`/`config_dir` are only for
/// the language-edit button's save action (persist + update the shared
/// in-memory list); `refresh_ui_cell` is how that action re-triggers a full
/// refresh afterward without `rebuild_flowbox` needing `refresh_ui` passed
/// in directly, which would be self-referential (`refresh_ui` is itself the
/// thing that calls `rebuild_flowbox`) -- filled in once, right after
/// `refresh_ui` is constructed, before anything can click a card.
#[allow(clippy::too_many_arguments)]
fn rebuild_flowbox(
    flowbox: &FlowBox,
    entries: &[LibraryEntry],
    entries_rc: &Rc<RefCell<Vec<LibraryEntry>>>,
    filters: &FilterState,
    cover_cache: &CoverCache,
    on_open: &Rc<dyn Fn(&str)>,
    content_stack: &gtk::Stack,
    search_entry: &SearchEntry,
    config_dir: &Rc<PathBuf>,
    refresh_ui_cell: &Rc<RefCell<Option<Rc<dyn Fn()>>>>,
) {
    content_stack.set_visible_child_name(if entries.is_empty() { "empty" } else { "grid" });

    while let Some(child) = flowbox.first_child() {
        flowbox.remove(&child);
    }

    let mut pending = Vec::new();

    for entry in filtered_and_sorted(entries, filters) {
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

        // Cover + title are the "open this book" target; the author line
        // deliberately sits *outside* that button rather than inside the
        // same card widget it's built from, below -- GTK buttons nested
        // inside other buttons fight over click handling, so when the
        // author needs its own click target (jump to their other books),
        // it has to be a sibling of the open-button, not a child of it.
        let open_card = GtkBox::new(Orientation::Vertical, 4);
        open_card.append(&picture);
        open_card.append(&title);
        let open_btn = Button::builder().child(&open_card).has_frame(false).build();
        let path = entry.path.clone();
        let on_open = on_open.clone();
        open_btn.connect_clicked(move |_| on_open(&path));

        let card = GtkBox::new(Orientation::Vertical, 4);
        card.set_width_request(COVER_WIDTH + 20);
        card.append(&open_btn);

        let meta_row = GtkBox::new(Orientation::Horizontal, 2);
        if let Some(author_name) = entry.author.clone() {
            let author_label = Label::new(Some(&author_name));
            author_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            author_label.set_max_width_chars(12);
            author_label.add_css_class("dim-label");
            author_label.set_hexpand(true);
            author_label.set_halign(Align::Start);

            let author_btn = Button::builder().child(&author_label).has_frame(false).hexpand(true).build();
            author_btn.set_tooltip_text(Some(&format!("Show books by {author_name}")));
            let search_entry = search_entry.clone();
            author_btn.connect_clicked(move |_| {
                // Reuses the existing free-text search rather than a
                // separate "author filter" dimension -- simpler state, and
                // correct for the common case; the one edge case (two
                // authors whose names happen to share a substring) isn't
                // worth a dedicated exact-match filter for a library
                // browser this size.
                search_entry.set_text(&author_name);
            });
            meta_row.append(&author_btn);
        }

        let edit_lang_btn = Button::from_icon_name("document-edit-symbolic");
        edit_lang_btn.set_has_frame(false);
        edit_lang_btn.set_tooltip_text(Some("Edit language metadata"));
        edit_lang_btn.add_css_class("flat");
        {
            let path = entry.path.clone();
            let current_language = entry.language.clone();
            let config_dir = config_dir.clone();
            let entries_rc = entries_rc.clone();
            let refresh_ui_cell = refresh_ui_cell.clone();
            let edit_lang_btn_c = edit_lang_btn.clone();
            edit_lang_btn.connect_clicked(move |_| {
                // A fresh Popover every click, never a mutated live one --
                // same reasoning as every other popover in this app (see
                // dictionary_ui.rs).
                let popover = gtk::Popover::new();
                popover.set_parent(&edit_lang_btn_c);

                let content = GtkBox::new(Orientation::Vertical, 6);
                content.set_margin_top(8);
                content.set_margin_bottom(8);
                content.set_margin_start(8);
                content.set_margin_end(8);
                content.set_width_request(200);

                let hint = Label::new(Some("Language code (e.g. en, fr, de) -- some source EPUBs, public-domain scans especially, declare the wrong one or none at all."));
                hint.set_wrap(true);
                hint.set_halign(Align::Start);
                hint.add_css_class("dim-label");
                content.append(&hint);

                let language_entry = gtk::Entry::new();
                language_entry.set_text(current_language.as_deref().unwrap_or(""));
                content.append(&language_entry);

                let btn_row = GtkBox::new(Orientation::Horizontal, 6);
                let clear_btn = Button::with_label("Clear");
                let save_btn = Button::with_label("Save");
                save_btn.add_css_class("suggested-action");
                btn_row.append(&clear_btn);
                btn_row.append(&save_btn);
                content.append(&btn_row);

                popover.set_child(Some(&content));

                let apply = {
                    let path = path.clone();
                    let config_dir = config_dir.clone();
                    let entries_rc = entries_rc.clone();
                    let refresh_ui_cell = refresh_ui_cell.clone();
                    let popover = popover.clone();
                    move |language: Option<&str>| {
                        let _ = persistence::set_library_entry_language(&config_dir, &path, language);
                        if let Some(e) = entries_rc.borrow_mut().iter_mut().find(|e| e.path == path) {
                            e.language = language.map(|s| s.to_string());
                        }
                        popover.popdown();
                        if let Some(refresh_ui) = refresh_ui_cell.borrow().clone() {
                            refresh_ui();
                        }
                    }
                };
                {
                    let apply = apply.clone();
                    let language_entry = language_entry.clone();
                    save_btn.connect_clicked(move |_| {
                        let text = language_entry.text().trim().to_string();
                        apply(if text.is_empty() { None } else { Some(text.as_str()) });
                    });
                }
                {
                    // Enter in the entry submits, same as clicking Save.
                    let apply = apply.clone();
                    language_entry.connect_activate(move |entry| {
                        let text = entry.text().trim().to_string();
                        apply(if text.is_empty() { None } else { Some(text.as_str()) });
                    });
                }
                clear_btn.connect_clicked(move |_| apply(None));

                popover.popup();
            });
        }
        meta_row.append(&edit_lang_btn);

        let share_toggle = ToggleButton::builder().icon_name("network-wireless-symbolic").build();
        share_toggle.set_has_frame(false);
        share_toggle.add_css_class("flat");
        share_toggle.set_tooltip_text(Some("Offer this book to devices on your LAN when sharing is on"));
        share_toggle.set_active(entry.shared == Some(true));
        {
            let path = entry.path.clone();
            let config_dir = config_dir.clone();
            let entries_rc = entries_rc.clone();
            let refresh_ui_cell = refresh_ui_cell.clone();
            share_toggle.connect_toggled(move |btn| {
                let shared = btn.is_active();
                let _ = persistence::set_library_entry_shared(&config_dir, &path, shared);
                if let Some(e) = entries_rc.borrow_mut().iter_mut().find(|e| e.path == path) {
                    e.shared = Some(shared);
                }
                if let Some(refresh_ui) = refresh_ui_cell.borrow().clone() {
                    refresh_ui();
                }
            });
        }
        meta_row.append(&share_toggle);
        card.append(&meta_row);

        if let Some(percent) = entry.last_position_percent {
            let progress = ProgressBar::new();
            progress.set_fraction(percent);
            progress.set_margin_top(2);
            card.append(&progress);
        }

        flowbox.append(&card);
    }

    if !pending.is_empty() {
        spawn_lazy_cover_decode(pending, cover_cache.clone());
    }
}

/// One-time background pass filling in `language` for every library entry
/// that predates that field -- anything imported (or last opened) before
/// language capture existed. Without this, the language filter would stay
/// permanently empty for a whole pre-existing library until each book
/// happened to be opened individually (app.rs's own reader-open path
/// already backfills one book at a time, but that's a poor substitute for
/// "the filter actually has options in it"). Runs once per library-page
/// load (see its call site), not on every refresh -- once `library.json` is
/// rewritten with the real value, an entry never needs this again.
fn spawn_language_backfill(config_dir: Rc<PathBuf>, entries: Rc<RefCell<Vec<LibraryEntry>>>, refresh_ui: Rc<dyn Fn()>) {
    let paths: Vec<String> = entries.borrow().iter().filter(|e| e.language.is_none()).map(|e| e.path.clone()).collect();
    if paths.is_empty() {
        return;
    }

    let (tx, rx) = mpsc::channel::<(String, Option<String>)>();
    std::thread::spawn(move || {
        for path in paths {
            let language = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .ok()
                .and_then(|conn| weland::db::load_metadata(&conn).ok())
                .and_then(|metadata| metadata.get("language").cloned())
                .filter(|lang| !lang.is_empty());
            if tx.send((path, language)).is_err() {
                break;
            }
        }
    });

    glib::timeout_add_local(Duration::from_millis(150), move || {
        let mut updated = false;
        loop {
            match rx.try_recv() {
                Ok((path, language)) => {
                    if language.is_some() {
                        if let Some(entry) = entries.borrow_mut().iter_mut().find(|e| e.path == path) {
                            entry.language = language;
                            updated = true;
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if updated {
                        let _ = persistence::write_library(&config_dir, &entries.borrow());
                        refresh_ui();
                    }
                    return glib::ControlFlow::Continue;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    if updated {
                        let _ = persistence::write_library(&config_dir, &entries.borrow());
                        refresh_ui();
                    }
                    return glib::ControlFlow::Break;
                }
            }
        }
    });
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

fn spawn_import_one(
    config_dir: Rc<PathBuf>,
    books_dir: Rc<PathBuf>,
    input: PathBuf,
    entries: Rc<RefCell<Vec<LibraryEntry>>>,
    import_ui: ImportUi,
    refresh_ui: Rc<dyn Fn()>,
) {
    let (tx, rx) = mpsc::channel::<ImportMsg>();
    import_ui.start("Importing\u{2026} (you can keep browsing or open a book meanwhile)");

    let config_dir_owned = (*config_dir).clone();
    let books_dir_owned = (*books_dir).clone();
    // The file name alone, not the full path -- plenty to recognize which
    // book this was about, and a lot less likely to need `status_label`'s
    // ellipsizing to kick in for the common case.
    let input_display = input.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| input.display().to_string());
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
        refresh_ui();
    });
}

fn spawn_import_folder(
    config_dir: Rc<PathBuf>,
    books_dir: Rc<PathBuf>,
    root: PathBuf,
    entries: Rc<RefCell<Vec<LibraryEntry>>>,
    import_ui: ImportUi,
    refresh_ui: Rc<dyn Fn()>,
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
            let entries = entries.clone();
            let config_dir = config_dir.clone();
            let import_ui = import_ui.clone();
            let refresh_ui = refresh_ui.clone();
            move |done, total| {
                import_ui.label.set_text(&format!(
                    "Importing folder\u{2026} {done}/{total} (you can keep browsing or open a book meanwhile)"
                ));
                if let Ok(fresh) = persistence::read_library(&config_dir) {
                    *entries.borrow_mut() = fresh;
                }
                refresh_ui();
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
            refresh_ui();
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
    persistence::upsert_library_entry(
        config_dir,
        &output.to_string_lossy(),
        &title,
        metadata.get("author").map(|s| s.as_str()),
        metadata.get("language").map(|s| s.as_str()),
        metadata.get("source_epub_sha256").map(|s| s.as_str()),
    )?;
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

fn sharing_device_name() -> String {
    gethostname::gethostname().to_string_lossy().to_string()
}

/// "Nearby Devices" dialog: lists LAN peers currently discovered by
/// `share_service` (re-rendered on a short timer while the dialog is open,
/// stopped via `dialog.connect_closed` once it isn't -- there's no push
/// notification from `sharing::ShareService`, and polling a plain `Vec` is
/// simpler than wiring one up for a list this size). Shows an explanatory
/// message instead of a peer list when sharing is off.
fn build_nearby_devices_dialog(
    parent: &impl IsA<gtk::Widget>,
    share_service: Rc<RefCell<Option<Rc<sharing::ShareService>>>>,
    config_dir: Rc<PathBuf>,
    books_dir: Rc<PathBuf>,
    refresh: Rc<dyn Fn()>,
) {
    let list = GtkBox::new(Orientation::Vertical, 10);
    list.set_margin_top(8);
    list.set_margin_bottom(8);
    list.set_margin_start(8);
    list.set_margin_end(8);

    let scroller = ScrolledWindow::builder().child(&list).hscrollbar_policy(PolicyType::Never).vexpand(true).build();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&Label::new(Some("Nearby Devices"))));
    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&scroller));

    let dialog = adw::Dialog::new();
    dialog.set_presentation_mode(adw::DialogPresentationMode::Floating);
    dialog.set_content_width(420);
    dialog.set_content_height(480);
    dialog.set_child(Some(&toolbar_view));

    let stop_polling = Rc::new(std::cell::Cell::new(false));
    {
        let stop_polling = stop_polling.clone();
        dialog.connect_closed(move |_| stop_polling.set(true));
    }

    let render = {
        let list = list.clone();
        let share_service = share_service.clone();
        let config_dir = config_dir.clone();
        let books_dir = books_dir.clone();
        let refresh = refresh.clone();
        let dialog = dialog.clone();
        move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }

            let Some(service) = share_service.borrow().clone() else {
                let msg = Label::new(Some("Enable LAN sharing in Settings to discover nearby devices."));
                msg.set_wrap(true);
                msg.set_halign(Align::Start);
                msg.add_css_class("dim-label");
                list.append(&msg);
                return;
            };

            let peers = service.peers();
            if peers.is_empty() {
                let msg = Label::new(Some("Searching for devices\u{2026} make sure LAN sharing is on for them too."));
                msg.set_wrap(true);
                msg.set_halign(Align::Start);
                msg.add_css_class("dim-label");
                list.append(&msg);
                return;
            }

            for peer in peers {
                let row = GtkBox::new(Orientation::Horizontal, 8);
                let name_label = Label::new(Some(&peer.name));
                name_label.set_hexpand(true);
                name_label.set_halign(Align::Start);
                row.append(&name_label);

                let books_btn = Button::with_label("Books\u{2026}");
                {
                    let config_dir = config_dir.clone();
                    let books_dir = books_dir.clone();
                    let refresh = refresh.clone();
                    let nearby_dialog = dialog.clone();
                    let addr = peer.addr;
                    let peer_name = peer.name.clone();
                    books_btn.connect_clicked(move |_| {
                        spawn_fetch_peer_books(addr, peer_name.clone(), nearby_dialog.clone(), config_dir.clone(), books_dir.clone(), refresh.clone());
                    });
                }
                row.append(&books_btn);
                list.append(&row);
            }
        }
    };

    render();
    glib::timeout_add_local(Duration::from_millis(1500), move || {
        if stop_polling.get() {
            return glib::ControlFlow::Break;
        }
        render();
        glib::ControlFlow::Continue
    });

    dialog.present(Some(parent));
}

enum FetchBooksMsg {
    Done(Vec<sharing::SharedBook>),
    Failed(String),
}

/// Fetches one peer's shared-book list in the background (blocking network
/// I/O, same `std::thread::spawn` + `mpsc` + `glib::timeout_add_local`
/// pattern as every other background op in this file) and opens
/// `build_peer_books_dialog` once it lands.
fn spawn_fetch_peer_books(
    addr: SocketAddr,
    peer_name: String,
    nearby_dialog: adw::Dialog,
    config_dir: Rc<PathBuf>,
    books_dir: Rc<PathBuf>,
    refresh: Rc<dyn Fn()>,
) {
    let (tx, rx) = mpsc::channel::<FetchBooksMsg>();
    std::thread::spawn(move || {
        let _ = tx.send(match sharing::fetch_peer_books(addr) {
            Ok(books) => FetchBooksMsg::Done(books),
            Err(e) => FetchBooksMsg::Failed(e.to_string()),
        });
    });

    glib::timeout_add_local(Duration::from_millis(150), move || match rx.try_recv() {
        Ok(FetchBooksMsg::Done(books)) => {
            build_peer_books_dialog(nearby_dialog.clone(), addr, peer_name.clone(), books, config_dir.clone(), books_dir.clone(), refresh.clone());
            glib::ControlFlow::Break
        }
        Ok(FetchBooksMsg::Failed(err)) => {
            eprintln!("[sharing] failed to fetch books from {peer_name}: {err}");
            let alert = adw::AlertDialog::new(Some("Couldn\u{2019}t reach device"), Some(&err));
            alert.add_response("ok", "OK");
            alert.present(Some(&nearby_dialog));
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
    });
}

/// `filter` is already lowercased by the caller (the search box's live text,
/// once per keystroke) -- kept as a plain function, not inlined into the
/// dialog's render closure, so it's unit-testable without constructing any
/// GTK widgets.
fn peer_book_matches_search(book: &sharing::SharedBook, filter: &str) -> bool {
    filter.is_empty()
        || book.title.to_lowercase().contains(filter)
        || book.author.as_ref().is_some_and(|a| a.to_lowercase().contains(filter))
}

/// Lists one peer's shared books with an Import button each -- clicking one
/// always goes through `confirm_and_import`'s accept dialog first, never
/// imports directly, per the security posture in the plan (no auto-import
/// on receipt, ever). Takes `nearby_dialog` (not just a generic parent
/// widget) so a successful import can close both this dialog and it,
/// dropping the user straight back onto the library grid where the newly
/// imported book is now visible -- otherwise a completed import has no
/// visible effect until the user manually closes two dialogs to find it.
fn build_peer_books_dialog(
    nearby_dialog: adw::Dialog,
    addr: SocketAddr,
    peer_name: String,
    books: Vec<sharing::SharedBook>,
    config_dir: Rc<PathBuf>,
    books_dir: Rc<PathBuf>,
    refresh: Rc<dyn Fn()>,
) {
    // Built before its content -- a per-book "Import" click needs to open an
    // `AlertDialog` anchored to *this* dialog (the thing actually on top of
    // the stack when the click happens), not the "Nearby Devices" dialog
    // still sitting underneath it. Anchoring to a dialog that isn't the
    // current top of the stack doesn't error, it just never presents --
    // `AlertDialog::choose()`'s callback then simply never fires, since
    // nothing the user can see or click ever appeared. That was the bug:
    // every row here used to capture this function's own `parent` parameter
    // (the Nearby Devices dialog) instead of this dialog's own widget.
    let dialog = adw::Dialog::new();
    dialog.set_presentation_mode(adw::DialogPresentationMode::Floating);
    dialog.set_content_width(420);
    dialog.set_content_height(480);

    let books = Rc::new(books);
    let search_entry = SearchEntry::builder().placeholder_text("Search books\u{2026}").build();
    search_entry.set_margin_top(8);
    search_entry.set_margin_start(8);
    search_entry.set_margin_end(8);

    let list = GtkBox::new(Orientation::Vertical, 10);
    list.set_margin_top(8);
    list.set_margin_bottom(8);
    list.set_margin_start(8);
    list.set_margin_end(8);

    // Re-run on every keystroke against the one `books` list this dialog was
    // opened with (a peer's shared list doesn't change while this dialog is
    // open) -- same live-filter shape as the annotation and vocab panels'
    // search boxes, just filtering an in-memory `Vec` instead of SQLite/JSON.
    let render = {
        let list = list.clone();
        let books = books.clone();
        let dialog = dialog.clone();
        let nearby_dialog = nearby_dialog.clone();
        let peer_name = peer_name.clone();
        let config_dir = config_dir.clone();
        let books_dir = books_dir.clone();
        let refresh = refresh.clone();
        let search_entry = search_entry.clone();
        move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }

            let filter = search_entry.text().to_lowercase();
            let matches: Vec<&sharing::SharedBook> = books.iter().filter(|b| peer_book_matches_search(b, &filter)).collect();

            if matches.is_empty() {
                let msg = Label::new(Some(if books.is_empty() {
                    "This device isn't sharing any books right now."
                } else {
                    "No books match your search."
                }));
                msg.set_wrap(true);
                msg.set_halign(Align::Start);
                msg.add_css_class("dim-label");
                list.append(&msg);
                return;
            }

            for book in matches {
                let row = GtkBox::new(Orientation::Vertical, 2);
                let title_label = Label::new(Some(&book.title));
                title_label.set_halign(Align::Start);
                title_label.add_css_class("heading");
                row.append(&title_label);

                if let Some(author) = &book.author {
                    let author_label = Label::new(Some(author));
                    author_label.set_halign(Align::Start);
                    author_label.add_css_class("dim-label");
                    row.append(&author_label);
                }

                let import_btn = Button::with_label(&format!("Import ({:.1} MB)\u{2026}", book.size as f64 / (1024.0 * 1024.0)));
                import_btn.set_halign(Align::Start);
                {
                    let nearby_dialog = nearby_dialog.clone();
                    let books_dialog = dialog.clone();
                    let book = book.clone();
                    let peer_name = peer_name.clone();
                    let config_dir = config_dir.clone();
                    let books_dir = books_dir.clone();
                    let refresh = refresh.clone();
                    import_btn.connect_clicked(move |_| {
                        confirm_and_import(
                            nearby_dialog.clone(),
                            books_dialog.clone(),
                            addr,
                            peer_name.clone(),
                            book.clone(),
                            config_dir.clone(),
                            books_dir.clone(),
                            refresh.clone(),
                        );
                    });
                }
                row.append(&import_btn);
                list.append(&row);
                list.append(&Separator::new(Orientation::Horizontal));
            }
        }
    };

    render();
    {
        let render = render.clone();
        search_entry.connect_changed(move |_| render());
    }

    let scroller = ScrolledWindow::builder().child(&list).hscrollbar_policy(PolicyType::Never).vexpand(true).build();
    let panel = GtkBox::new(Orientation::Vertical, 0);
    panel.append(&search_entry);
    panel.append(&scroller);

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&Label::new(Some(&format!("Books from {peer_name}")))));
    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&panel));

    dialog.set_child(Some(&toolbar_view));
    dialog.present(Some(&nearby_dialog));
}

/// The one and only path to an import from a peer -- always a named,
/// sized, explicit confirmation before anything is fetched, let alone
/// written to disk.
#[allow(clippy::too_many_arguments)]
fn confirm_and_import(
    nearby_dialog: adw::Dialog,
    books_dialog: adw::Dialog,
    addr: SocketAddr,
    peer_name: String,
    book: sharing::SharedBook,
    config_dir: Rc<PathBuf>,
    books_dir: Rc<PathBuf>,
    refresh: Rc<dyn Fn()>,
) {
    let body = format!(
        "Import \u{201c}{}\u{201d}{} from {peer_name}? ({:.1} MB)",
        book.title,
        book.author.as_ref().map(|a| format!(" by {a}")).unwrap_or_default(),
        book.size as f64 / (1024.0 * 1024.0)
    );
    let alert = adw::AlertDialog::new(Some("Import book?"), Some(&body));
    alert.add_response("cancel", "Cancel");
    alert.add_response("import", "Import");
    alert.set_response_appearance("import", adw::ResponseAppearance::Suggested);
    alert.set_default_response(Some("import"));
    alert.set_close_response("cancel");

    let alert_parent = books_dialog.clone();
    alert.choose(Some(&alert_parent), gio::Cancellable::NONE, move |response| {
        if response.as_str() == "import" {
            spawn_import_from_peer(addr, book.clone(), config_dir.clone(), books_dir.clone(), refresh.clone(), nearby_dialog.clone(), books_dialog.clone());
        }
    });
}

enum ImportPeerMsg {
    Done,
    Failed(String),
}

/// Fetch-validate-and-register in the background -- `sharing::import_from_peer`
/// does the actual work (including rejecting a malformed/mismatched
/// transfer); this just marshals its result back to the UI. On success both
/// dialogs close so the underlying (now-refreshed) library grid is
/// immediately visible with the new book in it -- that's the only positive
/// confirmation this gives, deliberately, rather than adding a separate
/// toast/status mechanism just for this one flow. On failure both dialogs
/// stay open and an `AlertDialog` states what went wrong, anchored to
/// `books_dialog` so the user can see it and try a different book.
#[allow(clippy::too_many_arguments)]
fn spawn_import_from_peer(
    addr: SocketAddr,
    book: sharing::SharedBook,
    config_dir: Rc<PathBuf>,
    books_dir: Rc<PathBuf>,
    refresh: Rc<dyn Fn()>,
    nearby_dialog: adw::Dialog,
    books_dialog: adw::Dialog,
) {
    let (tx, rx) = mpsc::channel::<ImportPeerMsg>();
    let config_dir_owned = (*config_dir).clone();
    let books_dir_owned = (*books_dir).clone();
    std::thread::spawn(move || {
        let result = sharing::import_from_peer(&config_dir_owned, &books_dir_owned, addr, &book);
        let _ = tx.send(match result {
            Ok(()) => ImportPeerMsg::Done,
            Err(e) => ImportPeerMsg::Failed(e.to_string()),
        });
    });

    glib::timeout_add_local(Duration::from_millis(150), move || match rx.try_recv() {
        Ok(ImportPeerMsg::Done) => {
            refresh();
            books_dialog.close();
            nearby_dialog.close();
            glib::ControlFlow::Break
        }
        Ok(ImportPeerMsg::Failed(err)) => {
            eprintln!("[sharing] import failed: {err}");
            let alert = adw::AlertDialog::new(Some("Import failed"), Some(&err));
            alert.add_response("ok", "OK");
            alert.present(Some(&books_dialog));
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_book(title: &str, author: Option<&str>) -> sharing::SharedBook {
        sharing::SharedBook { title: title.to_string(), author: author.map(String::from), content_hash: "hash".to_string(), size: 0 }
    }

    #[test]
    fn peer_book_search_matches_title_and_author_case_insensitively() {
        let book = shared_book("The Divine Comedy", Some("Dante Alighieri"));

        assert!(peer_book_matches_search(&book, ""), "an empty filter must match everything");
        assert!(peer_book_matches_search(&book, "divine"), "must match a substring of the title");
        assert!(peer_book_matches_search(&book, &"DANTE".to_lowercase()), "must match case-insensitively");
        assert!(peer_book_matches_search(&book, "alighieri"), "must match a substring of the author");
        assert!(!peer_book_matches_search(&book, "beowulf"), "must not match unrelated text");
    }

    #[test]
    fn peer_book_search_handles_missing_author() {
        let book = shared_book("Fresh Test Book", None);
        assert!(peer_book_matches_search(&book, ""));
        assert!(peer_book_matches_search(&book, "fresh"));
        assert!(!peer_book_matches_search(&book, "anyone"), "no author to match against must not panic or false-match");
    }

    fn entry(title: &str, author: Option<&str>, language: Option<&str>, added_at: i64, last_opened_at: i64, percent: Option<f64>) -> LibraryEntry {
        LibraryEntry {
            path: format!("/books/{title}.wld"),
            title: title.to_string(),
            author: author.map(String::from),
            added_at,
            last_opened_at,
            last_position_node_id: percent.map(|_| 1),
            last_position_percent: percent,
            language: language.map(String::from),
            content_hash: None,
            shared: None,
        }
    }

    #[test]
    fn reading_status_buckets_by_percent() {
        assert_eq!(reading_status(&entry("A", None, None, 0, 0, None)), ReadingStatus::Unread);
        assert_eq!(reading_status(&entry("A", None, None, 0, 0, Some(0.0))), ReadingStatus::Unread);
        assert_eq!(reading_status(&entry("A", None, None, 0, 0, Some(0.5))), ReadingStatus::InProgress);
        assert_eq!(reading_status(&entry("A", None, None, 0, 0, Some(0.99))), ReadingStatus::Finished);
        assert_eq!(reading_status(&entry("A", None, None, 0, 0, Some(FINISHED_THRESHOLD))), ReadingStatus::Finished);
    }

    #[test]
    fn filtered_and_sorted_applies_query_status_and_language_together() {
        let entries = vec![
            entry("Beowulf", Some("Unknown"), Some("en"), 1, 1, None),
            entry("The Poetic Edda", Some("Snorri Sturluson"), Some("en"), 2, 2, Some(0.5)),
            entry("Faust", Some("Goethe"), Some("de"), 3, 3, Some(1.0)),
        ];
        let filters = FilterState::new();

        // No filters: everything, default sort (RecentlyOpened) puts the
        // highest last_opened_at first.
        let all = filtered_and_sorted(&entries, &filters);
        assert_eq!(all.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(), vec!["Faust", "The Poetic Edda", "Beowulf"]);

        *filters.status.borrow_mut() = StatusFilter::InProgress;
        let in_progress = filtered_and_sorted(&entries, &filters);
        assert_eq!(in_progress.len(), 1);
        assert_eq!(in_progress[0].title, "The Poetic Edda");

        *filters.status.borrow_mut() = StatusFilter::All;
        *filters.language.borrow_mut() = Some("de".to_string());
        let german = filtered_and_sorted(&entries, &filters);
        assert_eq!(german.len(), 1);
        assert_eq!(german[0].title, "Faust");

        *filters.language.borrow_mut() = None;
        *filters.query.borrow_mut() = "poetic".to_string();
        let searched = filtered_and_sorted(&entries, &filters);
        assert_eq!(searched.len(), 1);
        assert_eq!(searched[0].title, "The Poetic Edda");
    }

    #[test]
    fn filtered_and_sorted_query_matches_author_too() {
        let entries = vec![entry("The Poetic Edda", Some("Snorri Sturluson"), None, 1, 1, None)];
        let filters = FilterState::new();
        *filters.query.borrow_mut() = "sturluson".to_string();
        assert_eq!(filtered_and_sorted(&entries, &filters).len(), 1);
    }

    #[test]
    fn sort_modes_order_correctly() {
        let entries = vec![
            entry("Zeta", Some("Zed"), None, 100, 10, Some(0.1)),
            entry("Alpha", Some("Anne"), None, 200, 20, Some(0.9)),
        ];
        let filters = FilterState::new();

        *filters.sort.borrow_mut() = SortMode::TitleAsc;
        assert_eq!(filtered_and_sorted(&entries, &filters)[0].title, "Alpha");

        *filters.sort.borrow_mut() = SortMode::TitleDesc;
        assert_eq!(filtered_and_sorted(&entries, &filters)[0].title, "Zeta");

        *filters.sort.borrow_mut() = SortMode::AuthorAsc;
        assert_eq!(filtered_and_sorted(&entries, &filters)[0].author.as_deref(), Some("Anne"));

        *filters.sort.borrow_mut() = SortMode::AuthorDesc;
        assert_eq!(filtered_and_sorted(&entries, &filters)[0].author.as_deref(), Some("Zed"));

        *filters.sort.borrow_mut() = SortMode::RecentlyAdded;
        assert_eq!(filtered_and_sorted(&entries, &filters)[0].title, "Alpha");

        *filters.sort.borrow_mut() = SortMode::Progress;
        assert_eq!(filtered_and_sorted(&entries, &filters)[0].title, "Alpha");
    }

    #[test]
    fn author_sort_uses_surname_not_full_name() {
        assert_eq!(author_sort_key("George R. R. Martin"), "martin");
        assert_eq!(author_sort_key("Amy Adams"), "adams");

        // First-name order and surname order disagree for this pair --
        // "Amy" < "Zoe" but "martin" < "wells", so this only passes if
        // sorting is really keying off the surname.
        let entries = vec![
            entry("The Hollow Crown", Some("Zoe Martin"), None, 1, 1, None),
            entry("Faust", Some("Amy Wells"), None, 2, 2, None),
        ];
        let filters = FilterState::new();
        *filters.sort.borrow_mut() = SortMode::AuthorAsc;
        assert_eq!(filtered_and_sorted(&entries, &filters)[0].author.as_deref(), Some("Zoe Martin"));
    }

    #[test]
    fn title_sort_ignores_leading_articles() {
        assert_eq!(title_sort_key("The Hobbit"), "hobbit");
        assert_eq!(title_sort_key("An American Tragedy"), "american tragedy");
        assert_eq!(title_sort_key("A Game of Thrones"), "game of thrones");
        // No leading article -- unaffected.
        assert_eq!(title_sort_key("Beowulf"), "beowulf");

        // "The Apple" sorts under "A" (ahead of "Banana Republic"), not "T"
        // -- straight string comparison ("the apple" vs "banana...") would
        // put Banana Republic first instead, since 't' > 'b'.
        let entries = vec![
            entry("Banana Republic", None, None, 1, 1, None),
            entry("The Apple", None, None, 2, 2, None),
        ];
        let filters = FilterState::new();
        *filters.sort.borrow_mut() = SortMode::TitleAsc;
        assert_eq!(filtered_and_sorted(&entries, &filters)[0].title, "The Apple");
    }

    #[test]
    fn library_stats_summarizes_the_whole_library_not_a_filtered_subset() {
        let entries = vec![
            entry("A", Some("X"), Some("en"), 1, 1, None),
            entry("B", Some("Y"), Some("en"), 2, 2, Some(0.5)),
            entry("C", Some("X"), Some("fr"), 3, 3, Some(1.0)),
        ];
        let stats = library_stats(&entries, entries.len());
        assert!(stats.contains("3 books"), "{stats}");
        assert!(stats.contains("1 in progress"), "{stats}");
        assert!(stats.contains("1 finished"), "{stats}");
        assert!(stats.contains("1 unread"), "{stats}");
        assert!(stats.contains("2 authors"), "{stats}");
        assert!(stats.contains("2 languages"), "{stats}");
    }

    #[test]
    fn library_stats_shows_the_visible_count_when_a_filter_narrows_it() {
        let entries = vec![
            entry("A", Some("X"), None, 1, 1, None),
            entry("B", Some("Y"), None, 2, 2, None),
            entry("C", Some("X"), None, 3, 3, None),
        ];
        let stats = library_stats(&entries, 1);
        assert!(stats.contains("Showing 1 of 3 books"), "{stats}");
    }

    #[test]
    fn library_stats_is_empty_for_an_empty_library() {
        assert_eq!(library_stats(&[], 0), "");
    }
}
