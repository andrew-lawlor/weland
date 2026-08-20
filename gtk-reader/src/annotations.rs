//! Annotation data layer: a `node_id -> [UserAnnotation]` lookup built from
//! `db::load_annotations`, so the future annotation UI (Phase 7) can check
//! "does this node already have a highlight/note?" without a linear scan.
//!
//! `db.rs` already implements every create/read/update/delete operation
//! needed here — nothing GTK-specific, nothing new to write against SQLite.
//! This phase exists to prove that layer correct end-to-end against a real
//! compiled `.wld` before Phase 7 spends effort on the much harder
//! text-selection/popover UI on top of it.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::Connection;
use weland::db;
use weland::schema::UserAnnotation;

// Not wired into any UI yet — the annotation UI arrives in Phase 7.
#[allow(dead_code)]
pub struct AnnotationIndex {
    by_node: HashMap<i64, Vec<UserAnnotation>>,
}

#[allow(dead_code)]
impl AnnotationIndex {
    pub fn load(conn: &Connection) -> Result<Self> {
        let annotations = db::load_annotations(conn)?;
        let mut by_node: HashMap<i64, Vec<UserAnnotation>> = HashMap::new();
        for annotation in annotations {
            by_node.entry(annotation.node_id).or_default().push(annotation);
        }
        Ok(Self { by_node })
    }

    pub fn for_node(&self, node_id: i64) -> &[UserAnnotation] {
        self.by_node.get(&node_id).map(Vec::as_slice).unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OpenFlags;
    use tempfile::tempdir;

    const SCRATCH_BOOK: &str =
        "/tmp/claude-1000/-home-andrew-Documents-Rust-weland/839cf43e-b477-43ff-8379-19470349a793/scratchpad/books/robin-hood.wld";

    /// A throwaway copy of a real compiled `.wld` — annotation mutations
    /// must never run against the shared scratch fixture other tests read.
    fn scratch_copy() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("book.wld");
        std::fs::copy(SCRATCH_BOOK, &path)
            .unwrap_or_else(|e| panic!("copy scratch fixture {SCRATCH_BOOK}: {e} (run `cargo test` once to recompile fixtures first)"));
        (dir, path)
    }

    fn open_rw(path: &std::path::Path) -> Connection {
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap()
    }

    fn new_highlight(node_id: i64) -> db::NewAnnotation {
        db::NewAnnotation {
            node_id,
            start_offset: 0,
            end_offset: 12,
            selected_text: Some("Robin Hood".to_string()),
            annotation_type: "highlight".to_string(),
            comment: None,
            asset_id: None,
            author_name: "Test Reader".to_string(),
        }
    }

    fn new_text_note(node_id: i64, comment: &str) -> db::NewAnnotation {
        db::NewAnnotation {
            node_id,
            start_offset: 20,
            end_offset: 30,
            selected_text: Some("outlaw".to_string()),
            annotation_type: "text_note".to_string(),
            comment: Some(comment.to_string()),
            asset_id: None,
            author_name: "Test Reader".to_string(),
        }
    }

    #[test]
    fn create_and_index_groups_by_node() {
        let (_dir, path) = scratch_copy();
        let conn = open_rw(&path);

        db::insert_annotation(&conn, new_highlight(10)).unwrap();
        db::insert_annotation(&conn, new_text_note(10, "first note")).unwrap();

        let voice_asset_id = db::insert_voice_asset(&conn, "audio/ogg", b"fake opus bytes").unwrap();
        let voice_note = db::insert_annotation(
            &conn,
            db::NewAnnotation {
                node_id: 42,
                start_offset: 0,
                end_offset: 0,
                selected_text: None,
                annotation_type: "voice_note".to_string(),
                comment: None,
                asset_id: Some(voice_asset_id),
                author_name: "Test Reader".to_string(),
            },
        )
        .unwrap();

        let index = AnnotationIndex::load(&conn).unwrap();
        assert_eq!(index.for_node(10).len(), 2, "both annotations on node 10 must be grouped together");
        assert_eq!(index.for_node(42).len(), 1);
        assert!(index.for_node(999_999).is_empty(), "a node with no annotations must return an empty slice, not panic");
        assert_eq!(index.for_node(42)[0].asset_id, Some(voice_asset_id));
        assert_eq!(voice_note.annotation_type, "voice_note");
    }

    #[test]
    fn update_comment_leaves_anchor_and_type_untouched() {
        let (_dir, path) = scratch_copy();
        let conn = open_rw(&path);

        let note = db::insert_annotation(&conn, new_text_note(10, "first note")).unwrap();
        let updated = db::update_annotation_comment(&conn, note.id, "revised note").unwrap();

        assert_eq!(updated.comment.as_deref(), Some("revised note"));
        assert_eq!(updated.node_id, note.node_id);
        assert_eq!(updated.start_offset, note.start_offset);
        assert_eq!(updated.end_offset, note.end_offset);
        assert_eq!(updated.annotation_type, "text_note");
    }

    #[test]
    fn update_comment_on_missing_id_errors() {
        let (_dir, path) = scratch_copy();
        let conn = open_rw(&path);
        let err = db::update_annotation_comment(&conn, 999_999, "x").unwrap_err();
        assert!(err.to_string().contains("999999"));
    }

    #[test]
    fn delete_removes_only_the_targeted_annotation() {
        let (_dir, path) = scratch_copy();
        let conn = open_rw(&path);

        let highlight = db::insert_annotation(&conn, new_highlight(10)).unwrap();
        db::insert_annotation(&conn, new_text_note(10, "keep me")).unwrap();

        db::delete_annotation(&conn, highlight.id).unwrap();

        let index = AnnotationIndex::load(&conn).unwrap();
        let remaining = index.for_node(10);
        assert_eq!(remaining.len(), 1, "deleting the highlight must leave the text note in place");
        assert_eq!(remaining[0].annotation_type, "text_note");
    }

    #[test]
    fn delete_missing_id_errors() {
        let (_dir, path) = scratch_copy();
        let conn = open_rw(&path);
        let err = db::delete_annotation(&conn, 999_999).unwrap_err();
        assert!(err.to_string().contains("999999"));
    }
}
