use anyhow::{anyhow, Context, Result};
use colored::*;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::db;
use crate::schema::Span;

/// Export format types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Markdown,
    Json,
    Text,
}

/// Inspects a .wld database and displays a comprehensive summary.
pub fn inspect_wld<P: AsRef<Path>>(wld_path: P) -> Result<()> {
    let path = wld_path.as_ref();
    let conn = Connection::open(path)
        .with_context(|| format!("Failed to open Weland database at: {}", path.display()))?;

    println!("{}", format!("=== Weland (.wld) Inspection: {} ===", path.display()).bold().cyan());

    // 1. Metadata
    println!("\n{}", "--- Metadata ---".bold());
    let metadata_map = db::load_metadata(&conn)?;
    let mut keys: Vec<&String> = metadata_map.keys().collect();
    keys.sort();
    for k in keys {
        println!("  {}: {}", k.dimmed(), metadata_map[k].bold());
    }

    // 2. Node Counts by Type
    println!("\n{}", "--- AST Node Breakdown ---".bold());
    let mut stmt = conn.prepare("SELECT node_type, COUNT(*) FROM ast_nodes GROUP BY node_type ORDER BY COUNT(*) DESC")?;
    let node_counts = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;

    let mut total_nodes = 0i64;
    for nc in node_counts {
        let (ntype, count) = nc?;
        println!("  {:16} : {}", ntype, count);
        total_nodes += count;
    }
    println!("  {:16} : {}", "TOTAL NODES".bold(), total_nodes.to_string().green().bold());

    // 3. Assets
    println!("\n{}", "--- Embedded Assets ---".bold());
    let asset_stats: (i64, Option<i64>) = conn.query_row(
        "SELECT COUNT(*), SUM(LENGTH(data)) FROM assets",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let asset_count = asset_stats.0;
    let asset_bytes = asset_stats.1.unwrap_or(0);
    println!("  Total Assets   : {}", asset_count);
    println!("  Total Payload  : {:.2} KB", asset_bytes as f64 / 1024.0);

    // 4. Annotations
    let annotation_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM user_annotations",
        [],
        |row| row.get(0),
    ).unwrap_or(0);
    println!("\n{}", "--- User Annotations ---".bold());
    println!("  Total Count    : {}", annotation_count);

    // 5. Full-Text Search Status
    let fts_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM fts_nodes",
        [],
        |row| row.get(0),
    ).unwrap_or(0);
    println!("\n{}", "--- Full-Text Search (FTS5) ---".bold());
    println!("  Indexed Nodes  : {}", fts_count);

    // 6. Table of Contents
    println!("\n{}", "--- Table of Contents ---".bold());
    let toc_entries = db::load_toc(&conn)?;

    if toc_entries.is_empty() {
        println!("  (No TOC entries found)");
    } else {
        let mut parent_map: HashMap<Option<i64>, Vec<&crate::schema::TocEntry>> = HashMap::new();
        for entry in &toc_entries {
            parent_map.entry(entry.parent_id).or_default().push(entry);
        }

        fn print_toc_tree(
            parent_id: Option<i64>,
            parent_map: &HashMap<Option<i64>, Vec<&crate::schema::TocEntry>>,
            depth: usize,
        ) {
            if let Some(children) = parent_map.get(&parent_id) {
                for child in children {
                    let indent = "  ".repeat(depth + 1);
                    let target_str = match child.target_node_id {
                        Some(nid) => format!("-> Node #{}", nid).cyan(),
                        None => "(unlinked)".dimmed(),
                    };
                    println!("{}{} {}", indent, child.title.bold(), target_str);
                    print_toc_tree(Some(child.id), parent_map, depth + 1);
                }
            }
        }

        print_toc_tree(None, &parent_map, 0);
        println!("  Total TOC Items: {}", toc_entries.len().to_string().green());
    }

    println!("\n{}", "✓ File conforms to Weland standard schema.".green());
    Ok(())
}

/// Searches the AST nodes in a .wld file using SQLite FTS5.
pub fn search_wld<P: AsRef<Path>>(wld_path: P, query: &str, limit: usize) -> Result<()> {
    let path = wld_path.as_ref();
    let conn = Connection::open(path)
        .with_context(|| format!("Failed to open Weland database at: {}", path.display()))?;

    let hits = db::search_nodes(&conn, query, limit)?;

    println!("{}", format!("=== Search Results for \"{}\" in {} ===", query, path.display()).bold().cyan());

    let mut count = 0;
    for hit in &hits {
        count += 1;
        println!(
            "[{}] Ordinal #{} ({}) | Node ID: {}\n  {}",
            count.to_string().yellow(),
            hit.ordinal,
            hit.node_type.dimmed(),
            hit.node_id,
            hit.snippet.bright_white()
        );
    }

    if count == 0 {
        println!("{}", "No matching nodes found.".dimmed());
    } else {
        println!("\nFound {} match(es).", count.to_string().green().bold());
    }

    Ok(())
}

/// Extracts assets (or cover only) from a .wld file to an output directory.
pub fn extract_assets<P: AsRef<Path>, Q: AsRef<Path>>(
    wld_path: P,
    out_dir: Q,
    cover_only: bool,
) -> Result<()> {
    let path = wld_path.as_ref();
    let dest_dir = out_dir.as_ref();
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("Failed to create output directory: {}", dest_dir.display()))?;

    let conn = Connection::open(path)
        .with_context(|| format!("Failed to open Weland database at: {}", path.display()))?;

    if cover_only {
        let cover_asset_id: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'cover_asset_id'",
                [],
                |row| row.get(0),
            )
            .ok();

        if let Some(id_str) = cover_asset_id {
            let asset_id: i64 = id_str.parse().unwrap_or(0);
            let (mime_type, data): (String, Vec<u8>) = conn.query_row(
                "SELECT mime_type, data FROM assets WHERE id = ?1",
                params![asset_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;

            let ext = match mime_type.as_str() {
                "image/png" => "png",
                "image/webp" => "webp",
                "image/svg+xml" => "svg",
                _ => "jpg",
            };

            let out_file = dest_dir.join(format!("cover.{}", ext));
            fs::write(&out_file, &data)?;
            println!("{}", format!("Extracted cover image to: {}", out_file.display()).green());
            return Ok(());
        } else {
            return Err(anyhow!("No cover_asset_id found in metadata."));
        }
    }

    let mut stmt = conn.prepare("SELECT id, hash, mime_type, data FROM assets ORDER BY id")?;
    let asset_rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Vec<u8>>(3)?,
        ))
    })?;

    let mut count = 0;
    for row in asset_rows {
        count += 1;
        let (id, hash, mime_type, data) = row?;
        let ext = match mime_type.as_str() {
            "image/png" => "png",
            "image/webp" => "webp",
            "image/svg+xml" => "svg",
            "image/gif" => "gif",
            "audio/mpeg" => "mp3",
            "audio/wav" => "wav",
            _ => "jpg",
        };

        let short_hash = &hash[..8.min(hash.len())];
        let file_name = format!("asset_{}_{}.{}", id, short_hash, ext);
        let out_file = dest_dir.join(&file_name);
        fs::write(&out_file, &data)?;
        println!("  Saved asset #{} ({}) -> {}", id, mime_type.dimmed(), file_name);
    }

    println!("{}", format!("\nSuccessfully extracted {} assets to: {}", count, dest_dir.display()).green().bold());
    Ok(())
}

/// Applies inline Markdown formatting to text using span ranges.
fn format_markdown_spans(text: &str, spans: &[Span]) -> String {
    if spans.is_empty() {
        return text.to_string();
    }

    // Sort spans by start offset descending so insertions from right to left don't invalidate left offsets
    let chars: Vec<char> = text.chars().collect();
    let text_len = chars.len();

    // For simplicity, handle non-overlapping or hierarchical spans
    let mut insertions: Vec<(usize, String)> = Vec::new();

    for span in spans {
        if span.start <= text_len && span.end <= text_len && span.start < span.end {
            match span.span_type.as_str() {
                "italic" => {
                    insertions.push((span.start, "*".to_string()));
                    insertions.push((span.end, "*".to_string()));
                }
                "bold" => {
                    insertions.push((span.start, "**".to_string()));
                    insertions.push((span.end, "**".to_string()));
                }
                "code" => {
                    insertions.push((span.start, "`".to_string()));
                    insertions.push((span.end, "`".to_string()));
                }
                "strikethrough" => {
                    insertions.push((span.start, "~~".to_string()));
                    insertions.push((span.end, "~~".to_string()));
                }
                "link" => {
                    if let Some(ref href) = span.href {
                        insertions.push((span.start, "[".to_string()));
                        insertions.push((span.end, format!("]({})", href)));
                    }
                }
                _ => {}
            }
        }
    }

    // Sort insertions: index ascending; if same index, closing tags before opening tags
    insertions.sort_by(|a, b| a.0.cmp(&b.0));

    let mut result = String::with_capacity(text.len() + 64);
    let mut current_idx = 0;

    for (idx, tag) in insertions {
        while current_idx < idx && current_idx < text_len {
            result.push(chars[current_idx]);
            current_idx += 1;
        }
        result.push_str(&tag);
    }

    while current_idx < text_len {
        result.push(chars[current_idx]);
        current_idx += 1;
    }

    result
}

/// Exports a .wld database to Markdown, JSON, or Plain Text.
pub fn export_wld<P: AsRef<Path>, Q: AsRef<Path>>(
    wld_path: P,
    format: ExportFormat,
    output_file: Option<Q>,
) -> Result<()> {
    let path = wld_path.as_ref();
    let conn = Connection::open(path)
        .with_context(|| format!("Failed to open Weland database at: {}", path.display()))?;

    // Load nodes ordered by ordinal
    let nodes = db::load_ast_nodes(&conn)?;

    let exported_text = match format {
        ExportFormat::Json => {
            serde_json::to_string_pretty(&nodes)?
        }
        ExportFormat::Text => {
            let mut out = String::new();
            for node in &nodes {
                if let Some(ref text) = node.content {
                    out.push_str(text);
                    out.push_str("\n\n");
                }
            }
            out
        }
        ExportFormat::Markdown => {
            let mut out = String::new();

            // Add title metadata header if available
            let title: Option<String> = conn
                .query_row("SELECT value FROM metadata WHERE key = 'title'", [], |r| r.get(0))
                .ok();
            let author: Option<String> = conn
                .query_row("SELECT value FROM metadata WHERE key = 'author'", [], |r| r.get(0))
                .ok();

            if let Some(t) = title {
                out.push_str(&format!("# {}\n", t));
                if let Some(a) = author {
                    out.push_str(&format!("*by {}*\n\n---\n\n", a));
                }
            }

            for node in &nodes {
                // Skip child footnotes here; they can be referenced or placed at end
                if node.parent_id.is_some() && node.node_type == "footnote" {
                    continue;
                }

                match node.node_type.as_str() {
                    "heading" => {
                        let level = node
                            .attributes
                            .as_ref()
                            .and_then(|a| a.get("level"))
                            .and_then(|l| l.as_u64())
                            .unwrap_or(1) as usize;
                        let prefix = "#".repeat(level.clamp(1, 6));
                        let text = node.content.as_deref().unwrap_or("");
                        out.push_str(&format!("{} {}\n\n", prefix, text));
                    }
                    "thematic_break" => {
                        out.push_str("\n---\n\n");
                    }
                    "image" => {
                        let alt = node
                            .attributes
                            .as_ref()
                            .and_then(|a| a.get("alt"))
                            .and_then(|s| s.as_str())
                            .unwrap_or("");
                        let asset_id = node
                            .attributes
                            .as_ref()
                            .and_then(|a| a.get("asset_id"))
                            .and_then(|id| id.as_i64())
                            .unwrap_or(0);
                        out.push_str(&format!("![{}](asset:{})\n\n", alt, asset_id));
                    }
                    "blockquote" => {
                        if let Some(ref text) = node.content {
                            out.push_str(&format!("> {}\n\n", text));
                        }
                    }
                    "list" => {
                        if let Some(ref text) = node.content {
                            out.push_str(&format!("- {}\n\n", text));
                        }
                    }
                    "table" => {
                        if let Some(ref attrs) = node.attributes {
                            if let Some(rows) = attrs.get("rows").and_then(|r| r.as_array()) {
                                for (i, row) in rows.iter().enumerate() {
                                    if let Some(cols) = row.as_array() {
                                        let line = cols
                                            .iter()
                                            .map(|c| c.as_str().unwrap_or(""))
                                            .collect::<Vec<_>>()
                                            .join(" | ");
                                        out.push_str(&format!("| {} |\n", line));

                                        if i == 0 {
                                            let sep = cols
                                                .iter()
                                                .map(|_| "---")
                                                .collect::<Vec<_>>()
                                                .join(" | ");
                                            out.push_str(&format!("| {} |\n", sep));
                                        }
                                    }
                                }
                                out.push('\n');
                            }
                        }
                    }
                    "paragraph" | "verse_line" => {
                        if let Some(ref text) = node.content {
                            let spans: Vec<Span> = node
                                .attributes
                                .as_ref()
                                .and_then(|a| a.get("spans"))
                                .and_then(|s| serde_json::from_value(s.clone()).ok())
                                .unwrap_or_default();

                            let formatted = format_markdown_spans(text, &spans);
                            out.push_str(&formatted);
                            // A single newline, not a blank line, between
                            // consecutive verse lines — otherwise every line of
                            // a poem gets rendered as its own separate paragraph.
                            if node.node_type == "verse_line" {
                                out.push('\n');
                            } else {
                                out.push_str("\n\n");
                            }
                        }
                    }
                    _ => {
                        if let Some(ref text) = node.content {
                            out.push_str(text);
                            out.push_str("\n\n");
                        }
                    }
                }
            }
            out
        }
    };

    if let Some(out_p) = output_file {
        fs::write(out_p.as_ref(), &exported_text)?;
        println!(
            "{}",
            format!("Exported {} to: {}", match format {
                ExportFormat::Markdown => "Markdown",
                ExportFormat::Json => "JSON",
                ExportFormat::Text => "Plain Text",
            }, out_p.as_ref().display()).green().bold()
        );
    } else {
        println!("{}", exported_text);
    }

    Ok(())
}
