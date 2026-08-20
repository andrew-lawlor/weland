//! Local dictionary lookup, entirely offline — no online fallback (there
//! used to be one, calling a free dictionary API; removed in favor of this
//! much larger bundled dataset, per CLAUDE.md's "no runtime network calls"
//! preference anyway). The definitions table is bundled directly into the
//! binary (`include_bytes!`), same pattern as the vendored fonts and the
//! dictionary this replaced. `rusqlite` needs a real file path to open, so
//! the bytes are materialized to disk once on first use; every lookup after
//! that just opens the already-written copy read-only.

use std::path::PathBuf;

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};

const DICTIONARY_BYTES: &[u8] = include_bytes!("../resources/dictionary.sqlite3");

pub struct DictionaryEntry {
    // Not read yet — the UI only shows `definition`, grouped under one
    // title label for the queried word. Kept since a headword can come back
    // with different capitalization than what was typed (matched
    // case-insensitively), which a future multi-word popover may want.
    #[allow(dead_code)]
    pub word: String,
    pub definition: String,
}

/// Writes the bundled dictionary bytes to a temp file and renames it into
/// place, rather than writing `path` directly — `rename` is atomic, so a
/// second lookup racing on the same first-ever materialization always sees
/// either no file or a complete one, never a truncated one mid-write (hit
/// exactly this racing gtk-reader's own parallel test threads with the
/// previous, smaller dictionary: two lookups both saw `path.exists() ==
/// false` and wrote concurrently, and whichever query landed against the
/// file while it was still being truncated got "no such table" back).
fn materialized_path() -> Result<PathBuf> {
    let dir = crate::persistence::data_dir()?;
    let path = dir.join("dictionary.sqlite3");
    if !path.exists() {
        // Unique per call (not just per-process) so two racing callers in
        // the same process never write the same tmp path concurrently.
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let tmp_path = dir.join(format!("dictionary.sqlite3.tmp-{}-{unique}", std::process::id()));
        std::fs::write(&tmp_path, DICTIONARY_BYTES)?;
        std::fs::rename(&tmp_path, &path)?;
    }
    Ok(path)
}

/// Offline lookup against the bundled Wiktionary-derived `entries` table.
/// `definition` is HTML (`<p>`/`<b>`/`<i>`/`<ol>`/`<li>`, HTML entities) —
/// see `dictionary_ui.rs` for the render-time conversion to Pango markup.
pub fn lookup_word(word: &str) -> Result<Vec<DictionaryEntry>> {
    let trimmed = word.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let path = materialized_path()?;
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare_cached("SELECT word, definition FROM entries WHERE word = ?1 COLLATE NOCASE ORDER BY word")?;
    let rows = stmt.query_map([trimmed], |row| Ok(DictionaryEntry { word: row.get(0)?, definition: row.get(1)? }))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_word_returns_empty_for_blank_input() {
        assert!(lookup_word("   ").unwrap().is_empty());
    }

    #[test]
    fn lookup_word_finds_a_common_word_case_insensitively() {
        let entries = lookup_word("Book").unwrap();
        assert!(!entries.is_empty(), "expected the bundled dictionary to have an entry for a common English word");
        assert!(entries.iter().all(|e| e.word.eq_ignore_ascii_case("book")));
    }

    #[test]
    fn lookup_word_returns_empty_for_a_nonsense_string() {
        let entries = lookup_word("zzzqxvbnotaword12345").unwrap();
        assert!(entries.is_empty());
    }
}
