//! Settings and library persistence — ported near-verbatim from
//! `reader/src-tauri/src/commands.rs`, with `tauri::AppHandle::path()` calls
//! replaced by plain directory parameters so this stays pure, GTK-free, and
//! directly testable against a temp dir.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// All fields optional so an older settings.json (or one only ever touched by
/// one group of fields) still parses fine, and a read-modify-write of one
/// group of fields never clobbers another written concurrently elsewhere.
///
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Settings {
    #[serde(default)]
    pub author_name: Option<String>,
    #[serde(default)]
    pub reading_font: Option<String>,
    #[serde(default)]
    pub reading_size_px: Option<f64>,
    #[serde(default)]
    pub reading_leading: Option<f64>,
    #[serde(default)]
    pub reading_verse_spacing: Option<f64>,
    #[serde(default)]
    pub reading_show_verse_numbers: Option<bool>,
}

fn settings_path(config_dir: &Path) -> PathBuf {
    config_dir.join("settings.json")
}

pub fn read_settings(config_dir: &Path) -> Settings {
    fs::read_to_string(settings_path(config_dir))
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

pub fn write_settings(config_dir: &Path, settings: &Settings) -> Result<()> {
    fs::create_dir_all(config_dir).context("Failed to create config dir")?;
    let data = serde_json::to_string_pretty(settings).context("Failed to serialize settings")?;
    fs::write(settings_path(config_dir), data).context("Failed to write settings.json")
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct LibraryEntry {
    pub path: String,
    pub title: String,
    pub author: Option<String>,
    pub added_at: i64,
    pub last_opened_at: i64,
    #[serde(default)]
    pub last_position_node_id: Option<i64>,
    // 0.0-1.0 fraction through the book's content, by character count up to
    // last_position_node_id.
    #[serde(default)]
    pub last_position_percent: Option<f64>,
}

fn library_path(config_dir: &Path) -> PathBuf {
    config_dir.join("library.json")
}

pub fn read_library(config_dir: &Path) -> Result<Vec<LibraryEntry>> {
    match fs::read_to_string(library_path(config_dir)) {
        Ok(data) => Ok(serde_json::from_str(&data).unwrap_or_default()),
        Err(_) => Ok(Vec::new()),
    }
}

pub fn write_library(config_dir: &Path, entries: &[LibraryEntry]) -> Result<()> {
    fs::create_dir_all(config_dir).context("Failed to create config dir")?;
    let data = serde_json::to_string_pretty(entries).context("Failed to serialize library")?;
    fs::write(library_path(config_dir), data).context("Failed to write library.json")
}

fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn upsert_library_entry(config_dir: &Path, path: &str, title: &str, author: Option<&str>) -> Result<()> {
    let mut entries = read_library(config_dir)?;
    let now = now_epoch_secs();
    if let Some(existing) = entries.iter_mut().find(|e| e.path == path) {
        existing.title = title.to_string();
        existing.author = author.map(|s| s.to_string());
        existing.last_opened_at = now;
    } else {
        entries.push(LibraryEntry {
            path: path.to_string(),
            title: title.to_string(),
            author: author.map(|s| s.to_string()),
            added_at: now,
            last_opened_at: now,
            last_position_node_id: None,
            last_position_percent: None,
        });
    }
    write_library(config_dir, &entries)
}

pub fn update_reading_position(config_dir: &Path, path: &str, node_id: i64, percent: f64) -> Result<()> {
    let mut entries = read_library(config_dir)?;
    if let Some(existing) = entries.iter_mut().find(|e| e.path == path) {
        existing.last_position_node_id = Some(node_id);
        existing.last_position_percent = Some(percent.clamp(0.0, 1.0));
        write_library(config_dir, &entries)?;
    }
    Ok(())
}

/// One saved word from the vocab-builder feature: a dictionary lookup the
/// reader chose to keep, with the surrounding sentence(s) captured at
/// save-time (not re-derived later from the book) so the entry still means
/// something even if that book is never opened again. App-level JSON, not
/// per-book SQLite like annotations — the point of a vocab list is
/// browsing it across everything read, not one book's own data.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct VocabEntry {
    pub id: i64,
    pub word: String,
    pub definition: String,
    pub context_before: String,
    pub context_after: String,
    pub book_title: String,
    pub added_at: i64,
}

fn vocab_path(config_dir: &Path) -> PathBuf {
    config_dir.join("vocab.json")
}

pub fn read_vocab(config_dir: &Path) -> Result<Vec<VocabEntry>> {
    match fs::read_to_string(vocab_path(config_dir)) {
        Ok(data) => Ok(serde_json::from_str(&data).unwrap_or_default()),
        Err(_) => Ok(Vec::new()),
    }
}

pub fn write_vocab(config_dir: &Path, entries: &[VocabEntry]) -> Result<()> {
    fs::create_dir_all(config_dir).context("Failed to create config dir")?;
    let data = serde_json::to_string_pretty(entries).context("Failed to serialize vocab")?;
    fs::write(vocab_path(config_dir), data).context("Failed to write vocab.json")
}

#[allow(clippy::too_many_arguments)]
pub fn add_vocab_entry(
    config_dir: &Path,
    word: &str,
    definition: &str,
    context_before: &str,
    context_after: &str,
    book_title: &str,
) -> Result<()> {
    let mut entries = read_vocab(config_dir)?;
    // Nanosecond-precision id (not just now_epoch_secs()) since a user could
    // plausibly add two words within the same second.
    let id = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0);
    entries.push(VocabEntry {
        id,
        word: word.to_string(),
        definition: definition.to_string(),
        context_before: context_before.to_string(),
        context_after: context_after.to_string(),
        book_title: book_title.to_string(),
        added_at: now_epoch_secs(),
    });
    write_vocab(config_dir, &entries)
}

pub fn remove_vocab_entry(config_dir: &Path, id: i64) -> Result<()> {
    let mut entries = read_vocab(config_dir)?;
    entries.retain(|e| e.id != id);
    write_vocab(config_dir, &entries)
}

/// Deterministic per-source sandboxed path: same source EPUB (by canonical
/// path) always resolves to the same output, so re-importing an
/// already-compiled book skips recompilation and never clobbers its
/// annotations. `DefaultHasher::new()` uses fixed keys (unlike HashMap's
/// randomized RandomState), so this is stable across process restarts on a
/// given Rust toolchain — it only needs to be self-consistent, never
/// portable, since nothing persists the hash.
pub fn sandboxed_wld_output_path(books_dir: &Path, input: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(input)
        .with_context(|| format!("Failed to resolve {}", input.display()))?;
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        canonical.hash(&mut h);
        h.finish()
    };

    let raw_stem = input.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "book".into());
    let sanitized: String = raw_stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let sanitized = if sanitized.is_empty() { "book".to_string() } else { sanitized };

    Ok(books_dir.join(format!("{sanitized}-{hash:016x}.wld")))
}

/// Resolves (and creates) this app's config directory, e.g.
/// `~/.config/weland-gtk-reader` on Linux.
pub fn config_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "weland", "gtk-reader")
        .context("Failed to resolve config directory")?;
    let dir = dirs.config_dir().to_path_buf();
    fs::create_dir_all(&dir).context("Failed to create config dir")?;
    Ok(dir)
}

/// Resolves (and creates) this app's data directory, e.g.
/// `~/.local/share/weland-gtk-reader` on Linux — where compiled `.wld` books
/// live, separate from the small JSON config files.
pub fn data_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "weland", "gtk-reader")
        .context("Failed to resolve data directory")?;
    let dir = dirs.data_dir().to_path_buf();
    fs::create_dir_all(&dir).context("Failed to create data dir")?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn settings_round_trip_preserves_unrelated_fields() {
        let dir = tempdir().unwrap();

        let mut settings = read_settings(dir.path());
        assert_eq!(settings, Settings::default());
        settings.author_name = Some("Andrew".to_string());
        write_settings(dir.path(), &settings).unwrap();

        // A second, independent read-modify-write of a different field group
        // must not clobber author_name — the merge-safety invariant CLAUDE.md
        // calls out as load-bearing.
        let mut settings2 = read_settings(dir.path());
        assert_eq!(settings2.author_name.as_deref(), Some("Andrew"));
        settings2.reading_font = Some("literata".to_string());
        settings2.reading_size_px = Some(18.0);
        write_settings(dir.path(), &settings2).unwrap();

        let final_settings = read_settings(dir.path());
        assert_eq!(final_settings.author_name.as_deref(), Some("Andrew"));
        assert_eq!(final_settings.reading_font.as_deref(), Some("literata"));
        assert_eq!(final_settings.reading_size_px, Some(18.0));
    }

    #[test]
    fn library_upsert_adds_then_updates_in_place() {
        let dir = tempdir().unwrap();

        upsert_library_entry(dir.path(), "/books/odyssey.wld", "The Odyssey", Some("Homer")).unwrap();
        let entries = read_library(dir.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "The Odyssey");
        assert_eq!(entries[0].last_position_node_id, None);

        update_reading_position(dir.path(), "/books/odyssey.wld", 42, 0.5).unwrap();
        upsert_library_entry(dir.path(), "/books/odyssey.wld", "The Odyssey (2nd ed.)", Some("Homer")).unwrap();

        let entries = read_library(dir.path()).unwrap();
        assert_eq!(entries.len(), 1, "re-adding the same path must not duplicate the entry");
        assert_eq!(entries[0].title, "The Odyssey (2nd ed.)");
        // Re-upserting title/author must not clobber the reading position set
        // in between — same read-modify-write discipline as settings.
        assert_eq!(entries[0].last_position_node_id, Some(42));
        assert_eq!(entries[0].last_position_percent, Some(0.5));
    }

    #[test]
    fn update_reading_position_clamps_percent() {
        let dir = tempdir().unwrap();
        upsert_library_entry(dir.path(), "/books/x.wld", "X", None).unwrap();
        update_reading_position(dir.path(), "/books/x.wld", 1, 1.5).unwrap();
        let entries = read_library(dir.path()).unwrap();
        assert_eq!(entries[0].last_position_percent, Some(1.0));
    }

    #[test]
    fn sandboxed_path_is_stable_and_collision_resistant() {
        let dir = tempdir().unwrap();
        let books_dir = dir.path().join("books");
        fs::create_dir_all(&books_dir).unwrap();

        let epub_a = dir.path().join("a.epub");
        let epub_b = dir.path().join("b.epub");
        fs::write(&epub_a, b"a").unwrap();
        fs::write(&epub_b, b"b").unwrap();

        let out_a1 = sandboxed_wld_output_path(&books_dir, &epub_a).unwrap();
        let out_a2 = sandboxed_wld_output_path(&books_dir, &epub_a).unwrap();
        let out_b = sandboxed_wld_output_path(&books_dir, &epub_b).unwrap();

        assert_eq!(out_a1, out_a2, "the same source path must hash to the same output every time");
        assert_ne!(out_a1, out_b, "different source paths must not collide");
    }

    #[test]
    fn vocab_add_persists_context_and_remove_deletes_only_that_entry() {
        let dir = tempdir().unwrap();
        assert!(read_vocab(dir.path()).unwrap().is_empty());

        add_vocab_entry(dir.path(), "lenteous", "part of \"plenteous\"", "the harvest was p", "and his master was kind", "Robin Hood").unwrap();
        add_vocab_entry(dir.path(), "cat", "a small domesticated feline", "she had a", "sleeping on the sill", "Some Other Book").unwrap();

        let entries = read_vocab(dir.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].word, "lenteous");
        assert_eq!(entries[0].context_before, "the harvest was p");
        assert_eq!(entries[0].context_after, "and his master was kind");
        assert_eq!(entries[0].book_title, "Robin Hood");
        assert_ne!(entries[0].id, entries[1].id, "each entry must get a distinct id");

        remove_vocab_entry(dir.path(), entries[0].id).unwrap();
        let remaining = read_vocab(dir.path()).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].word, "cat");
    }
}
