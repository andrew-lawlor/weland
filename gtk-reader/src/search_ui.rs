//! Per-book full-text search: a query box and results list in the sidebar,
//! jumping to a hit via the same Phase 2 node index the TOC and annotations
//! panel use, with a brief temporary highlight ("flash") on the matched
//! term so the jump target is obvious on a long page. Mirrors the web
//! reader's `runSearch`/`flashSearchMatch` behavior in `reader/dist/app.js`.
//!
//! Reuses `AnnotationState`'s node list/index/text view rather than
//! threading a parallel copy through `app.rs` — search itself is read-only
//! against the book, so it gets its own `Connection` (same "one connection
//! per concern" pattern as the lazy image decode in `app.rs`).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::{
    gdk, glib, pango, prelude::*, Align, Box as GtkBox, Button, Entry, Label, Orientation, PolicyType, ScrolledWindow,
    TextTag,
};
use rusqlite::Connection;
use weland::db;

use crate::annotation_ui::{self, AnnotationState};

pub fn build_search_panel(conn: Connection, state: Rc<AnnotationState>) -> GtkBox {
    let buffer = state.text_view().buffer();
    let flash_tag = buffer
        .create_tag(Some("search_flash"), &[("background-rgba", &gdk::RGBA::new(0.996, 0.780, 0.345, 0.85))])
        .expect("create search flash tag");

    let entry = Entry::builder().placeholder_text("Search this book\u{2026}").build();
    entry.set_margin_top(8);
    entry.set_margin_start(8);
    entry.set_margin_end(8);

    let results = GtkBox::new(Orientation::Vertical, 4);
    results.set_margin_top(4);
    results.set_margin_bottom(8);
    results.set_margin_start(8);
    results.set_margin_end(8);

    let scroller = ScrolledWindow::builder().child(&results).hscrollbar_policy(PolicyType::Never).vexpand(true).build();

    let panel = GtkBox::new(Orientation::Vertical, 0);
    panel.append(&entry);
    panel.append(&scroller);

    let conn = Rc::new(conn);
    let debounce: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    {
        let conn = conn.clone();
        let results = results.clone();
        let state = state.clone();
        let flash_tag = flash_tag.clone();
        let debounce = debounce.clone();
        entry.connect_changed(move |entry| {
            if let Some(id) = debounce.borrow_mut().take() {
                id.remove();
            }
            let query = entry.text().to_string();
            if query.trim().is_empty() {
                clear_results(&results);
                return;
            }

            let conn = conn.clone();
            let results = results.clone();
            let state = state.clone();
            let flash_tag = flash_tag.clone();
            let debounce_inner = debounce.clone();
            let source_id = glib::timeout_add_local(Duration::from_millis(250), move || {
                run_search(&conn, &query, &results, &state, &flash_tag);
                *debounce_inner.borrow_mut() = None;
                glib::ControlFlow::Break
            });
            *debounce.borrow_mut() = Some(source_id);
        });
    }
    {
        let debounce = debounce.clone();
        entry.connect_activate(move |entry| {
            if let Some(id) = debounce.borrow_mut().take() {
                id.remove();
            }
            let query = entry.text().to_string();
            if !query.trim().is_empty() {
                run_search(&conn, &query, &results, &state, &flash_tag);
            }
        });
    }

    panel
}

fn clear_results(results: &GtkBox) {
    while let Some(child) = results.first_child() {
        results.remove(&child);
    }
}

fn run_search(conn: &Connection, query: &str, results: &GtkBox, state: &Rc<AnnotationState>, flash_tag: &TextTag) {
    clear_results(results);

    let hits = match db::search_nodes(conn, query, 20) {
        Ok(h) => h,
        Err(_) => return,
    };

    if hits.is_empty() {
        let empty = Label::new(Some("No results."));
        empty.set_halign(Align::Start);
        empty.add_css_class("dim-label");
        results.append(&empty);
        return;
    }

    for hit in hits {
        let type_label = Label::new(Some(&hit.node_type));
        type_label.set_halign(Align::Start);
        type_label.add_css_class("dim-label");

        let snippet_label = Label::new(None);
        snippet_label.set_markup(&snippet_markup(&hit.snippet));
        snippet_label.set_wrap(true);
        snippet_label.set_halign(Align::Start);
        snippet_label.set_lines(3);
        snippet_label.set_ellipsize(pango::EllipsizeMode::End);

        let row = GtkBox::new(Orientation::Vertical, 2);
        row.append(&type_label);
        row.append(&snippet_label);

        let btn = Button::builder().child(&row).has_frame(false).build();
        let node_id = hit.node_id;
        let terms = extract_terms(&hit.snippet);
        let state_c = state.clone();
        let flash_tag_c = flash_tag.clone();
        btn.connect_clicked(move |_| {
            flash_match(&state_c, &flash_tag_c, node_id, &terms);
            if let Some(mark) = state_c.index().mark_for_node(node_id) {
                state_c.text_view().scroll_to_mark(mark, 0.0, true, 0.0, 0.0);
            }
        });

        results.append(&btn);
    }
}

/// Briefly highlights the first matched term found in `node_id`'s content,
/// then removes the highlight after ~2s — same one-shot flash the web
/// reader uses to draw the eye to a jump target on a long page.
fn flash_match(state: &Rc<AnnotationState>, tag: &TextTag, node_id: i64, terms: &[String]) {
    let Some(node) = state.nodes().iter().find(|n| n.id == node_id) else { return };
    let content = node.content.clone().unwrap_or_default();
    let buffer = state.text_view().buffer();
    let Some(content_start) = annotation_ui::content_start_offset(&buffer, node, state.index()) else { return };
    let Some((start_rel, end_rel)) = terms.iter().find_map(|t| find_codepoint_range(&content, t)) else { return };

    let start = content_start + start_rel as i32;
    let end = content_start + end_rel as i32;
    let start_iter = buffer.iter_at_offset(start);
    let end_iter = buffer.iter_at_offset(end);
    buffer.apply_tag(tag, &start_iter, &end_iter);

    let buffer_c = buffer.clone();
    let tag_c = tag.clone();
    glib::timeout_add_local_once(Duration::from_millis(2200), move || {
        let start_iter = buffer_c.iter_at_offset(start);
        let end_iter = buffer_c.iter_at_offset(end);
        buffer_c.remove_tag(&tag_c, &start_iter, &end_iter);
    });
}

/// Finds the first case-insensitive occurrence of `term` in `content`, in
/// the same Unicode-codepoint offset space `content_start_offset` uses.
/// Case-folds one `char` at a time (approximate for the rare codepoints
/// whose lowercase form is multiple characters) — good enough for locating
/// a plain-text search match, not a full Unicode case-folding pass.
fn find_codepoint_range(content: &str, term: &str) -> Option<(usize, usize)> {
    let lower: Vec<char> = content.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect();
    let term_lower: Vec<char> = term.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect();
    if term_lower.is_empty() || term_lower.len() > lower.len() {
        return None;
    }
    for i in 0..=(lower.len() - term_lower.len()) {
        if lower[i..i + term_lower.len()] == term_lower[..] {
            return Some((i, i + term_lower.len()));
        }
    }
    None
}

/// Pulls the terms FTS5's `snippet()` wrapped in `«»` back out, so they can
/// be located in the node's actual content for flashing.
fn extract_terms(snippet: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut rest = snippet;
    while let Some(start) = rest.find('\u{ab}') {
        let after = &rest[start + '\u{ab}'.len_utf8()..];
        let Some(end) = after.find('\u{bb}') else { break };
        terms.push(after[..end].to_string());
        rest = &after[end + '\u{bb}'.len_utf8()..];
    }
    terms
}

/// Converts FTS5's `«match»` markers into Pango `<b>` markup, escaping
/// everything else so raw snippet text can't be misread as markup itself.
fn snippet_markup(snippet: &str) -> String {
    let mut out = String::new();
    let mut rest = snippet;
    loop {
        let Some(start) = rest.find('\u{ab}') else {
            out.push_str(&glib::markup_escape_text(rest));
            break;
        };
        out.push_str(&glib::markup_escape_text(&rest[..start]));
        let after = &rest[start + '\u{ab}'.len_utf8()..];
        let Some(end) = after.find('\u{bb}') else {
            out.push_str(&glib::markup_escape_text(after));
            break;
        };
        out.push_str("<b>");
        out.push_str(&glib::markup_escape_text(&after[..end]));
        out.push_str("</b>");
        rest = &after[end + '\u{bb}'.len_utf8()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_terms_pulls_every_bracketed_match() {
        let snippet = "...\u{ab}Robin\u{bb} shot the \u{ab}arrow\u{bb}...";
        assert_eq!(extract_terms(snippet), vec!["Robin".to_string(), "arrow".to_string()]);
    }

    #[test]
    fn extract_terms_handles_no_matches() {
        assert_eq!(extract_terms("no markers here"), Vec::<String>::new());
    }

    #[test]
    fn find_codepoint_range_is_case_insensitive() {
        assert_eq!(find_codepoint_range("Robin Hood was here", "hood"), Some((6, 10)));
    }

    #[test]
    fn find_codepoint_range_returns_none_when_absent() {
        assert_eq!(find_codepoint_range("Robin Hood", "sherwood"), None);
    }

    #[test]
    fn find_codepoint_range_counts_unicode_codepoints_not_bytes() {
        // "café" — é is a single codepoint but 2 bytes in UTF-8. A
        // byte-indexed search would misplace anything after it.
        let (start, end) = find_codepoint_range("café shop", "shop").unwrap();
        assert_eq!(start, 5);
        assert_eq!(end, 9);
    }

    #[test]
    fn snippet_markup_bolds_matches_and_escapes_the_rest() {
        let out = snippet_markup("Tom & \u{ab}Robin\u{bb} <met>");
        assert_eq!(out, "Tom &amp; <b>Robin</b> &lt;met&gt;");
    }

    #[test]
    fn snippet_markup_passes_through_plain_text_unchanged() {
        assert_eq!(snippet_markup("no matches here"), "no matches here");
    }
}
