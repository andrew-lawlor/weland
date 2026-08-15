use base64::{engine::general_purpose::STANDARD, Engine as _};
use rusqlite::Connection;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter, Manager, State};

use weland::compiler::{compile_epub, CompileOptions};
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

// Deterministic per-source sandboxed path: same source EPUB (by canonical
// path) always resolves to the same output, so re-importing an already-
// compiled book still hits the `if !output.exists()` skip in import_epub
// and never clobbers its annotations. DefaultHasher::new() uses fixed
// keys (unlike HashMap's randomized RandomState), so this is stable
// across process restarts on a given Rust toolchain — it only needs to
// be self-consistent, never portable, since nothing persists the hash.
fn sandboxed_wld_output_path(app: &AppHandle, input: &Path) -> Result<PathBuf, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let books_dir = data_dir.join("books");
    fs::create_dir_all(&books_dir).map_err(|e| e.to_string())?;

    let canonical = fs::canonicalize(input)
        .map_err(|e| format!("Failed to resolve {}: {e}", input.display()))?;
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

// Compiles an EPUB to .wld (via the same compiler the CLI uses) and opens it,
// reusing open_book entirely for the load/library-bookkeeping step.
//
// `output_path` lets the frontend retry at a caller-chosen location (e.g. via
// a save dialog) if the default adjacent-to-the-source path isn't writable.
// If a .wld already exists at the target path, compiling is skipped and it's
// opened as-is — silently recompiling would blow away any annotations
// already stored in it.
//
// Declared async and runs the actual compile via spawn_blocking: this is a
// genuinely CPU/IO-heavy synchronous call (HTML parsing, asset extraction,
// FTS indexing), and running it as a plain sync command left the UI frozen
// with no chance to paint the "compiling" overlay first.
#[tauri::command]
pub async fn import_epub(
    input_path: String,
    output_path: Option<String>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<BookPayload, String> {
    let input = std::path::Path::new(&input_path).to_path_buf();
    let output = match output_path {
        Some(p) => PathBuf::from(p),
        None => sandboxed_wld_output_path(&app, &input)?,
    };

    if !output.exists() {
        let compile_target = output.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let options = CompileOptions { quiet: true, verbose: false };
            compile_epub(&input, &compile_target, &options)
        })
        .await
        .map_err(|e| format!("Compile task failed: {e}"))?
        .map_err(|e| format!("Failed to compile {input_path}: {e}"))?;

        // A freshly compiled file has no reading history, even if it happens to
        // land at a path a since-deleted .wld previously occupied — otherwise
        // whatever position was saved there before gets restored into content
        // that (from the reader's perspective) was just opened for the first time.
        clear_last_position(&app, &output.to_string_lossy());
    }

    open_book(output.to_string_lossy().to_string(), state, app)
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
    #[serde(default)]
    reading_verse_spacing: Option<f64>,
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
const DEFAULT_READING_VERSE_SPACING: f64 = 2.0;

#[derive(serde::Serialize)]
pub struct ReadingSettings {
    pub font: String,
    pub size_px: f64,
    pub leading: f64,
    pub verse_spacing: f64,
}

#[tauri::command]
pub fn get_reading_settings(app: AppHandle) -> Result<ReadingSettings, String> {
    let s = read_settings(&app);
    Ok(ReadingSettings {
        font: s.reading_font.unwrap_or_else(|| DEFAULT_READING_FONT.to_string()),
        size_px: s.reading_size_px.unwrap_or(DEFAULT_READING_SIZE_PX),
        leading: s.reading_leading.unwrap_or(DEFAULT_READING_LEADING),
        verse_spacing: s.reading_verse_spacing.unwrap_or(DEFAULT_READING_VERSE_SPACING),
    })
}

#[tauri::command]
pub fn set_reading_settings(
    font: String,
    size_px: f64,
    leading: f64,
    verse_spacing: f64,
    app: AppHandle,
) -> Result<(), String> {
    let mut settings = read_settings(&app);
    settings.reading_font = Some(font);
    settings.reading_size_px = Some(size_px);
    settings.reading_leading = Some(leading);
    settings.reading_verse_spacing = Some(verse_spacing);
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

fn clear_last_position(app: &AppHandle, path: &str) {
    if let Ok(mut entries) = read_library(app) {
        if let Some(existing) = entries.iter_mut().find(|e| e.path == path) {
            existing.last_position_node_id = None;
            let _ = write_library(app, &entries);
        }
    }
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
pub fn list_library(app: AppHandle, state: State<AppState>) -> Result<Vec<LibraryBook>, String> {
    let mut entries = read_library(&app)?;
    entries.sort_by(|a, b| b.last_opened_at.cmp(&a.last_opened_at));

    let mut cache = state.cover_cache.lock().map_err(|e| e.to_string())?;

    Ok(entries
        .into_iter()
        .map(|e| {
            let available = std::path::Path::new(&e.path).exists();
            let cover_data_uri = if available {
                cache
                    .entry(e.path.clone())
                    .or_insert_with(|| load_cover_data_uri(&e.path).ok().flatten())
                    .clone()
            } else {
                None
            };
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

// Copies a book's sandboxed .wld out to a user-chosen location — the sole
// way a book leaves the app's sandboxed data dir intact. `path` in
// library.json (the identity key used everywhere else) is never touched;
// this is a copy-out, not a move.
#[tauri::command]
pub fn export_book(path: String, dest_path: String) -> Result<(), String> {
    fs::copy(&path, &dest_path).map_err(|e| format!("Failed to export {path}: {e}"))?;
    Ok(())
}

#[derive(serde::Serialize)]
pub struct ExportResult {
    pub exported: Vec<String>,
    pub failed: Vec<String>,
}

// Bulk-copies every library book that still has a backing file into
// `dest_dir`. Missing-file entries are skipped (nothing to export, not a
// failure); one book's copy failing doesn't abort the rest of the batch.
#[tauri::command]
pub fn export_library(dest_dir: String, app: AppHandle) -> Result<ExportResult, String> {
    let entries = read_library(&app)?;
    let dest = PathBuf::from(&dest_dir);
    let mut exported = Vec::new();
    let mut failed = Vec::new();
    let mut used_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for e in entries.iter().filter(|e| Path::new(&e.path).exists()) {
        let sanitized: String = e
            .title
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == ' ' || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let base = if sanitized.trim().is_empty() { "book".to_string() } else { sanitized.trim().to_string() };
        let mut name = format!("{base}.wld");
        let mut n = 1;
        while used_names.contains(&name) {
            n += 1;
            name = format!("{base} ({n}).wld");
        }
        used_names.insert(name.clone());

        match fs::copy(&e.path, dest.join(&name)) {
            Ok(_) => exported.push(name),
            Err(err) => failed.push(format!("{}: {err}", e.title)),
        }
    }
    Ok(ExportResult { exported, failed })
}

// Iteratively (not recursively, to avoid stack depth concerns on deep
// libraries) walks `root` for .epub files. Hidden directories (Calibre's
// `.caltrash`, `.git`, etc.) are skipped; unreadable subdirectories are
// skipped rather than aborting the whole scan.
fn find_epubs_recursive(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let hidden = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with('.'))
                    .unwrap_or(false);
                if !hidden {
                    stack.push(path);
                }
            } else if path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("epub")).unwrap_or(false) {
                found.push(path);
            }
        }
    }
    found
}

// Registers an already-compiled .wld into the library without touching
// AppState's single "currently open" connection slot — open_book isn't
// reused here since bulk import shouldn't leave that slot pointing at
// whatever book happened to be imported last.
fn register_imported_book(app: &AppHandle, path: &Path) -> Result<String, String> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())?;
    let metadata = db::load_metadata(&conn).map_err(|e| e.to_string())?;
    let title = metadata.get("title").cloned().unwrap_or_else(|| {
        path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Untitled".to_string())
    });
    upsert_library_entry(app, &path.to_string_lossy(), &title, metadata.get("author").map(|s| s.as_str()))?;
    Ok(title)
}

#[derive(Clone, serde::Serialize)]
struct BulkImportProgress {
    current: usize,
    total: usize,
    title: String,
}

#[derive(serde::Serialize)]
pub struct BulkImportSummary {
    pub imported: Vec<String>,
    pub skipped: Vec<String>,
    pub failed: Vec<String>,
}

// Recursively imports every EPUB under `root_path`, emitting a
// "bulk-import-progress" event before each attempt so the frontend can
// show live "Importing X of N" feedback. Already-imported books (same
// sandboxed output path already exists) are skipped, not recompiled —
// same idempotency guarantee as a single import_epub call. One book
// failing doesn't abort the batch.
#[tauri::command]
pub async fn import_folder(root_path: String, app: AppHandle) -> Result<BulkImportSummary, String> {
    let root = PathBuf::from(&root_path);
    if !root.is_dir() {
        return Err(format!("{root_path} is not a folder"));
    }

    let scan_root = root.clone();
    let epub_paths = tauri::async_runtime::spawn_blocking(move || find_epubs_recursive(&scan_root))
        .await
        .map_err(|e| format!("Folder scan failed: {e}"))?;

    let total = epub_paths.len();
    let mut summary = BulkImportSummary { imported: Vec::new(), skipped: Vec::new(), failed: Vec::new() };

    for (i, input) in epub_paths.into_iter().enumerate() {
        let stem = input.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "book".to_string());
        let _ = app.emit("bulk-import-progress", BulkImportProgress { current: i + 1, total, title: stem.clone() });

        let output = match sandboxed_wld_output_path(&app, &input) {
            Ok(p) => p,
            Err(err) => {
                summary.failed.push(format!("{stem}: {err}"));
                continue;
            }
        };

        if output.exists() {
            summary.skipped.push(stem);
            continue;
        }

        let compile_target = output.clone();
        let compile_input = input.clone();
        let result = tauri::async_runtime::spawn_blocking(move || {
            let options = CompileOptions { quiet: true, verbose: false };
            compile_epub(&compile_input, &compile_target, &options)
        })
        .await;

        match result {
            Ok(Ok(_)) => match register_imported_book(&app, &output) {
                Ok(title) => summary.imported.push(title),
                Err(err) => summary.failed.push(format!("{stem}: {err}")),
            },
            Ok(Err(err)) => summary.failed.push(format!("{stem}: {err}")),
            Err(err) => summary.failed.push(format!("{stem}: task failed: {err}")),
        }
    }

    Ok(summary)
}
