use std::fs::File;
use std::io::Write;
use std::process::Command;
use tempfile::TempDir;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use weland::compiler::{compile_epub, CompileOptions};
use weland::toolkit::{export_wld, extract_assets, ExportFormat};

mod common;
use common::{create_test_epub, COVER_PNG_BYTES, DIAGRAM_PNG_BYTES};

/// Compiles the shared fixture epub and returns (temp_dir, wld_path).
fn compile_fixture() -> (TempDir, std::path::PathBuf) {
    let temp_dir = TempDir::new().unwrap();
    let epub_path = temp_dir.path().join("test_book.epub");
    let wld_path = temp_dir.path().join("test_book.wld");
    create_test_epub(&epub_path);
    compile_epub(&epub_path, &wld_path, &CompileOptions { quiet: true, verbose: false })
        .expect("Compilation failed");
    (temp_dir, wld_path)
}

/// Builds a minimal valid epub with no cover image at all.
fn create_coverless_epub(file_path: &std::path::Path) {
    let file = File::create(file_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let stored_opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let deflated_opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("mimetype", stored_opts).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();

    zip.start_file("META-INF/container.xml", deflated_opts).unwrap();
    zip.write_all(br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#).unwrap();

    zip.start_file("content.opf", deflated_opts).unwrap();
    zip.write_all(br#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>No Cover Here</dc:title>
    <dc:creator>Anonymous</dc:creator>
  </metadata>
  <manifest>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
  </spine>
</package>"#).unwrap();

    zip.start_file("ch1.xhtml", deflated_opts).unwrap();
    zip.write_all(br#"<html><body><h1>Hello</h1><p>No pictures in this one.</p></body></html>"#).unwrap();

    zip.finish().unwrap();
}

#[test]
fn test_extract_all_assets_writes_correct_bytes() {
    let (temp_dir, wld_path) = compile_fixture();
    let extract_dir = temp_dir.path().join("extracted");

    extract_assets(&wld_path, &extract_dir, false).expect("Extract should succeed");

    let entries: Vec<_> = std::fs::read_dir(&extract_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 2, "Should extract exactly the cover + diagram assets");

    let contents: Vec<Vec<u8>> = entries.iter().map(|p| std::fs::read(p).unwrap()).collect();
    assert!(contents.iter().any(|c| c == COVER_PNG_BYTES), "Cover bytes should round-trip exactly");
    assert!(contents.iter().any(|c| c == DIAGRAM_PNG_BYTES), "Diagram bytes should round-trip exactly");

    for entry in &entries {
        let name = entry.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("asset_") && name.ends_with(".png"), "Unexpected asset file name: {name}");
    }
}

#[test]
fn test_extract_cover_only_writes_single_cover_file() {
    let (temp_dir, wld_path) = compile_fixture();
    let extract_dir = temp_dir.path().join("cover_only");

    extract_assets(&wld_path, &extract_dir, true).expect("Cover-only extract should succeed");

    let entries: Vec<_> = std::fs::read_dir(&extract_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1, "Cover-only extract should write exactly one file");
    assert_eq!(entries[0].file_name().unwrap().to_string_lossy(), "cover.png");
    assert_eq!(std::fs::read(&entries[0]).unwrap(), COVER_PNG_BYTES);
}

#[test]
fn test_extract_cover_only_errors_without_cover() {
    let temp_dir = TempDir::new().unwrap();
    let epub_path = temp_dir.path().join("no_cover.epub");
    let wld_path = temp_dir.path().join("no_cover.wld");
    create_coverless_epub(&epub_path);
    compile_epub(&epub_path, &wld_path, &CompileOptions { quiet: true, verbose: false }).unwrap();

    let extract_dir = temp_dir.path().join("out");
    let result = extract_assets(&wld_path, &extract_dir, true);
    assert!(result.is_err(), "Cover-only extract should fail when the book has no cover");
}

#[test]
fn test_export_json_structure_and_spans() {
    let (temp_dir, wld_path) = compile_fixture();
    let json_out = temp_dir.path().join("book.json");

    export_wld(&wld_path, ExportFormat::Json, Some(&json_out)).expect("JSON export should succeed");
    let raw = std::fs::read_to_string(&json_out).unwrap();
    let nodes: serde_json::Value = serde_json::from_str(&raw).expect("Export should be valid JSON");
    let nodes = nodes.as_array().expect("Top-level JSON should be an array of nodes");
    assert!(!nodes.is_empty());

    // Heading node carries its level in attributes.
    let heading = nodes
        .iter()
        .find(|n| n["node_type"] == "heading" && n["content"] == "Introduction to Weland")
        .expect("Heading node should be present");
    assert_eq!(heading["attributes"]["level"], 1);

    // Paragraph node carries character-offset spans for its inline formatting.
    let paragraph = nodes
        .iter()
        .find(|n| n["content"].as_str().is_some_and(|c| c.contains("digital publishing")))
        .expect("Formatted paragraph node should be present");
    let text = paragraph["content"].as_str().unwrap();
    let chars: Vec<char> = text.chars().collect();
    let spans = paragraph["attributes"]["spans"].as_array().expect("Paragraph should have spans");

    let slice = |start: usize, end: usize| -> String { chars[start..end].iter().collect() };

    let italic = spans.iter().find(|s| s["type"] == "italic").expect("italic span missing");
    assert_eq!(slice(italic["start"].as_u64().unwrap() as usize, italic["end"].as_u64().unwrap() as usize), "future");

    let bold = spans.iter().find(|s| s["type"] == "bold").expect("bold span missing");
    assert_eq!(
        slice(bold["start"].as_u64().unwrap() as usize, bold["end"].as_u64().unwrap() as usize),
        "digital publishing"
    );

    let link = spans.iter().find(|s| s["type"] == "link").expect("link span missing");
    assert_eq!(
        slice(link["start"].as_u64().unwrap() as usize, link["end"].as_u64().unwrap() as usize),
        "open standards"
    );
    assert_eq!(link["href"], "https://example.com");

    // Image node has no text content; its data lives entirely in attributes.
    let image = nodes.iter().find(|n| n["node_type"] == "image").expect("Image node should be present");
    assert!(image["content"].is_null());
    assert_eq!(image["attributes"]["alt"], "Architecture Diagram");
    assert!(image["attributes"]["asset_id"].as_i64().unwrap() > 0);
}

#[test]
fn test_export_text_is_plain_no_markup() {
    let (temp_dir, wld_path) = compile_fixture();
    let text_out = temp_dir.path().join("book.txt");

    export_wld(&wld_path, ExportFormat::Text, Some(&text_out)).expect("Text export should succeed");
    let content = std::fs::read_to_string(&text_out).unwrap();

    assert!(content.contains("Introduction to Weland"));
    assert!(content.contains("digital publishing"));
    assert!(content.contains("Knowledge is power in the digital age."));
    assert!(content.contains("Fast random access"));

    // Plain text export must carry no Markdown decoration (content itself may
    // legitimately contain a literal '*', e.g. the SQL wildcard in the code sample).
    assert!(!content.contains("*future*"), "Text export should not apply Markdown italics");
    assert!(!content.contains("**digital publishing**"), "Text export should not apply Markdown bold");
    assert!(!content.contains('#'), "Text export should not contain Markdown heading markers");
    assert!(!content.contains("!["), "Text export should not contain Markdown image syntax");

    // Image nodes have no `content`, so their alt text is not present in plain text export.
    assert!(!content.contains("Architecture Diagram"));
}

#[test]
fn test_export_markdown_formatting() {
    let (temp_dir, wld_path) = compile_fixture();
    let md_out = temp_dir.path().join("book.md");

    export_wld(&wld_path, ExportFormat::Markdown, Some(&md_out)).expect("Markdown export should succeed");
    let content = std::fs::read_to_string(&md_out).unwrap();

    assert!(content.contains("# The Principles of Weland"));
    assert!(content.contains("*by Jane Doe*"));

    // Headings retain their level.
    assert!(content.contains("# Introduction to Weland"));
    assert!(content.contains("## Deep Dive: Performance"));

    // Inline spans render as Markdown formatting.
    assert!(content.contains("*future*"));
    assert!(content.contains("**digital publishing**"));
    assert!(content.contains("[open standards](https://example.com)"));
    assert!(content.contains("`SELECT * FROM ast_nodes;`"));

    // Structural elements.
    assert!(content.contains("> Knowledge is power in the digital age."));
    assert!(content.contains("- ") && content.contains("Fast random access"));
    assert!(content.contains("| Feature | Status |"));
    assert!(content.contains("| --- | --- |"));
    assert!(content.contains("| AST Storage | Active |"));
    assert!(content.contains("![Architecture Diagram](asset:"));
}

#[test]
fn test_search_cli_reports_matches_and_no_matches() {
    let (_temp_dir, wld_path) = compile_fixture();

    let output = Command::new(env!("CARGO_BIN_EXE_weland"))
        .args(["search", wld_path.to_str().unwrap(), "digital", "--limit", "5"])
        .output()
        .expect("Failed to run weland search");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Search Results"));
    assert!(stdout.contains("Found"));
    assert!(stdout.contains("match"));

    let output = Command::new(env!("CARGO_BIN_EXE_weland"))
        .args(["search", wld_path.to_str().unwrap(), "zzznomatchzzz", "--limit", "5"])
        .output()
        .expect("Failed to run weland search");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No matching nodes found."));
}

#[test]
fn test_inspect_cli_reports_summary() {
    let (_temp_dir, wld_path) = compile_fixture();

    let output = Command::new(env!("CARGO_BIN_EXE_weland"))
        .args(["inspect", wld_path.to_str().unwrap()])
        .output()
        .expect("Failed to run weland inspect");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("Weland (.wld) Inspection"));
    assert!(stdout.contains("The Principles of Weland"));
    assert!(stdout.contains("Jane Doe"));
    assert!(stdout.contains("TOTAL NODES"));
    assert!(stdout.contains("Total Assets"));
    assert!(stdout.contains("1. Introduction to Weland"));
    assert!(stdout.contains("2. Deep Dive: Performance"));
    assert!(stdout.contains("File conforms to Weland standard schema."));
}
