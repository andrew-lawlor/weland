use rusqlite::Connection;
use tempfile::TempDir;

use weland::compiler::{compile_epub, CompileOptions};
use weland::db::{self, NewAnnotation};

mod common;
use common::create_test_epub;

fn compile_fixture() -> (TempDir, std::path::PathBuf) {
    let temp_dir = TempDir::new().unwrap();
    let epub_path = temp_dir.path().join("test_book.epub");
    let wld_path = temp_dir.path().join("test_book.wld");
    create_test_epub(&epub_path);
    compile_epub(&epub_path, &wld_path, &CompileOptions { quiet: true, verbose: false })
        .expect("Compilation failed");
    (temp_dir, wld_path)
}

#[test]
fn test_load_metadata_and_toc() {
    let (_temp_dir, wld_path) = compile_fixture();
    let conn = Connection::open(&wld_path).unwrap();

    let meta = db::load_metadata(&conn).unwrap();
    assert_eq!(meta.get("title").unwrap(), "The Principles of Weland");
    assert_eq!(meta.get("author").unwrap(), "Jane Doe");

    let toc = db::load_toc(&conn).unwrap();
    assert_eq!(toc.len(), 2);
    assert_eq!(toc[0].title, "1. Introduction to Weland");
    assert_eq!(toc[1].title, "2. Deep Dive: Performance");
    assert!(toc[0].target_node_id.is_some());
}

#[test]
fn test_load_ast_nodes_matches_content() {
    let (_temp_dir, wld_path) = compile_fixture();
    let conn = Connection::open(&wld_path).unwrap();

    let nodes = db::load_ast_nodes(&conn).unwrap();
    assert!(!nodes.is_empty());

    // Ordinal order is preserved.
    for (idx, node) in nodes.iter().enumerate() {
        assert_eq!(node.ordinal, idx as i64);
    }

    let heading = nodes
        .iter()
        .find(|n| n.node_type == "heading" && n.content.as_deref() == Some("Introduction to Weland"))
        .expect("Heading node should be present");
    assert_eq!(heading.attributes.as_ref().unwrap()["level"], 1);
}

#[test]
fn test_load_annotations_starts_empty() {
    let (_temp_dir, wld_path) = compile_fixture();
    let conn = Connection::open(&wld_path).unwrap();
    assert!(db::load_annotations(&conn).unwrap().is_empty());
}

#[test]
fn test_search_nodes_returns_snippet() {
    let (_temp_dir, wld_path) = compile_fixture();
    let conn = Connection::open(&wld_path).unwrap();

    let hits = db::search_nodes(&conn, "digital", 5).unwrap();
    assert!(!hits.is_empty());
    assert!(hits[0].snippet.to_lowercase().contains("digital"));

    let none = db::search_nodes(&conn, "zzznomatchzzz", 5).unwrap();
    assert!(none.is_empty());
}

#[test]
fn test_insert_and_load_highlight_annotation() {
    let (_temp_dir, wld_path) = compile_fixture();
    let conn = Connection::open(&wld_path).unwrap();
    let node_id = db::load_ast_nodes(&conn).unwrap()[0].id;

    let created = db::insert_annotation(
        &conn,
        NewAnnotation {
            node_id,
            start_offset: 0,
            end_offset: 5,
            selected_text: Some("Intro".to_string()),
            annotation_type: "highlight".to_string(),
            comment: None,
            asset_id: None,
            author_name: "Local Reader".to_string(),
        },
    )
    .unwrap();

    assert!(created.id > 0);
    assert_eq!(created.node_id, node_id);
    assert_eq!(created.annotation_type, "highlight");
    assert_eq!(created.selected_text.as_deref(), Some("Intro"));
    assert!(!created.created_at.is_empty());

    let all = db::load_annotations(&conn).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, created.id);
}

#[test]
fn test_insert_text_note_with_comment() {
    let (_temp_dir, wld_path) = compile_fixture();
    let conn = Connection::open(&wld_path).unwrap();
    let node_id = db::load_ast_nodes(&conn).unwrap()[0].id;

    let created = db::insert_annotation(
        &conn,
        NewAnnotation {
            node_id,
            start_offset: 4,
            end_offset: 10,
            selected_text: Some("future".to_string()),
            annotation_type: "text_note".to_string(),
            comment: Some("This is the whole pitch.".to_string()),
            asset_id: None,
            author_name: "Local Reader".to_string(),
        },
    )
    .unwrap();

    assert_eq!(created.annotation_type, "text_note");
    assert_eq!(created.comment.as_deref(), Some("This is the whole pitch."));
}

#[test]
fn test_insert_voice_asset_dedupes_and_links_to_annotation() {
    let (_temp_dir, wld_path) = compile_fixture();
    let conn = Connection::open(&wld_path).unwrap();
    let node_id = db::load_ast_nodes(&conn).unwrap()[0].id;

    let clip = vec![0x52, 0x49, 0x46, 0x46, 0x01, 0x02, 0x03, 0x04]; // fake RIFF/WAV bytes
    let asset_id_1 = db::insert_voice_asset(&conn, "audio/wav", &clip).unwrap();
    let asset_id_2 = db::insert_voice_asset(&conn, "audio/wav", &clip).unwrap();
    assert_eq!(asset_id_1, asset_id_2, "Identical audio bytes should dedupe to the same asset id");

    let (mime, data) = db::load_asset(&conn, asset_id_1).unwrap();
    assert_eq!(mime, "audio/wav");
    assert_eq!(data, clip);

    let created = db::insert_annotation(
        &conn,
        NewAnnotation {
            node_id,
            start_offset: 0,
            end_offset: 20,
            selected_text: Some("welund answers it structurally.".to_string()),
            annotation_type: "voice_note".to_string(),
            comment: None,
            asset_id: Some(asset_id_1),
            author_name: "Local Reader".to_string(),
        },
    )
    .unwrap();

    assert_eq!(created.annotation_type, "voice_note");
    assert_eq!(created.asset_id, Some(asset_id_1));
}

#[test]
fn test_load_asset_missing_id_errors() {
    let (_temp_dir, wld_path) = compile_fixture();
    let conn = Connection::open(&wld_path).unwrap();
    assert!(db::load_asset(&conn, 99999).is_err());
}
