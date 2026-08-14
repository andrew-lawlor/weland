use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use crate::schema::{AstNode, TocEntry, UserAnnotation};

/// A single FTS5 search hit against `ast_nodes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub node_id: i64,
    pub ordinal: i64,
    pub node_type: String,
    pub snippet: String,
}

/// Parameters for creating a new annotation row.
#[derive(Debug, Clone)]
pub struct NewAnnotation {
    pub node_id: i64,
    pub start_offset: i64,
    pub end_offset: i64,
    pub selected_text: Option<String>,
    pub annotation_type: String,
    pub comment: Option<String>,
    pub asset_id: Option<i64>,
    pub author_name: String,
}

/// Loads the flat metadata key/value table.
pub fn load_metadata(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt = conn.prepare("SELECT key, value FROM metadata")?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    rows.collect::<std::result::Result<HashMap<_, _>, _>>()
        .context("Failed to load metadata")
}

/// Loads the table of contents, ordered for stable tree reconstruction.
pub fn load_toc(conn: &Connection) -> Result<Vec<TocEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, ordinal, title, target_node_id, href
         FROM table_of_contents ORDER BY ordinal ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(TocEntry {
            id: row.get(0)?,
            parent_id: row.get(1)?,
            ordinal: row.get(2)?,
            title: row.get(3)?,
            target_node_id: row.get(4)?,
            href: row.get(5)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("Failed to load table of contents")
}

/// Loads every AST node in reading order.
pub fn load_ast_nodes(conn: &Connection) -> Result<Vec<AstNode>> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, ordinal, node_type, content, attributes
         FROM ast_nodes ORDER BY ordinal ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        let attr_str: Option<String> = row.get(5)?;
        let attr_val = attr_str.and_then(|s| serde_json::from_str::<Value>(&s).ok());
        Ok(AstNode {
            id: row.get(0)?,
            parent_id: row.get(1)?,
            ordinal: row.get(2)?,
            node_type: row.get(3)?,
            content: row.get(4)?,
            attributes: attr_val,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("Failed to load AST nodes")
}

/// Loads every user annotation.
pub fn load_annotations(conn: &Connection) -> Result<Vec<UserAnnotation>> {
    let mut stmt = conn.prepare(
        "SELECT id, node_id, start_offset, end_offset, selected_text, type, comment,
                asset_id, author_name, author_id, device_id, created_at, updated_at
         FROM user_annotations ORDER BY id ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(UserAnnotation {
            id: row.get(0)?,
            node_id: row.get(1)?,
            start_offset: row.get(2)?,
            end_offset: row.get(3)?,
            selected_text: row.get(4)?,
            annotation_type: row.get(5)?,
            comment: row.get(6)?,
            asset_id: row.get(7)?,
            author_name: row.get(8)?,
            author_id: row.get(9)?,
            device_id: row.get(10)?,
            created_at: row.get(11)?,
            updated_at: row.get(12)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("Failed to load annotations")
}

/// Runs an FTS5 query against `fts_nodes`, returning ranked snippet hits.
pub fn search_nodes(conn: &Connection, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.ordinal, a.node_type, snippet(fts_nodes, 0, '«', '»', '...', 12) as snip
         FROM fts_nodes
         JOIN ast_nodes a ON a.id = fts_nodes.rowid
         WHERE fts_nodes MATCH ?1
         ORDER BY rank
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![query, limit as i64], |row| {
        Ok(SearchHit {
            node_id: row.get(0)?,
            ordinal: row.get(1)?,
            node_type: row.get(2)?,
            snippet: row.get(3)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("Search query failed")
}

/// Loads a single asset's MIME type and bytes by id.
pub fn load_asset(conn: &Connection, asset_id: i64) -> Result<(String, Vec<u8>)> {
    conn.query_row(
        "SELECT mime_type, data FROM assets WHERE id = ?1",
        params![asset_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .with_context(|| format!("Failed to load asset #{asset_id}"))
}

/// Inserts a new annotation row and returns it fully populated (including
/// server-assigned id and timestamps).
pub fn insert_annotation(conn: &Connection, new: NewAnnotation) -> Result<UserAnnotation> {
    conn.execute(
        "INSERT INTO user_annotations
            (node_id, start_offset, end_offset, selected_text, type, comment, asset_id, author_name)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            new.node_id,
            new.start_offset,
            new.end_offset,
            new.selected_text,
            new.annotation_type,
            new.comment,
            new.asset_id,
            new.author_name,
        ],
    )
    .context("Failed to insert annotation")?;

    let id = conn.last_insert_rowid();
    conn.query_row(
        "SELECT id, node_id, start_offset, end_offset, selected_text, type, comment,
                asset_id, author_name, author_id, device_id, created_at, updated_at
         FROM user_annotations WHERE id = ?1",
        params![id],
        |row| {
            Ok(UserAnnotation {
                id: row.get(0)?,
                node_id: row.get(1)?,
                start_offset: row.get(2)?,
                end_offset: row.get(3)?,
                selected_text: row.get(4)?,
                annotation_type: row.get(5)?,
                comment: row.get(6)?,
                asset_id: row.get(7)?,
                author_name: row.get(8)?,
                author_id: row.get(9)?,
                device_id: row.get(10)?,
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
            })
        },
    )
    .context("Failed to read back inserted annotation")
}

/// Updates a text note's comment in place and returns the row as it now
/// stands. Only the comment and `updated_at` change — the anchor
/// (node/offsets/selected_text) and type are left alone.
pub fn update_annotation_comment(conn: &Connection, id: i64, comment: &str) -> Result<UserAnnotation> {
    let changed = conn.execute(
        "UPDATE user_annotations SET comment = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
        params![comment, id],
    )
    .context("Failed to update annotation comment")?;
    if changed == 0 {
        anyhow::bail!("No annotation with id {id}");
    }

    conn.query_row(
        "SELECT id, node_id, start_offset, end_offset, selected_text, type, comment,
                asset_id, author_name, author_id, device_id, created_at, updated_at
         FROM user_annotations WHERE id = ?1",
        params![id],
        |row| {
            Ok(UserAnnotation {
                id: row.get(0)?,
                node_id: row.get(1)?,
                start_offset: row.get(2)?,
                end_offset: row.get(3)?,
                selected_text: row.get(4)?,
                annotation_type: row.get(5)?,
                comment: row.get(6)?,
                asset_id: row.get(7)?,
                author_name: row.get(8)?,
                author_id: row.get(9)?,
                device_id: row.get(10)?,
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
            })
        },
    )
    .context("Failed to read back updated annotation")
}

/// Deletes an annotation by id. Does not delete a linked voice-note asset
/// (assets are content-hash deduped and may be referenced elsewhere).
pub fn delete_annotation(conn: &Connection, id: i64) -> Result<()> {
    let changed = conn
        .execute("DELETE FROM user_annotations WHERE id = ?1", params![id])
        .context("Failed to delete annotation")?;
    if changed == 0 {
        anyhow::bail!("No annotation with id {id}");
    }
    Ok(())
}

/// Inserts (or dedupes, by content hash) a binary asset and returns its id —
/// same hash + `ON CONFLICT` idiom the compiler uses for embedded images, so a
/// re-recorded identical clip doesn't create a duplicate BLOB.
pub fn insert_voice_asset(conn: &Connection, mime_type: &str, data: &[u8]) -> Result<i64> {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let hash_hex = hex::encode(hasher.finalize());

    conn.query_row(
        "INSERT INTO assets (hash, mime_type, data) VALUES (?1, ?2, ?3)
         ON CONFLICT(hash) DO UPDATE SET id=id RETURNING id",
        params![&hash_hex, mime_type, data],
        |row| row.get(0),
    )
    .context("Failed to insert voice note asset")
}
