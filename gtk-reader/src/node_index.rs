//! Maps `ast_node` ids to positions in the `GtkTextBuffer`, so reading
//! position, TOC jump-to, and (later) annotation anchoring can all resolve
//! "where in the document is node N" without re-deriving it three times.
//!
//! One `TextMark` is recorded per node, at the start of that node's content,
//! **with `left_gravity: true`**. This isn't optional: a mark created with
//! `left_gravity: false` right after inserting a node keeps sliding forward
//! as every later node keeps appending text at the buffer's then-current end
//! (right gravity = "stay to the right of insertions at this exact
//! position"), so it ends up pinned to the very end of the document instead
//! of staying where it was created. Found the hard way in the rendering
//! spike — see the reading-pane centering/scroll notes in the rewrite plan.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk4::{prelude::*, TextBuffer, TextIter, TextMark, TextView};

pub struct NodeIndex {
    /// (node_id, mark), in the same ordinal order nodes were recorded in —
    /// i.e. non-decreasing buffer position.
    entries: Vec<(i64, TextMark)>,
    /// Same marks as `entries`, keyed for O(1) `mark_for_node` lookup —
    /// `entries` alone made that a linear scan, which `annotation_ui.rs`'s
    /// old `node_boundaries` helper called once per node on every rebuild,
    /// making it effectively O(n^2). That got rebuilt on every mouse-hover
    /// tooltip query, which is what made a verse-heavy book like the Poetic
    /// Edda (thousands of one-line nodes) noticeably laggier than an
    /// image-heavy one with far fewer nodes overall.
    by_id: HashMap<i64, TextMark>,
    /// Cached result of `boundaries()` — safe to cache indefinitely because
    /// nothing in this app inserts/deletes buffer text after the initial
    /// `build_document` pass (annotations only apply tags), so every mark's
    /// offset is fixed for the reader page's whole lifetime.
    boundaries_cache: RefCell<Option<Rc<Vec<i32>>>>,
}

impl NodeIndex {
    pub fn new() -> Self {
        Self { entries: Vec::new(), by_id: HashMap::new(), boundaries_cache: RefCell::new(None) }
    }

    /// Records `node_id` as starting at `iter`'s current position. Must be
    /// called *before* that node's content is inserted.
    pub fn record(&mut self, buffer: &TextBuffer, iter: &TextIter, node_id: i64) {
        let mark = buffer.create_mark(None, iter, true);
        self.entries.push((node_id, mark.clone()));
        self.by_id.insert(node_id, mark);
    }

    // Only exercised by #[cfg(test)] code today, which a plain `cargo build`
    // doesn't see — real (non-test) callers arrive in later phases (TOC
    // jump-to, annotation anchoring).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn mark_for_node(&self, node_id: i64) -> Option<&TextMark> {
        self.by_id.get(&node_id)
    }

    /// Every recorded node's buffer offset, in the same order nodes were
    /// recorded — parallel to the book's own `ast_nodes` list. Computed once
    /// and cached (see `boundaries_cache`'s doc comment for why that's
    /// sound); callers doing frequent per-event lookups (hover, click) get
    /// an `Rc` clone instead of re-walking every mark through GTK each time.
    pub fn boundaries(&self, buffer: &TextBuffer) -> Rc<Vec<i32>> {
        if let Some(cached) = self.boundaries_cache.borrow().as_ref() {
            return cached.clone();
        }
        let computed = Rc::new(self.entries.iter().map(|(_, mark)| buffer.iter_at_mark(mark).offset()).collect());
        *self.boundaries_cache.borrow_mut() = Some(Rc::clone(&computed));
        computed
    }

    /// The last recorded node whose mark's buffer offset is at or before the
    /// first visible line's offset — i.e. the topmost node currently on
    /// screen.
    pub fn topmost_visible_node_id(&self, buffer: &TextBuffer, text_view: &TextView) -> Option<i64> {
        let visible_top_y = text_view.visible_rect().y();
        let (top_iter, _line_top) = text_view.line_at_y(visible_top_y);
        let top_offset = top_iter.offset();

        self.entries
            .iter()
            .filter(|(_, mark)| buffer.iter_at_mark(mark).offset() <= top_offset)
            .next_back()
            .or_else(|| self.entries.first())
            .map(|(id, _)| *id)
    }

    /// 0.0-1.0 fraction through the book's content, by character count, for
    /// `node_id`'s position — mirrors the web reader's character-count-based
    /// progress, but computed directly against the buffer instead of needing
    /// the frontend to derive it separately.
    pub fn percent_through(&self, buffer: &TextBuffer, node_id: i64) -> Option<f64> {
        let mark = self.mark_for_node(node_id)?;
        let total = buffer.char_count();
        if total == 0 {
            return Some(0.0);
        }
        let offset = buffer.iter_at_mark(mark).offset();
        Some((offset as f64 / total as f64).clamp(0.0, 1.0))
    }

    #[cfg(test)]
    pub(crate) fn offsets(&self, buffer: &TextBuffer) -> Vec<i32> {
        self.entries.iter().map(|(_, mark)| buffer.iter_at_mark(mark).offset()).collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gtk4::TextBuffer;

    // Not a #[test] itself: GTK's main context is single-thread-only, but
    // libtest gives every #[test] fn its own OS thread regardless of
    // --test-threads, so every GTK-touching check in this crate runs from
    // one shared #[test] entry point (`tests::gtk_backed_checks` in
    // main.rs) instead of each independently claiming a thread and
    // colliding with "Attempted to initialize GTK from two different
    // threads."
    pub(crate) fn check_marks_stay_anchored_and_offsets_are_monotonic() {
        let buffer = TextBuffer::new(None);
        let mut index = NodeIndex::new();

        let mut iter = buffer.end_iter();
        index.record(&buffer, &iter, 1);
        buffer.insert(&mut iter, "first node text\n");

        index.record(&buffer, &iter, 2);
        buffer.insert(&mut iter, "second node text, quite a bit longer than the first\n");

        index.record(&buffer, &iter, 3);
        buffer.insert(&mut iter, "third\n");

        assert_eq!(index.len(), 3);
        assert!(!index.is_empty());

        let offsets = index.offsets(&buffer);
        assert_eq!(offsets[0], 0, "first mark must stay at the very start, not slide to the end");
        assert!(offsets[1] > offsets[0]);
        assert!(offsets[2] > offsets[1]);
        assert!(offsets[2] < buffer.char_count(), "last mark must stay before its own node's text, not at the buffer end");

        assert_eq!(index.percent_through(&buffer, 1), Some(0.0));
        let p2 = index.percent_through(&buffer, 2).unwrap();
        assert!(p2 > 0.0 && p2 < 1.0);
    }
}
