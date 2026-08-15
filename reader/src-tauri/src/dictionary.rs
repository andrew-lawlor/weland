use rusqlite::Connection;
use tauri::{AppHandle, Manager, State};

use crate::AppState;

#[derive(serde::Serialize)]
pub struct DictionaryEntry {
    pub word: String,
    pub definition: String,
}

fn open_dictionary(app: &AppHandle) -> Result<Connection, String> {
    let path = app
        .path()
        .resolve("resources/dictionary.db", tauri::path::BaseDirectory::Resource)
        .map_err(|e| e.to_string())?;
    Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn lookup_word(word: String, state: State<'_, AppState>, app: AppHandle) -> Result<Vec<DictionaryEntry>, String> {
    let trimmed = word.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let mut guard = state.dict_db.lock().map_err(|_| "Dictionary lock poisoned".to_string())?;
    if guard.is_none() {
        *guard = Some(open_dictionary(&app)?);
    }
    let conn = guard.as_ref().unwrap();

    let mut stmt = conn
        .prepare_cached("SELECT word, definition FROM definitions WHERE word = ?1 COLLATE NOCASE ORDER BY word")
        .map_err(|e| e.to_string())?;

    let rows = stmt
        .query_map([trimmed], |row| {
            Ok(DictionaryEntry {
                word: row.get(0)?,
                definition: row.get(1)?,
            })
        })
        .map_err(|e| e.to_string())?;

    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
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

// Free, keyless dictionary API sourced from Wiktionary — opt-in per lookup
// from the frontend (never called automatically), so the app stays fully
// offline unless the reader explicitly asks for this one word. Done from
// Rust with reqwest rather than the webview's own fetch(), which turned out
// to be unreliable across repeated calls in webkit2gtk.
#[tauri::command]
pub async fn lookup_word_online(word: String) -> Result<Vec<DictionaryEntry>, String> {
    let trimmed = word.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let url = format!(
        "https://api.dictionaryapi.dev/api/v2/entries/en/{}",
        urlencoding_encode(trimmed)
    );

    let response = reqwest::get(&url).await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        // 404 (no entry) is the expected "not found" case, not an error.
        return Ok(Vec::new());
    }

    let api_entries: Vec<ApiEntry> = response.json().await.map_err(|e| e.to_string())?;

    let mut entries = Vec::new();
    for api_entry in api_entries {
        for meaning in api_entry.meanings {
            for def in meaning.definitions {
                entries.push(DictionaryEntry {
                    word: word.clone(),
                    definition: format!("({}) {}", meaning.part_of_speech, def.definition),
                });
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
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}
