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

use gtk4::{prelude::*, TextBuffer, TextIter, TextMark, TextView};

pub struct NodeIndex {
    /// (node_id, mark), in the same ordinal order nodes were recorded in —
    /// i.e. non-decreasing buffer position.
    entries: Vec<(i64, TextMark)>,
}

impl NodeIndex {
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// Records `node_id` as starting at `iter`'s current position. Must be
    /// called *before* that node's content is inserted.
    pub fn record(&mut self, buffer: &TextBuffer, iter: &TextIter, node_id: i64) {
        let mark = buffer.create_mark(None, iter, true);
        self.entries.push((node_id, mark));
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
        self.entries.iter().find(|(id, _)| *id == node_id).map(|(_, mark)| mark)
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
