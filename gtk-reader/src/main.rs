//! Native GTK4 frontend for the Weland .wld format.

use std::cell::RefCell;
use std::env;
use std::rc::Rc;

mod annotation_ui;
mod annotations;
mod app;
mod branding;
mod dictionary;
mod dictionary_ui;
mod document;
mod fonts;
mod keybindings;
mod library;
mod node_index;
mod persistence;
mod recording;
mod search_ui;
mod settings_ui;
mod toc;
mod vocab_ui;

use anyhow::Result;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

const APP_ID: &str = "dev.weland.GtkReaderSpike";

/// Single-window navigation (Phase 10): one `ApplicationWindow` for the
/// whole app lifetime, with a `gtk::Stack` swapped between the library page
/// and (at most one, rebuilt per book) reader page — replaces the earlier
/// one-window-per-book behavior, which was awkward on a small/handheld
/// display and not what a first-time user would expect. `library_refresh`
/// is filled in right after the library page is built; `open_book`/
/// `show_library` only ever run afterward (in response to user input), so
/// the `RefCell` is never read before it's populated.
struct Nav {
    window: adw::ApplicationWindow,
    header: adw::HeaderBar,
    stack: gtk4::Stack,
    library_refresh: RefCell<Option<Rc<dyn Fn()>>>,
    // Back-to-library navigation and the sidebar-collapse toggle are both
    // header-bar-level concepts (GNOME/Adwaita convention), not widgets
    // owned by the reader page itself — packed into `header` on
    // `open_book`, removed again on `show_library`.
    back_button: RefCell<Option<gtk4::Button>>,
    sidebar_toggle: RefCell<Option<gtk4::Button>>,
    // The current reader page's sidebar widget, if any — read fresh by the
    // toggle button's click handler each time, since the page (and its
    // sidebar) gets rebuilt from scratch on every `open_book`.
    current_sidebar: RefCell<Option<gtk4::Box>>,
}

impl Nav {
    fn show_library(self: &Rc<Self>) {
        if let Some(back_button) = self.back_button.borrow_mut().take() {
            self.header.remove(&back_button);
        }
        if let Some(sidebar_toggle) = self.sidebar_toggle.borrow_mut().take() {
            self.header.remove(&sidebar_toggle);
        }
        *self.current_sidebar.borrow_mut() = None;
        if let Some(refresh) = self.library_refresh.borrow().as_ref() {
            refresh();
        }
        self.stack.set_visible_child_name("library");
        self.window.set_title(Some("Weland Library"));
    }

    fn open_book(self: &Rc<Self>, path: &str) {
        let on_back: Rc<dyn Fn()> = {
            let nav = self.clone();
            Rc::new(move || nav.show_library())
        };
        match app::build_reader_page(path, on_back) {
            Ok((page, title, sidebar)) => {
                if let Some(old) = self.stack.child_by_name("reader") {
                    self.stack.remove(&old);
                }
                self.stack.add_named(&page, Some("reader"));
                self.stack.set_visible_child_name("reader");
                self.window.set_title(Some(&title));
                *self.current_sidebar.borrow_mut() = Some(sidebar);

                if self.back_button.borrow().is_none() {
                    let back_button = gtk4::Button::from_icon_name("go-previous-symbolic");
                    back_button.set_tooltip_text(Some("Library"));
                    let nav = self.clone();
                    back_button.connect_clicked(move |_| nav.show_library());
                    self.header.pack_start(&back_button);
                    *self.back_button.borrow_mut() = Some(back_button);
                }
                if self.sidebar_toggle.borrow().is_none() {
                    let sidebar_toggle = gtk4::Button::from_icon_name("sidebar-show-symbolic");
                    sidebar_toggle.set_tooltip_text(Some("Toggle sidebar"));
                    let nav = self.clone();
                    sidebar_toggle.connect_clicked(move |_| {
                        if let Some(sidebar) = nav.current_sidebar.borrow().as_ref() {
                            sidebar.set_visible(!sidebar.is_visible());
                        }
                    });
                    self.header.pack_start(&sidebar_toggle);
                    *self.sidebar_toggle.borrow_mut() = Some(sidebar_toggle);
                }
            }
            Err(e) => eprintln!("error opening {path}: {e}"),
        }
    }
}

fn main() -> Result<()> {
    // A path arg opens straight into the reader (handy for dev/testing);
    // with no arg the library view is the app's real entry point either
    // way, the same shared window can navigate back to it via the reader's
    // "Library" button.
    let path = env::args().nth(1);
    adw::init().expect("libadwaita init");
    let gtk_app = adw::Application::builder().application_id(APP_ID).build();
    gtk_app.connect_activate(move |gtk_app| {
        if let Err(e) = fonts::load_reading_fonts() {
            eprintln!("failed to load reading fonts: {e}");
        }

        let window = adw::ApplicationWindow::builder()
            .application(gtk_app)
            .title("Weland Library")
            .default_width(1080)
            .default_height(1000)
            .build();

        // AdwToolbarView gives every page a proper AdwHeaderBar (native
        // window controls, centered title) instead of relying on bare GTK
        // window decoration — one header shared across both pages, swapped
        // along with the stack's own content underneath it.
        let header = adw::HeaderBar::new();
        let stack = gtk4::Stack::new();
        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header);
        toolbar_view.set_content(Some(&stack));
        window.set_content(Some(&toolbar_view));

        let nav = Rc::new(Nav {
            window: window.clone(),
            header: header.clone(),
            stack: stack.clone(),
            library_refresh: RefCell::new(None),
            back_button: RefCell::new(None),
            sidebar_toggle: RefCell::new(None),
            current_sidebar: RefCell::new(None),
        });

        let on_open: Rc<dyn Fn(&str)> = {
            let nav = nav.clone();
            Rc::new(move |path: &str| nav.open_book(path))
        };
        match library::build_library_page(&window, on_open) {
            Ok((library_page, refresh)) => {
                *nav.library_refresh.borrow_mut() = Some(refresh);
                stack.add_named(&library_page, Some("library"));
            }
            Err(e) => eprintln!("error building library: {e}"),
        }

        match &path {
            Some(path) => nav.open_book(path),
            None => stack.set_visible_child_name("library"),
        }

        window.present();
    });
    gtk_app.run_with_args(&Vec::<String>::new());
    Ok(())
}

#[cfg(test)]
mod tests {
    use gtk4::{self as gtk, prelude::*, TextView};
    use rusqlite::{Connection, OpenFlags};
    use weland::db;

    use crate::{document, node_index::NodeIndex};

    fn check_book(path: &str) {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap_or_else(|e| panic!("open {path}: {e}"));
        let nodes = db::load_ast_nodes(&conn).unwrap_or_else(|e| panic!("load {path}: {e}"));
        assert!(!nodes.is_empty(), "{path} has no ast_nodes");
        let image_node_count = nodes.iter().filter(|n| n.node_type == "image").count();

        let text_view = TextView::new();
        let buffer = text_view.buffer();
        let tags = document::build_tags(&buffer);
        let mut index = NodeIndex::new();
        let mut pending_images = Vec::new();
        document::build_document(&text_view, &buffer, &nodes, &tags, &mut index, &mut pending_images);

        assert!(buffer.char_count() > 0, "{path} produced an empty buffer");
        assert_eq!(index.len(), nodes.len(), "{path}: node index must have one entry per ast_node");
        assert!(!index.is_empty());
        assert_eq!(
            pending_images.len(),
            image_node_count,
            "{path}: every image node must queue a pending decode, none decoded eagerly"
        );

        let offsets = index.offsets(&buffer);
        assert!(
            offsets.windows(2).all(|w| w[0] <= w[1]),
            "{path}: node index offsets must be non-decreasing"
        );
    }

    // The single GTK-backed test entry point for the whole crate — see the
    // comment on `node_index::tests::check_marks_stay_anchored_and_offsets_are_monotonic`
    // for why this can't be split into independent #[test] fns.
    #[test]
    fn gtk_backed_checks() {
        gtk::init().expect("gtk init");

        let scratch = "/tmp/claude-1000/-home-andrew-Documents-Rust-weland/839cf43e-b477-43ff-8379-19470349a793/scratchpad/books";
        check_book(&format!("{scratch}/robin-hood.wld"));
        check_book(&format!("{scratch}/beowulf.wld"));
        check_book(&format!("{scratch}/grist.wld"));

        crate::node_index::tests::check_marks_stay_anchored_and_offsets_are_monotonic();
        crate::annotation_ui::tests::check_selection_anchoring_and_annotation_lookup();
        crate::annotation_ui::tests::check_selection_is_single_word();
        crate::dictionary_ui::tests::check_context_around();
        crate::document::tests::check_stanza_and_line_numbers_both_use_dim_tag();
    }
}
