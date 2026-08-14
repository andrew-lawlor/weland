use rusqlite::Connection;
use std::fs::File;
use std::io::Write;
use tempfile::TempDir;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use weland::compiler::{compile_epub, CompileOptions};
use weland::dom::extract_text_and_spans;
use weland::toolkit::{export_wld, extract_assets, inspect_wld, search_wld, ExportFormat};

mod common;
use common::create_test_epub;

#[test]
fn test_span_extraction_and_whitespace_normalization() {
    let html = r#"<p>   Hello,   <em>world</em>!  Here is   <strong>bold text</strong> and <a href="https://rust-lang.org">Rust</a>.   </p>"#;
    let doc = scraper::Html::parse_fragment(html);
    let p_sel = scraper::Selector::parse("p").unwrap();
    let p_elem = doc.select(&p_sel).next().unwrap();

    let res = extract_text_and_spans(p_elem);

    assert_eq!(res.text, "Hello, world! Here is bold text and Rust.");

    // Check spans
    let em_span = res.spans.iter().find(|s| s.span_type == "italic").unwrap();
    let bold_span = res.spans.iter().find(|s| s.span_type == "bold").unwrap();
    let link_span = res.spans.iter().find(|s| s.span_type == "link").unwrap();

    // Verify exact slice match
    let chars: Vec<char> = res.text.chars().collect();
    let em_text: String = chars[em_span.start..em_span.end].iter().collect();
    let bold_text: String = chars[bold_span.start..bold_span.end].iter().collect();
    let link_text: String = chars[link_span.start..link_span.end].iter().collect();

    assert_eq!(em_text, "world");
    assert_eq!(bold_text, "bold text");
    assert_eq!(link_text, "Rust");
    assert_eq!(link_span.href.as_deref(), Some("https://rust-lang.org"));
}

#[test]
fn test_end_to_end_epub_compilation() {
    let temp_dir = TempDir::new().unwrap();
    let epub_path = temp_dir.path().join("test_book.epub");
    let wld_path = temp_dir.path().join("test_book.wld");

    create_test_epub(&epub_path);

    let options = CompileOptions {
        quiet: true,
        verbose: false,
    };

    let stats = compile_epub(&epub_path, &wld_path, &options).expect("Compilation failed");

    assert_eq!(stats.title, "The Principles of Weland");
    assert_eq!(stats.author, "Jane Doe");
    assert_eq!(stats.chapter_count, 2);
    assert!(stats.total_nodes > 0);
    assert_eq!(stats.asset_count, 2); // cover + diagram

    // Verify SQLite Database Contents
    let conn = Connection::open(&wld_path).expect("Failed to open compiled .wld database");

    // 1. Verify Metadata
    let mut stmt = conn.prepare("SELECT key, value FROM metadata").unwrap();
    let meta_rows = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .unwrap()
        .collect::<Result<std::collections::HashMap<_, _>, _>>()
        .unwrap();

    assert_eq!(meta_rows.get("title").unwrap(), "The Principles of Weland");
    assert_eq!(meta_rows.get("author").unwrap(), "Jane Doe");
    assert_eq!(meta_rows.get("language").unwrap(), "en");
    assert!(meta_rows.contains_key("cover_asset_id"));

    // 2. Verify AST Nodes
    let mut stmt = conn
        .prepare("SELECT ordinal, node_type, content, attributes, parent_id FROM ast_nodes ORDER BY ordinal ASC")
        .unwrap();

    let ast_rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    // Verify ordinals are strictly monotonic starting at 0
    for (idx, row) in ast_rows.iter().enumerate() {
        assert_eq!(row.0, idx as i64);
    }

    // Verify node types presence
    let node_types: Vec<&str> = ast_rows.iter().map(|r| r.1.as_str()).collect();
    assert!(node_types.contains(&"heading"));
    assert!(node_types.contains(&"paragraph"));
    assert!(node_types.contains(&"thematic_break"));
    assert!(node_types.contains(&"blockquote"));
    assert!(node_types.contains(&"list"));
    assert!(node_types.contains(&"table"));
    assert!(node_types.contains(&"image"));
    assert!(node_types.contains(&"footnote"));

    // Verify footnote parent_id link
    let footnote_node = ast_rows.iter().find(|r| r.1 == "footnote").unwrap();
    assert!(footnote_node.4.is_some(), "Footnote must have a parent_id");

    // 3. Verify FTS5 Search Index
    let mut fts_stmt = conn
        .prepare("SELECT rowid, content FROM fts_nodes WHERE fts_nodes MATCH ?1")
        .unwrap();
    let fts_matches = fts_stmt
        .query_map(["Weland"], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert!(!fts_matches.is_empty(), "FTS5 should find occurrences of 'Weland'");

    // 4. Verify Table of Contents
    let mut toc_stmt = conn
        .prepare("SELECT ordinal, title, target_node_id, href FROM table_of_contents ORDER BY ordinal ASC")
        .unwrap();
    let toc_rows = toc_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(toc_rows.len(), 2, "TOC should contain 2 entries");
    assert_eq!(toc_rows[0].1, "1. Introduction to Weland");
    assert_eq!(toc_rows[1].1, "2. Deep Dive: Performance");
    assert!(toc_rows[0].2.is_some(), "TOC entry must point to target AST node ID");
    assert!(toc_rows[1].2.is_some(), "TOC entry must point to target AST node ID");

    // 5. Test Toolkit Commands
    inspect_wld(&wld_path).expect("Inspect should succeed");
    search_wld(&wld_path, "digital", 5).expect("Search should succeed");

    let extract_dir = temp_dir.path().join("extracted");
    extract_assets(&wld_path, &extract_dir, false).expect("Extract should succeed");
    assert!(extract_dir.join("cover.png").exists() || extract_dir.read_dir().unwrap().count() >= 2);

    let md_out = temp_dir.path().join("book.md");
    export_wld(&wld_path, ExportFormat::Markdown, Some(&md_out)).expect("Export MD should succeed");
    let md_content = std::fs::read_to_string(&md_out).unwrap();
    assert!(md_content.contains("# The Principles of Weland"));
    assert!(md_content.contains("*by Jane Doe*"));
}

#[test]
fn test_multibyte_unicode_and_nested_spans() {
    let html = r#"<p>  Café ☕ <strong><em>délicieux</em></strong> et <code>let x = "🦀";</code>!  </p>"#;
    let doc = scraper::Html::parse_fragment(html);
    let p_elem = doc.select(&scraper::Selector::parse("p").unwrap()).next().unwrap();

    let res = extract_text_and_spans(p_elem);

    assert_eq!(res.text, "Café ☕ délicieux et let x = \"🦀\";!");

    let chars: Vec<char> = res.text.chars().collect();

    // Check italic span inside bold
    let em_span = res.spans.iter().find(|s| s.span_type == "italic").unwrap();
    let bold_span = res.spans.iter().find(|s| s.span_type == "bold").unwrap();
    let code_span = res.spans.iter().find(|s| s.span_type == "code").unwrap();

    let em_text: String = chars[em_span.start..em_span.end].iter().collect();
    let bold_text: String = chars[bold_span.start..bold_span.end].iter().collect();
    let code_text: String = chars[code_span.start..code_span.end].iter().collect();

    assert_eq!(em_text, "délicieux");
    assert_eq!(bold_text, "délicieux");
    assert_eq!(code_text, "let x = \"🦀\";");
}

#[test]
fn test_asset_deduplication() {
    let temp_dir = TempDir::new().unwrap();
    let epub_path = temp_dir.path().join("dedup_book.epub");
    let wld_path = temp_dir.path().join("dedup_book.wld");

    let file = File::create(&epub_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let deflated_opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("mimetype", SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();

    zip.start_file("META-INF/container.xml", deflated_opts).unwrap();
    zip.write_all(br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#).unwrap();

    zip.start_file("content.opf", deflated_opts).unwrap();
    zip.write_all(br#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Asset Dedup Test</dc:title>
    <dc:creator>Author</dc:creator>
    <meta name="cover" content="shared-img"/>
  </metadata>
  <manifest>
    <item id="shared-img" href="img.png" media-type="image/png"/>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="ch2.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
    <itemref idref="ch2"/>
  </spine>
</package>"#).unwrap();

    // The shared image
    zip.start_file("img.png", deflated_opts).unwrap();
    zip.write_all(b"UNIQUE_IMAGE_CONTENT_BYTES_123456").unwrap();

    zip.start_file("ch1.xhtml", deflated_opts).unwrap();
    zip.write_all(br#"<html><body><img src="img.png" alt="First use"/></body></html>"#).unwrap();

    zip.start_file("ch2.xhtml", deflated_opts).unwrap();
    zip.write_all(br#"<html><body><img src="img.png" alt="Second use"/></body></html>"#).unwrap();

    zip.finish().unwrap();

    let stats = compile_epub(&epub_path, &wld_path, &CompileOptions { quiet: true, verbose: false }).unwrap();

    // Cover + 2 chapters all point to the SAME single image file
    // Therefore, assets table MUST contain exactly 1 asset!
    assert_eq!(stats.asset_count, 1, "Duplicate assets must be deduplicated by SHA-256");

    let conn = Connection::open(&wld_path).unwrap();
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM assets", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 1);
}

#[test]
fn test_epub2_ncx_hierarchical_toc() {
    let temp_dir = TempDir::new().unwrap();
    let epub_path = temp_dir.path().join("epub2_toc.epub");
    let wld_path = temp_dir.path().join("epub2_toc.wld");

    let file = File::create(&epub_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let deflated_opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("mimetype", SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();

    zip.start_file("META-INF/container.xml", deflated_opts).unwrap();
    zip.write_all(br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#).unwrap();

    zip.start_file("OEBPS/content.opf", deflated_opts).unwrap();
    zip.write_all(br#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>EPUB 2 NCX Test</dc:title>
    <dc:creator>Author</dc:creator>
  </metadata>
  <manifest>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="ch1"/>
  </spine>
</package>"#).unwrap();

    // toc.ncx with hierarchical navPoints
    zip.start_file("OEBPS/toc.ncx", deflated_opts).unwrap();
    zip.write_all(br#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <navMap>
    <navPoint id="np-1" playOrder="1">
      <navLabel><text>Chapter 1: Foundations</text></navLabel>
      <content src="ch1.xhtml#chap1"/>
      <navPoint id="np-1-1" playOrder="2">
        <navLabel><text>1.1 Architecture</text></navLabel>
        <content src="ch1.xhtml#arch"/>
      </navPoint>
      <navPoint id="np-1-2" playOrder="3">
        <navLabel><text>1.2 Storage Model</text></navLabel>
        <content src="ch1.xhtml#storage"/>
      </navPoint>
    </navPoint>
  </navMap>
</ncx>"#).unwrap();

    zip.start_file("OEBPS/ch1.xhtml", deflated_opts).unwrap();
    zip.write_all(br#"<!DOCTYPE html>
<html>
<body>
  <h1 id="chap1">Chapter 1: Foundations</h1>
  <p>Overview of the system.</p>
  <h2 id="arch">1.1 Architecture</h2>
  <p>System components and interfaces.</p>
  <h2 id="storage">1.2 Storage Model</h2>
  <p>SQLite AST representation.</p>
</body>
</html>"#).unwrap();

    zip.finish().unwrap();

    let stats = compile_epub(&epub_path, &wld_path, &CompileOptions { quiet: true, verbose: false }).unwrap();
    assert_eq!(stats.toc_count, 3, "TOC should contain 3 hierarchical items");

    let conn = Connection::open(&wld_path).unwrap();

    let mut stmt = conn.prepare("SELECT id, parent_id, ordinal, title, target_node_id FROM table_of_contents ORDER BY ordinal ASC").unwrap();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<i64>>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<i64>>(4)?,
        ))
    }).unwrap().collect::<Result<Vec<_>, _>>().unwrap();

    assert_eq!(rows.len(), 3);

    // Root item
    let root_id = rows[0].0;
    assert_eq!(rows[0].1, None, "Root TOC item must have no parent_id");
    assert_eq!(rows[0].3, "Chapter 1: Foundations");
    assert!(rows[0].4.is_some(), "Root item must point to target node");

    // Child 1
    assert_eq!(rows[1].1, Some(root_id), "Child 1 must have parent_id pointing to root");
    assert_eq!(rows[1].3, "1.1 Architecture");
    assert!(rows[1].4.is_some(), "Child 1 must point to target node");

    // Child 2
    assert_eq!(rows[2].1, Some(root_id), "Child 2 must have parent_id pointing to root");
    assert_eq!(rows[2].3, "1.2 Storage Model");
    assert!(rows[2].4.is_some(), "Child 2 must point to target node");

    // Verify target node IDs are all distinct
    assert_ne!(rows[0].4, rows[1].4);
    assert_ne!(rows[1].4, rows[2].4);
}

#[test]
fn test_metadata_html_sanitization_and_no_wal_sidecars() {
    let temp_dir = TempDir::new().unwrap();
    let epub_path = temp_dir.path().join("html_meta.epub");
    let wld_path = temp_dir.path().join("html_meta.wld");

    let file = File::create(&epub_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let deflated_opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("mimetype", SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();

    zip.start_file("META-INF/container.xml", deflated_opts).unwrap();
    zip.write_all(br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#).unwrap();

    zip.start_file("content.opf", deflated_opts).unwrap();
    zip.write_all(br#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title><b>The</b> <i>Weland</i> Guide &amp; Reference</dc:title>
    <dc:creator><span class="author">Alice &amp; Bob</span></dc:creator>
    <dc:description><p>This is paragraph 1 of the description.</p><p>This is paragraph 2 with <b>bold text</b> and &lt;encoded markup&gt;.</p></dc:description>
  </metadata>
  <manifest>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
  </spine>
</package>"#).unwrap();

    zip.start_file("ch1.xhtml", deflated_opts).unwrap();
    zip.write_all(br#"<html><body><h1>Hello World</h1><p>Test content.</p></body></html>"#).unwrap();

    zip.finish().unwrap();

    compile_epub(&epub_path, &wld_path, &CompileOptions { quiet: true, verbose: false }).unwrap();

    // 1. Verify that no -wal or -shm sidecar files exist on disk
    let wal_file = temp_dir.path().join("html_meta.wld-wal");
    let shm_file = temp_dir.path().join("html_meta.wld-shm");
    assert!(!wal_file.exists(), "No .wld-wal file should remain on disk");
    assert!(!shm_file.exists(), "No .wld-shm file should remain on disk");

    // 2. Open database and verify journal_mode is DELETE (or not WAL)
    let conn = Connection::open(&wld_path).unwrap();
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(mode.to_lowercase(), "delete", "Database should be in DELETE journal mode");

    // 3. Verify HTML tags were stripped from metadata
    let title: String = conn.query_row("SELECT value FROM metadata WHERE key = 'title'", [], |r| r.get(0)).unwrap();
    let author: String = conn.query_row("SELECT value FROM metadata WHERE key = 'author'", [], |r| r.get(0)).unwrap();
    let description: String = conn.query_row("SELECT value FROM metadata WHERE key = 'description'", [], |r| r.get(0)).unwrap();

    assert_eq!(title, "The Weland Guide & Reference");
    assert_eq!(author, "Alice & Bob");
    assert!(!description.contains("<p>"), "Description must not contain raw HTML tags");
    assert!(!description.contains("<b>"), "Description must not contain raw HTML tags");
    assert!(description.contains("This is paragraph 1 of the description."));
    assert!(description.contains("This is paragraph 2 with bold text and <encoded markup>."));
}



