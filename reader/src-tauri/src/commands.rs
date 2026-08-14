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
    pub path: String,
    pub metadata: HashMap<String, String>,
    pub toc: Vec<TocEntry>,
    pub nodes: Vec<AstNode>,
    pub annotations: Vec<UserAnnotation>,
    pub last_position_node_id: Option<i64>,
}

fn locked_conn<'a>(guard: &'a std::sync::MutexGuard<'_, Option<Connection>>) -> Result<&'a Connection, String> {
    guard.as_ref().ok_or_else(|| "No book is open".to_string())
}

#[tauri::command]
pub fn open_book(path: String, state: State<AppState>, app: AppHandle) -> Result<BookPayload, String> {
    let conn = Connection::open(&path).map_err(|e| format!("Failed to open {path}: {e}"))?;

    let metadata = db::load_metadata(&conn).map_err(|e| e.to_string())?;
    let toc = db::load_toc(&conn).map_err(|e| e.to_string())?;
    let nodes = db::load_ast_nodes(&conn).map_err(|e| e.to_string())?;
    let annotations = db::load_annotations(&conn).map_err(|e| e.to_string())?;

    let title = metadata.get("title").cloned().unwrap_or_else(|| {
        std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "Untitled".to_string())
    });
    // Read the saved position before upserting — upsert doesn't touch it, but
    // reading it first keeps this independent of that ordering.
    let last_position_node_id = find_last_position(&app, &path);

    // Library bookkeeping is best-effort — a failure here shouldn't stop the book from opening.
    let _ = upsert_library_entry(&app, &path, &title, metadata.get("author").map(|s| s.as_str()));

    *state.db.lock().map_err(|e| e.to_string())? = Some(conn);

    Ok(BookPayload { path, metadata, toc, nodes, annotations, last_position_node_id })
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

// All fields optional so an older settings.json (or one only ever touched by one
// of get/set_author_name vs get/set_reading_settings) still parses fine, and a
// read-modify-write of one group of fields never clobbers the other.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Settings {
    #[serde(default)]
    author_name: Option<String>,
    #[serde(default)]
    reading_font: Option<String>,
    #[serde(default)]
    reading_size_px: Option<f64>,
    #[serde(default)]
    reading_leading: Option<f64>,
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("settings.json"))
}

fn read_settings(app: &AppHandle) -> Settings {
    settings_path(app)
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

fn write_settings(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let path = settings_path(app)?;
    let data = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    fs::write(&path, data).map_err(|e| e.to_string())
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
    match read_settings(&app).author_name {
        Some(name) => Ok(AuthorInfo { name, is_saved: true }),
        None => Ok(AuthorInfo { name: guess_os_author_name(), is_saved: false }),
    }
}

#[tauri::command]
pub fn set_author_name(name: String, app: AppHandle) -> Result<(), String> {
    let mut settings = read_settings(&app);
    settings.author_name = Some(name);
    write_settings(&app, &settings)
}

const DEFAULT_READING_FONT: &str = "literata";
const DEFAULT_READING_SIZE_PX: f64 = 17.0;
const DEFAULT_READING_LEADING: f64 = 1.75;

#[derive(serde::Serialize)]
pub struct ReadingSettings {
    pub font: String,
    pub size_px: f64,
    pub leading: f64,
}

#[tauri::command]
pub fn get_reading_settings(app: AppHandle) -> Result<ReadingSettings, String> {
    let s = read_settings(&app);
    Ok(ReadingSettings {
        font: s.reading_font.unwrap_or_else(|| DEFAULT_READING_FONT.to_string()),
        size_px: s.reading_size_px.unwrap_or(DEFAULT_READING_SIZE_PX),
        leading: s.reading_leading.unwrap_or(DEFAULT_READING_LEADING),
    })
}

#[tauri::command]
pub fn set_reading_settings(font: String, size_px: f64, leading: f64, app: AppHandle) -> Result<(), String> {
    let mut settings = read_settings(&app);
    settings.reading_font = Some(font);
    settings.reading_size_px = Some(size_px);
    settings.reading_leading = Some(leading);
    write_settings(&app, &settings)
}

/* ================= Library ================= */

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct LibraryEntry {
    path: String,
    title: String,
    author: Option<String>,
    added_at: i64,
    last_opened_at: i64,
    #[serde(default)]
    last_position_node_id: Option<i64>,
}

#[derive(serde::Serialize)]
pub struct LibraryBook {
    pub path: String,
    pub title: String,
    pub author: Option<String>,
    pub added_at: i64,
    pub last_opened_at: i64,
    pub cover_data_uri: Option<String>,
    pub available: bool,
}

fn library_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("library.json"))
}

fn read_library(app: &AppHandle) -> Result<Vec<LibraryEntry>, String> {
    let path = library_path(app)?;
    match fs::read_to_string(&path) {
        Ok(data) => Ok(serde_json::from_str(&data).unwrap_or_default()),
        Err(_) => Ok(Vec::new()),
    }
}

fn write_library(app: &AppHandle, entries: &[LibraryEntry]) -> Result<(), String> {
    let path = library_path(app)?;
    let data = serde_json::to_string_pretty(entries).map_err(|e| e.to_string())?;
    fs::write(&path, data).map_err(|e| e.to_string())
}

fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn upsert_library_entry(app: &AppHandle, path: &str, title: &str, author: Option<&str>) -> Result<(), String> {
    let mut entries = read_library(app)?;
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
        });
    }
    write_library(app, &entries)
}

fn find_last_position(app: &AppHandle, path: &str) -> Option<i64> {
    read_library(app)
        .ok()?
        .into_iter()
        .find(|e| e.path == path)?
        .last_position_node_id
}

#[tauri::command]
pub fn update_reading_position(path: String, node_id: i64, app: AppHandle) -> Result<(), String> {
    let mut entries = read_library(&app)?;
    if let Some(existing) = entries.iter_mut().find(|e| e.path == path) {
        existing.last_position_node_id = Some(node_id);
        write_library(&app, &entries)?;
    }
    Ok(())
}

// Opens a book's own database read-only just to pull its cover thumbnail —
// never creates or modifies the file, so a stale/missing library path is safe.
fn load_cover_data_uri(path: &str) -> anyhow::Result<Option<String>> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let metadata = db::load_metadata(&conn)?;
    let Some(cover_id_str) = metadata.get("cover_asset_id") else {
        return Ok(None);
    };
    let cover_id: i64 = cover_id_str.parse()?;
    let (mime, data) = db::load_asset(&conn, cover_id)?;
    Ok(Some(format!("data:{};base64,{}", mime, STANDARD.encode(data))))
}

#[tauri::command]
pub fn list_library(app: AppHandle) -> Result<Vec<LibraryBook>, String> {
    let mut entries = read_library(&app)?;
    entries.sort_by(|a, b| b.last_opened_at.cmp(&a.last_opened_at));

    Ok(entries
        .into_iter()
        .map(|e| {
            let available = std::path::Path::new(&e.path).exists();
            let cover_data_uri = if available { load_cover_data_uri(&e.path).ok().flatten() } else { None };
            LibraryBook {
                path: e.path,
                title: e.title,
                author: e.author,
                added_at: e.added_at,
                last_opened_at: e.last_opened_at,
                cover_data_uri,
                available,
            }
        })
        .collect())
}

#[tauri::command]
pub fn remove_from_library(path: String, app: AppHandle) -> Result<(), String> {
    let mut entries = read_library(&app)?;
    entries.retain(|e| e.path != path);
    write_library(&app, &entries)
}
