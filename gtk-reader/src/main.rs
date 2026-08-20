//! Native GTK4 frontend for the Weland .wld format.

use std::env;

mod annotation_ui;
mod annotations;
mod app;
mod dictionary;
mod dictionary_ui;
mod document;
mod fonts;
mod library;
mod node_index;
mod persistence;
mod recording;
mod search_ui;
mod settings_ui;
mod toc;

use anyhow::Result;
use gtk4::{prelude::*, Application};

const APP_ID: &str = "dev.weland.GtkReaderSpike";

fn main() -> Result<()> {
    // A path arg opens straight into the reader (handy for dev/testing);
    // with no arg the library view is the app's real entry point.
    let path = env::args().nth(1);
    let gtk_app = Application::builder().application_id(APP_ID).build();
    gtk_app.connect_activate(move |gtk_app| {
        if let Err(e) = fonts::load_reading_fonts() {
            eprintln!("failed to load reading fonts: {e}");
        }
        let result = match &path {
            Some(path) => app::build_ui(gtk_app, path),
            None => library::build_library_window(gtk_app),
        };
        if let Err(e) = result {
            eprintln!("error: {e}");
        }
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
    }
}
