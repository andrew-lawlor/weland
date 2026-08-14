use base64::{engine::general_purpose::STANDARD, Engine as _};
use rusqlite::Connection;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};

use weland::db::{self, NewAnnotation, SearchHit};
use weland::schema::{AstNode, TocEntry, UserAnnotation};

use crate::AppState;

#[derive(serde::Serialize)]
pub struct BookPayload {
    pub metadata: HashMap<String, String>,
    pub toc: Vec<TocEntry>,
    pub nodes: Vec<AstNode>,
    pub annotations: Vec<UserAnnotation>,
}

fn locked_conn<'a>(guard: &'a std::sync::MutexGuard<'_, Option<Connection>>) -> Result<&'a Connection, String> {
    guard.as_ref().ok_or_else(|| "No book is open".to_string())
}

#[tauri::command]
pub fn open_book(path: String, state: State<AppState>) -> Result<BookPayload, String> {
    let conn = Connection::open(&path).map_err(|e| format!("Failed to open {path}: {e}"))?;

    let metadata = db::load_metadata(&conn).map_err(|e| e.to_string())?;
    let toc = db::load_toc(&conn).map_err(|e| e.to_string())?;
    let nodes = db::load_ast_nodes(&conn).map_err(|e| e.to_string())?;
    let annotations = db::load_annotations(&conn).map_err(|e| e.to_string())?;

    *state.db.lock().map_err(|e| e.to_string())? = Some(conn);

    Ok(BookPayload { metadata, toc, nodes, annotations })
}

#[tauri::command]
pub fn search_book(query: String, limit: usize, state: State<AppState>) -> Result<Vec<SearchHit>, String> {
    let guard = state.db.lock().map_err(|e| e.to_string())?;
    let conn = locked_conn(&guard)?;
    db::search_nodes(conn, &query, limit).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn create_highlight(
    node_id: i64,
    start_offset: i64,
    end_offset: i64,
    selected_text: Option<String>,
    author_name: String,
    state: State<AppState>,
) -> Result<UserAnnotation, String> {
    let guard = state.db.lock().map_err(|e| e.to_string())?;
    let conn = locked_conn(&guard)?;
    db::insert_annotation(
        conn,
        NewAnnotation {
            node_id,
            start_offset,
            end_offset,
            selected_text,
            annotation_type: "highlight".to_string(),
            comment: None,
            asset_id: None,
            author_name,
        },
    )
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn create_text_note(
    node_id: i64,
    start_offset: i64,
    end_offset: i64,
    selected_text: Option<String>,
    comment: String,
    author_name: String,
    state: State<AppState>,
) -> Result<UserAnnotation, String> {
    let guard = state.db.lock().map_err(|e| e.to_string())?;
    let conn = locked_conn(&guard)?;
    db::insert_annotation(
        conn,
        NewAnnotation {
            node_id,
            start_offset,
            end_offset,
            selected_text,
            annotation_type: "text_note".to_string(),
            comment: Some(comment),
            asset_id: None,
            author_name,
        },
    )
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_voice_note(
    node_id: i64,
    start_offset: i64,
    end_offset: i64,
    selected_text: Option<String>,
    audio_base64: String,
    mime_type: String,
    author_name: String,
    state: State<AppState>,
) -> Result<UserAnnotation, String> {
    let guard = state.db.lock().map_err(|e| e.to_string())?;
    let conn = locked_conn(&guard)?;

    let bytes = STANDARD
        .decode(audio_base64.as_bytes())
        .map_err(|e| format!("Failed to decode recorded audio: {e}"))?;

    let asset_id = db::insert_voice_asset(conn, &mime_type, &bytes).map_err(|e| e.to_string())?;

    db::insert_annotation(
        conn,
        NewAnnotation {
            node_id,
            start_offset,
            end_offset,
            selected_text,
            annotation_type: "voice_note".to_string(),
            comment: None,
            asset_id: Some(asset_id),
            author_name,
        },
    )
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn update_note(id: i64, comment: String, state: State<AppState>) -> Result<UserAnnotation, String> {
    let guard = state.db.lock().map_err(|e| e.to_string())?;
    let conn = locked_conn(&guard)?;
    db::update_annotation_comment(conn, id, &comment).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_annotation(id: i64, state: State<AppState>) -> Result<(), String> {
    let guard = state.db.lock().map_err(|e| e.to_string())?;
    let conn = locked_conn(&guard)?;
    db::delete_annotation(conn, id).map_err(|e| e.to_string())
}

#[derive(serde::Serialize)]
pub struct AuthorInfo {
    pub name: String,
    pub is_saved: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Settings {
    author_name: String,
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("settings.json"))
}

// `whoami::realname()` gives the OS account's display name where the OS tracks one
// (macOS full name, Windows display name, Linux GECOS field); it falls back to the
// login username when nothing is set, which is common on Linux.
fn guess_os_author_name() -> String {
    let name = whoami::realname();
    if name.trim().is_empty() {
        whoami::username()
    } else {
        name
    }
}

#[tauri::command]
pub fn get_author_name(app: AppHandle) -> Result<AuthorInfo, String> {
    let path = settings_path(&app)?;
    if let Ok(data) = fs::read_to_string(&path) {
        if let Ok(settings) = serde_json::from_str::<Settings>(&data) {
            return Ok(AuthorInfo { name: settings.author_name, is_saved: true });
        }
    }
    Ok(AuthorInfo { name: guess_os_author_name(), is_saved: false })
}

#[tauri::command]
pub fn set_author_name(name: String, app: AppHandle) -> Result<(), String> {
    let path = settings_path(&app)?;
    let data = serde_json::to_string_pretty(&Settings { author_name: name }).map_err(|e| e.to_string())?;
    fs::write(&path, data).map_err(|e| e.to_string())
}
