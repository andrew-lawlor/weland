#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod dictionary;
mod recording;

use rusqlite::Connection;
use std::sync::Mutex;
use tauri::{Manager, State};

pub struct AppState {
    pub db: Mutex<Option<Connection>>,
    pub dict_db: Mutex<Option<Connection>>,
    pub recording: Mutex<Option<recording::RecordingSession>>,
}

fn asset_response(mime: String, data: Vec<u8>) -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .header("Content-Type", mime)
        .header("Access-Control-Allow-Origin", "*")
        // Asset IDs are only unique within a single .wld's own `assets` table, so the
        // same weland-asset://asset/<id> URL can point at completely different bytes
        // in different books. Without this, the webview's HTTP cache happily serves
        // a previous book's image for that URL instead of re-querying the DB.
        .header("Cache-Control", "no-store")
        .status(200)
        .body(data)
        .unwrap()
}

fn not_found() -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder().status(404).body(Vec::new()).unwrap()
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState { db: Mutex::new(None), dict_db: Mutex::new(None), recording: Mutex::new(None) })
        .register_uri_scheme_protocol("weland-cover", |_ctx, request| {
            // weland-cover://cover/<percent-encoded absolute .wld path> — unlike
            // weland-asset (scoped to whatever book is currently open), library
            // cards need covers for every book in the library at once, so this
            // opens each target file fresh, read-only, on demand. Letting the
            // webview fetch these as plain <img> requests (instead of the old
            // approach of base64-embedding every cover into list_library's JSON
            // response) keeps that response tiny and moves image decoding off
            // the JS main thread entirely.
            let raw_path = request.uri().path().trim_start_matches('/');
            let path = percent_encoding::percent_decode_str(raw_path)
                .decode_utf8_lossy()
                .into_owned();

            let conn = match Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
                Ok(c) => c,
                Err(_) => return not_found(),
            };
            let Ok(metadata) = weland::db::load_metadata(&conn) else {
                return not_found();
            };
            let Some(cover_id) = metadata.get("cover_asset_id").and_then(|s| s.parse::<i64>().ok()) else {
                return not_found();
            };
            match weland::db::load_asset(&conn, cover_id) {
                Ok((mime, data)) => asset_response(mime, data),
                Err(_) => not_found(),
            }
        })
        .register_uri_scheme_protocol("weland-asset", |ctx, request| {
            let app = ctx.app_handle();
            let state: State<AppState> = app.state();

            let guard = match state.db.lock() {
                Ok(g) => g,
                Err(_) => return not_found(),
            };
            let Some(conn) = guard.as_ref() else {
                return not_found();
            };

            // weland-asset://asset/<id> — the id is the only part we need.
            let asset_id = request.uri().path().trim_start_matches('/').parse::<i64>();
            let Ok(asset_id) = asset_id else {
                return not_found();
            };

            match weland::db::load_asset(conn, asset_id) {
                Ok((mime, data)) => asset_response(mime, data),
                Err(_) => not_found(),
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::open_book,
            commands::import_epub,
            commands::search_book,
            commands::create_highlight,
            commands::create_text_note,
            commands::save_voice_note,
            recording::start_voice_recording,
            recording::stop_voice_recording,
            commands::update_note,
            commands::delete_annotation,
            commands::get_asset_data,
            commands::get_author_name,
            commands::set_author_name,
            commands::list_library,
            commands::search_library,
            commands::remove_from_library,
            commands::export_book,
            commands::export_library,
            commands::import_folder,
            commands::get_reading_settings,
            commands::set_reading_settings,
            commands::update_reading_position,
            dictionary::lookup_word,
            dictionary::lookup_word_online,
        ])
        .run(tauri::generate_context!())
        .expect("error while running weland-reader");
}
