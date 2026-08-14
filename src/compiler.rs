use anyhow::{Context, Result};
use colored::*;
use rusqlite::{params, Connection};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

use crate::dom::{parse_chapter_html, resolve_footnote, sanitize_metadata_text, ChapterElement};
use crate::epub::{get_mime_type, get_parent_dir, resolve_relative_path, EpubArchive};
use crate::schema::init_db;

/// Compilation options.
#[derive(Debug, Clone, Default)]
pub struct CompileOptions {
    pub quiet: bool,
    pub verbose: bool,
}

/// Statistics returned after successful compilation.
#[derive(Debug, Clone, Default)]
pub struct CompileStats {
    pub title: String,
    pub author: String,
    pub chapter_count: usize,
    pub total_nodes: usize,
    pub node_counts: HashMap<String, usize>,
    pub asset_count: usize,
    pub total_asset_bytes: usize,
    pub toc_count: usize,
    pub output_file_size: u64,
    pub elapsed_millis: u128,
}

/// Compiles an EPUB file into a Weland (.wld) SQLite database.
pub fn compile_epub<P: AsRef<Path>, Q: AsRef<Path>>(
    input_path: P,
    output_path: Q,
    options: &CompileOptions,
) -> Result<CompileStats> {
    let start_time = Instant::now();
    let in_path = input_path.as_ref();
    let out_path = output_path.as_ref();

    if !options.quiet {
        println!("{}", format!("[WERG] Ingesting EPUB: {}", in_path.display()).cyan().bold());
    }

    // Reset output file if it exists
    if out_path.exists() {
        fs::remove_file(out_path)
            .with_context(|| format!("Failed to remove existing output file: {}", out_path.display()))?;
    }

    // Create and initialize database
    let mut conn = Connection::open(out_path)
        .with_context(|| format!("Failed to create SQLite database at: {}", out_path.display()))?;
    init_db(&conn)?;

    // Open and parse EPUB package
    let mut epub = EpubArchive::open(in_path)
        .with_context(|| format!("Failed to read EPUB file: {}", in_path.display()))?;

    let clean_title = sanitize_metadata_text(&epub.metadata.title);
    let clean_author = sanitize_metadata_text(&epub.metadata.author);

    // Prepare metadata insertions
    {
        let mut stmt_meta = conn.prepare_cached(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
        )?;

        stmt_meta.execute(params!["title", &clean_title])?;
        stmt_meta.execute(params!["author", &clean_author])?;
        stmt_meta.execute(params!["language", &epub.metadata.language])?;

        if let Some(ref desc) = epub.metadata.description {
            let clean_desc = sanitize_metadata_text(desc);
            if !clean_desc.is_empty() {
                stmt_meta.execute(params!["description", &clean_desc])?;
            }
        }
        if let Some(ref ident) = epub.metadata.identifier {
            stmt_meta.execute(params!["identifier", ident])?;
        }
        if let Some(ref publ) = epub.metadata.publisher {
            let clean_publ = sanitize_metadata_text(publ);
            if !clean_publ.is_empty() {
                stmt_meta.execute(params!["publisher", &clean_publ])?;
            }
        }
        if let Some(ref date) = epub.metadata.date {
            stmt_meta.execute(params!["date", date])?;
        }
        if let Some(ref rights) = epub.metadata.rights {
            let clean_rights = sanitize_metadata_text(rights);
            if !clean_rights.is_empty() {
                stmt_meta.execute(params!["rights", &clean_rights])?;
            }
        }
        stmt_meta.execute(params!["weland_version", env!("CARGO_PKG_VERSION")])?;
    }

    // Handle Cover Image Extraction
    let mut asset_id_map: HashMap<String, i64> = HashMap::new();
    let mut total_asset_bytes = 0usize;

    let cover_href_opt = epub.metadata.cover_href.clone();
    if let Some(ref cover_href) = cover_href_opt {
        if let Ok(cover_bytes) = epub.read_bytes(cover_href) {
            let mut hasher = Sha256::new();
            hasher.update(&cover_bytes);
            let hash_hex = hex::encode(hasher.finalize());
            let mime_type = get_mime_type(cover_href);

            total_asset_bytes += cover_bytes.len();

            let mut stmt_asset = conn.prepare_cached(
                "INSERT INTO assets (hash, mime_type, data) VALUES (?1, ?2, ?3)
                 ON CONFLICT(hash) DO UPDATE SET id=id RETURNING id",
            )?;

            let asset_id: i64 = stmt_asset.query_row(
                params![&hash_hex, &mime_type, &cover_bytes],
                |row| row.get(0),
            )?;

            asset_id_map.insert(hash_hex, asset_id);

            conn.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
                params!["cover_asset_id", asset_id.to_string()],
            )?;

            if !options.quiet && options.verbose {
                println!(
                    "{}",
                    format!("  + Extracted cover image ({} bytes, ID: {})", cover_bytes.len(), asset_id).dimmed()
                );
            }
        }
    }

    let spine_paths = epub.spine_paths.clone();
    let raw_toc = epub.extract_toc();

    if !options.quiet {
        println!(
            "{}",
            format!("[WERG] Extracted Metadata: \"{}\" by {}", clean_title, clean_author).green()
        );
        println!(
            "{}",
            format!("[WERG] Found {} chapters in reading spine.", spine_paths.len()).blue()
        );
        if !raw_toc.is_empty() {
            println!(
                "{}",
                format!("[WERG] Extracted Table of Contents ({} top-level entries).", raw_toc.len()).blue()
            );
        }
    }

    let mut ordinal: i64 = 0;
    let mut node_counts: HashMap<String, usize> = HashMap::new();
    let mut chapter_first_node_map: HashMap<String, i64> = HashMap::new();
    let mut element_id_map: HashMap<(String, String), i64> = HashMap::new();
    let mut heading_nodes: Vec<(i64, String)> = Vec::new();

    // Execute compilation in a single atomic transaction for maximum performance
    let tx = conn.transaction()?;

    let mut toc_entries_count = 0;

    {
        let mut stmt_node = tx.prepare_cached(
            "INSERT INTO ast_nodes (parent_id, ordinal, node_type, content, attributes)
             VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
        )?;

        let mut stmt_fts = tx.prepare_cached(
            "INSERT INTO fts_nodes (rowid, content) VALUES (?1, ?2)",
        )?;

        let mut stmt_asset = tx.prepare_cached(
            "INSERT INTO assets (hash, mime_type, data) VALUES (?1, ?2, ?3)
             ON CONFLICT(hash) DO UPDATE SET id=id RETURNING id",
        )?;

        for chapter_path in &spine_paths {
            let chapter_html = match epub.read_string(chapter_path) {
                Ok(content) => content,
                Err(err) => {
                    if options.verbose {
                        eprintln!("Warning: skipping unreadable spine chapter {}: {}", chapter_path, err);
                    }
                    continue;
                }
            };

            let chapter_dir = get_parent_dir(chapter_path);
            let (doc, elements) = parse_chapter_html(&chapter_html, chapter_path);

            for item in elements {
                match item {
                    ChapterElement::Heading {
                        level,
                        text,
                        spans,
                        element_id,
                    } => {
                        let attrs = json!({
                            "level": level,
                            "spans": spans
                        });

                        let node_id: i64 = stmt_node.query_row(
                            params![None::<i64>, ordinal, "heading", &text, attrs.to_string()],
                            |row| row.get(0),
                        )?;

                        stmt_fts.execute(params![node_id, &text])?;
                        *node_counts.entry("heading".to_string()).or_insert(0) += 1;

                        if !chapter_first_node_map.contains_key(chapter_path) {
                            chapter_first_node_map.insert(chapter_path.clone(), node_id);
                        }
                        if let Some(ref el_id) = element_id {
                            element_id_map.insert((chapter_path.clone(), el_id.clone()), node_id);
                        }
                        heading_nodes.push((node_id, text));
                        ordinal += 1;
                    }

                    ChapterElement::Paragraph {
                        node_type,
                        text,
                        spans,
                        source_file,
                        footnotes,
                        element_id,
                    } => {
                        let attrs = json!({
                            "source_file": source_file,
                            "spans": spans
                        });

                        let parent_node_id: i64 = stmt_node.query_row(
                            params![None::<i64>, ordinal, &node_type, &text, attrs.to_string()],
                            |row| row.get(0),
                        )?;

                        stmt_fts.execute(params![parent_node_id, &text])?;
                        *node_counts.entry(node_type.clone()).or_insert(0) += 1;

                        if !chapter_first_node_map.contains_key(chapter_path) {
                            chapter_first_node_map.insert(chapter_path.clone(), parent_node_id);
                        }
                        if let Some(ref el_id) = element_id {
                            element_id_map.insert((chapter_path.clone(), el_id.clone()), parent_node_id);
                        }
                        ordinal += 1;

                        // Insert resolved footnotes as children of this paragraph
                        for fn_ref in footnotes {
                            if let Some(resolved) = resolve_footnote(&doc, &fn_ref.anchor_id, &fn_ref.label) {
                                let fn_attrs = json!({
                                    "anchor_id": resolved.anchor_id,
                                    "label": resolved.label,
                                    "spans": resolved.spans
                                });

                                let fn_node_id: i64 = stmt_node.query_row(
                                    params![
                                        Some(parent_node_id),
                                        ordinal,
                                        "footnote",
                                        &resolved.text,
                                        fn_attrs.to_string()
                                    ],
                                    |row| row.get(0),
                                )?;

                                stmt_fts.execute(params![fn_node_id, &resolved.text])?;
                                *node_counts.entry("footnote".to_string()).or_insert(0) += 1;
                                ordinal += 1;
                            }
                        }
                    }

                    ChapterElement::ThematicBreak { element_id } => {
                        let attrs = json!({});
                        let node_id: i64 = stmt_node.query_row(
                            params![None::<i64>, ordinal, "thematic_break", None::<String>, attrs.to_string()],
                            |row| row.get(0),
                        )?;

                        *node_counts.entry("thematic_break".to_string()).or_insert(0) += 1;
                        if !chapter_first_node_map.contains_key(chapter_path) {
                            chapter_first_node_map.insert(chapter_path.clone(), node_id);
                        }
                        if let Some(ref el_id) = element_id {
                            element_id_map.insert((chapter_path.clone(), el_id.clone()), node_id);
                        }
                        ordinal += 1;
                    }

                    ChapterElement::Table { text, rows, source_file, element_id } => {
                        let attrs = json!({
                            "rows": rows,
                            "source_file": source_file
                        });

                        let node_id: i64 = stmt_node.query_row(
                            params![None::<i64>, ordinal, "table", &text, attrs.to_string()],
                            |row| row.get(0),
                        )?;

                        stmt_fts.execute(params![node_id, &text])?;
                        *node_counts.entry("table".to_string()).or_insert(0) += 1;

                        if !chapter_first_node_map.contains_key(chapter_path) {
                            chapter_first_node_map.insert(chapter_path.clone(), node_id);
                        }
                        if let Some(ref el_id) = element_id {
                            element_id_map.insert((chapter_path.clone(), el_id.clone()), node_id);
                        }
                        ordinal += 1;
                    }

                    ChapterElement::Image { src, alt, caption, element_id } => {
                        let full_img_path = resolve_relative_path(&chapter_dir, &src);
                        if let Ok(img_bytes) = epub.read_bytes(&full_img_path) {
                            let mut hasher = Sha256::new();
                            hasher.update(&img_bytes);
                            let hash_hex = hex::encode(hasher.finalize());
                            let mime_type = get_mime_type(&full_img_path);

                            let asset_id = match asset_id_map.get(&hash_hex) {
                                Some(&id) => id,
                                None => {
                                    total_asset_bytes += img_bytes.len();
                                    let id: i64 = stmt_asset.query_row(
                                        params![&hash_hex, &mime_type, &img_bytes],
                                        |row| row.get(0),
                                    )?;
                                    asset_id_map.insert(hash_hex, id);
                                    id
                                }
                            };

                            let attrs = json!({
                                "asset_id": asset_id,
                                "alt": alt,
                                "caption": caption
                            });

                            let node_id: i64 = stmt_node.query_row(
                                params![None::<i64>, ordinal, "image", None::<String>, attrs.to_string()],
                                |row| row.get(0),
                            )?;

                            *node_counts.entry("image".to_string()).or_insert(0) += 1;
                            if !chapter_first_node_map.contains_key(chapter_path) {
                                chapter_first_node_map.insert(chapter_path.clone(), node_id);
                            }
                            if let Some(ref el_id) = element_id {
                                element_id_map.insert((chapter_path.clone(), el_id.clone()), node_id);
                            }
                            ordinal += 1;
                        }
                    }
                }
            }
        }

        // ====================================================================
        // TABLE OF CONTENTS (TOC) POPULATION
        // ====================================================================
        let mut stmt_toc = tx.prepare_cached(
            "INSERT INTO table_of_contents (parent_id, ordinal, title, target_node_id, href)
             VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
        )?;

        let mut toc_ordinal: i64 = 0;

        if !raw_toc.is_empty() {
            toc_entries_count = insert_toc_hierarchy(
                &mut stmt_toc,
                &raw_toc,
                None,
                &mut toc_ordinal,
                &chapter_first_node_map,
                &element_id_map,
            )?;
        } else if !heading_nodes.is_empty() {
            // Fallback: Generate TOC entries from compiled headings
            for (node_id, heading_text) in heading_nodes {
                let _: i64 = stmt_toc.query_row(
                    params![None::<i64>, toc_ordinal, &heading_text, Some(node_id), None::<String>],
                    |row| row.get(0),
                )?;
                toc_ordinal += 1;
                toc_entries_count += 1;
            }
        }
    }

    tx.commit()?;

    // ====================================================================
    // FINALIZE & CONVERT DATABASE TO PORTABLE STANDALONE MODE
    // ====================================================================
    // 1. Flush all WAL log pages back into the main .wld database file
    let _ = conn.execute("PRAGMA wal_checkpoint(TRUNCATE);", []);
    // 2. Switch journal_mode to DELETE so the file is a clean standalone binary
    //    without sidecar files, allowing it to open in read-only filesystems.
    let _: Result<String, _> = conn.query_row("PRAGMA journal_mode = DELETE;", [], |r| r.get(0));
    // 3. Optimize query planner statistics
    let _ = conn.execute("ANALYZE;", []);
    let _ = conn.execute("PRAGMA optimize;", []);

    conn.close().map_err(|(_, err)| err)?;

    let elapsed = start_time.elapsed().as_millis();
    let out_file_size = fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);

    if !options.quiet {
        println!(
            "{}",
            format!(
                "[WERG] Successfully compiled database: {} ({:.2} KB, {} AST nodes, {} assets, {} TOC entries in {} ms)",
                out_path.display(),
                out_file_size as f64 / 1024.0,
                ordinal,
                asset_id_map.len(),
                toc_entries_count,
                elapsed
            )
            .green()
            .bold()
        );
    }

    Ok(CompileStats {
        title: clean_title,
        author: clean_author,
        chapter_count: epub.spine_paths.len(),
        total_nodes: ordinal as usize,
        node_counts,
        asset_count: asset_id_map.len(),
        total_asset_bytes,
        toc_count: toc_entries_count,
        output_file_size: out_file_size,
        elapsed_millis: elapsed,
    })
}

fn insert_toc_hierarchy(
    stmt_toc: &mut rusqlite::CachedStatement,
    items: &[crate::epub::RawTocItem],
    parent_id: Option<i64>,
    ordinal_counter: &mut i64,
    chapter_first_node_map: &HashMap<String, i64>,
    element_id_map: &HashMap<(String, String), i64>,
) -> Result<usize> {
    let mut count = 0;

    for item in items {
        let mut target_node_id = None;

        if !item.href.is_empty() {
            let parts: Vec<&str> = item.href.split('#').collect();
            let file_part = crate::epub::normalize_zip_path(parts[0]);

            if parts.len() > 1 && !parts[1].is_empty() {
                let frag = parts[1].to_string();
                target_node_id = element_id_map.get(&(file_part.clone(), frag)).copied();
            }

            if target_node_id.is_none() {
                target_node_id = chapter_first_node_map.get(&file_part).copied();
            }
        }

        let toc_id: i64 = stmt_toc.query_row(
            params![
                parent_id,
                *ordinal_counter,
                &item.title,
                target_node_id,
                if item.href.is_empty() { None } else { Some(&item.href) }
            ],
            |row| row.get(0),
        )?;

        *ordinal_counter += 1;
        count += 1;

        if !item.children.is_empty() {
            count += insert_toc_hierarchy(
                stmt_toc,
                &item.children,
                Some(toc_id),
                ordinal_counter,
                chapter_first_node_map,
                element_id_map,
            )?;
        }
    }

    Ok(count)
}
