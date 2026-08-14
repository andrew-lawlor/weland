use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// The SQLite database schema for the Weland (.wld) ebook standard.
pub const INIT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS metadata (
  key TEXT PRIMARY KEY,
  value TEXT
);

CREATE TABLE IF NOT EXISTS ast_nodes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  parent_id INTEGER REFERENCES ast_nodes(id) ON DELETE CASCADE,
  ordinal INTEGER NOT NULL,
  node_type TEXT NOT NULL,
  content TEXT,
  attributes JSON
);

CREATE TABLE IF NOT EXISTS assets (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  hash TEXT UNIQUE NOT NULL,
  mime_type TEXT NOT NULL,
  data BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS user_annotations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  node_id INTEGER NOT NULL REFERENCES ast_nodes(id) ON DELETE CASCADE,

  -- Precise character selection bounds in node.content
  start_offset INTEGER NOT NULL,
  end_offset INTEGER NOT NULL,
  selected_text TEXT,

  -- Annotation Type: 'highlight', 'text_note', 'voice_note', 'ink_sketch'
  type TEXT NOT NULL,

  -- Payload fields
  comment TEXT,                               -- Textual note or voice transcript
  asset_id INTEGER REFERENCES assets(id),     -- Linked BLOB (voice note audio, SVG ink vector)

  -- Provenance & Metadata
  author_name TEXT DEFAULT 'Local Reader',
  author_id TEXT,                             -- UUID or PubKey
  device_id TEXT,                             -- Optional client hardware identifier
  created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
  updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

CREATE VIRTUAL TABLE IF NOT EXISTS fts_nodes USING fts5(
  content,
  content='ast_nodes',
  content_rowid='id'
);

CREATE TABLE IF NOT EXISTS table_of_contents (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  parent_id INTEGER REFERENCES table_of_contents(id) ON DELETE CASCADE,
  ordinal INTEGER NOT NULL,
  title TEXT NOT NULL,
  target_node_id INTEGER REFERENCES ast_nodes(id) ON DELETE SET NULL,
  href TEXT
);

-- Performance indexes for rapid tree navigation and ordinal iteration
CREATE INDEX IF NOT EXISTS idx_ast_nodes_parent ON ast_nodes(parent_id);
CREATE INDEX IF NOT EXISTS idx_ast_nodes_ordinal ON ast_nodes(ordinal);
CREATE INDEX IF NOT EXISTS idx_ast_nodes_type ON ast_nodes(node_type);
CREATE INDEX IF NOT EXISTS idx_user_annotations_node ON user_annotations(node_id);
CREATE INDEX IF NOT EXISTS idx_toc_parent ON table_of_contents(parent_id);
CREATE INDEX IF NOT EXISTS idx_toc_ordinal ON table_of_contents(ordinal);
CREATE INDEX IF NOT EXISTS idx_toc_target ON table_of_contents(target_node_id);
"#;

/// Initializes a SQLite connection with high-performance PRAGMAs and creates the Weland schema.
pub fn init_db(conn: &Connection) -> Result<()> {
    // journal_mode returns a row with the new mode name, so we use query_row
    let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.execute("PRAGMA synchronous = NORMAL", [])?;
    conn.execute("PRAGMA foreign_keys = ON", [])?;
    conn.execute("PRAGMA temp_store = MEMORY", [])?;
    conn.execute("PRAGMA cache_size = -64000", [])?;

    conn.execute_batch(INIT_SCHEMA)?;
    Ok(())
}

/// Represents an inline formatting span attached to an AST node's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    #[serde(rename = "type")]
    pub span_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
}

/// Represents an AST node stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AstNode {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub ordinal: i64,
    pub node_type: String,
    pub content: Option<String>,
    pub attributes: Option<serde_json::Value>,
}

/// Represents an asset stored in the database.
#[derive(Debug, Clone)]
pub struct Asset {
    pub id: i64,
    pub hash: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

/// Represents a user annotation on an AST node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserAnnotation {
    pub id: i64,
    pub node_id: i64,
    pub start_offset: i64,
    pub end_offset: i64,
    pub selected_text: Option<String>,
    pub annotation_type: String,
    pub comment: Option<String>,
    pub asset_id: Option<i64>,
    pub author_name: String,
    pub author_id: Option<String>,
    pub device_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Represents a table of contents entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TocEntry {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub ordinal: i64,
    pub title: String,
    pub target_node_id: Option<i64>,
    pub href: Option<String>,
}

