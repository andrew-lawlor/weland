//! Local + online dictionary lookup — ported from
//! `reader/src-tauri/src/dictionary.rs`. The definitions table is bundled
//! directly into the binary (`include_bytes!`) rather than resolved via
//! Tauri's resource resolver, since this crate has no equivalent resource
//! system. `rusqlite` needs a real file path to open, so the bytes are
//! materialized to disk once on first use; every lookup after that just
//! opens the already-written copy read-only.

use std::path::PathBuf;

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};

const DICTIONARY_BYTES: &[u8] = include_bytes!("../resources/dictionary.db");

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
/// exactly this racing gtk-reader's own parallel test threads: two lookups
/// both saw `path.exists() == false` and wrote concurrently, and whichever
/// query landed against the file while it was still being truncated got
/// "no such table: definitions" back).
fn materialized_path() -> Result<PathBuf> {
    let dir = crate::persistence::data_dir()?;
    let path = dir.join("dictionary.db");
    if !path.exists() {
        // Unique per call (not just per-process) so two racing callers in
        // the same process never write the same tmp path concurrently.
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let tmp_path = dir.join(format!("dictionary.db.tmp-{}-{unique}", std::process::id()));
        std::fs::write(&tmp_path, DICTIONARY_BYTES)?;
        std::fs::rename(&tmp_path, &path)?;
    }
    Ok(path)
}

/// Offline lookup against the bundled GCIDE-derived definitions table.
pub fn lookup_word(word: &str) -> Result<Vec<DictionaryEntry>> {
    let trimmed = word.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let path = materialized_path()?;
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare_cached("SELECT word, definition FROM definitions WHERE word = ?1 COLLATE NOCASE ORDER BY word")?;
    let rows = stmt.query_map([trimmed], |row| Ok(DictionaryEntry { word: row.get(0)?, definition: row.get(1)? }))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

#[derive(serde::Deserialize)]
struct ApiEntry {
    #[serde(default)]
    meanings: Vec<ApiMeaning>,
}

#[derive(serde::Deserialize)]
struct ApiMeaning {
    #[serde(rename = "partOfSpeech", default)]
    part_of_speech: String,
    #[serde(default)]
    definitions: Vec<ApiDefinition>,
}

#[derive(serde::Deserialize)]
struct ApiDefinition {
    definition: String,
}

/// Free, keyless dictionary API sourced from Wiktionary — opt-in per lookup
/// (only called when the local table has nothing), so the app stays fully
/// offline unless a lookup explicitly falls through to it. Blocking, meant
/// to be called from a background thread — this crate has no async
/// runtime, and pulling one in for a single occasional HTTP call isn't
/// worth it next to the `std::thread` + channel pattern already used
/// elsewhere (recording, library import).
pub fn lookup_word_online(word: &str) -> Result<Vec<DictionaryEntry>> {
    let trimmed = word.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let url = format!("https://api.dictionaryapi.dev/api/v2/entries/en/{}", urlencoding_encode(trimmed));
    let response = reqwest::blocking::get(&url)?;
    if !response.status().is_success() {
        // 404 (no entry) is the expected "not found" case, not an error.
        return Ok(Vec::new());
    }

    let api_entries: Vec<ApiEntry> = response.json()?;
    let mut entries = Vec::new();
    for api_entry in api_entries {
        for meaning in api_entry.meanings {
            for def in meaning.definitions {
                entries.push(DictionaryEntry { word: word.to_string(), definition: format!("({}) {}", meaning.part_of_speech, def.definition) });
            }
        }
    }
    Ok(entries)
}

// Small self-contained percent-encoder for the one path segment we need,
// rather than pulling in a whole dedicated crate for it.
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
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

    #[test]
    fn urlencoding_encode_escapes_reserved_characters() {
        assert_eq!(urlencoding_encode("hello world"), "hello%20world");
        assert_eq!(urlencoding_encode("safe-word_.~"), "safe-word_.~");
    }
}
