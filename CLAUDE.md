# weland

EPUB → SQLite `.wld` ebook compiler/format, plus a Tauri v2 desktop reader (`reader/`).
Named for the smith-god of Germanic legend, imprisoned for his craft, who forged himself
wings and escaped — annotations and reading position live with the book, not locked in a
proprietary reader.

## Layout

- `src/` — core library crate `weland`: `compiler.rs` (EPUB → `.wld`), `db.rs` (shared
  query/mutation layer over the SQLite schema), `schema.rs`, `toolkit.rs` (CLI-facing
  inspect/search/export helpers built on `db.rs`), `main.rs` (CLI binary).
- `reader/` — Tauri v2 desktop reader app, depends on the core crate as
  `weland = { path = "../.." }` so both the CLI and the GUI share one data layer instead of
  duplicating SQL.
  - `reader/src-tauri/src/commands.rs` — `#[tauri::command]` handlers.
  - `reader/dist/` — plain HTML/CSS/JS frontend. **No npm, no bundler, no build step** —
    `withGlobalTauri: true` injects `window.__TAURI__` directly into `app.js`. Keep it that
    way; don't introduce a JS toolchain here.
- `README.md` has the full format spec (Mermaid ER diagram of the six `.wld` tables) — read
  it before touching `schema.rs` or `db.rs`.

## Conventions

- **Only commit/push when explicitly told.** Iterate, rebuild, relaunch, and let the user
  test in between — don't commit as a task-completion reflex.
- **Fonts/assets must be FOSS and self-hosted**, no runtime network calls. Reading fonts are
  pulled from Google Fonts' CSS2 API at build/dev time, filtered to `latin`/`latin-ext`,
  saved as local `.woff2` under `reader/dist/fonts/` with matching `@font-face` rules — never
  link to `fonts.googleapis.com` at runtime.
- **Settings persistence is merge-safe by construction**: `settings.json` (via
  `app.path().app_config_dir()`) holds several independently-read/written optional fields.
  Any new setting must go through a read-modify-write helper (see `get/set_reading_settings`
  in `commands.rs`) so writing one field can never clobber another that was set concurrently
  elsewhere.
- **SQLite is bundled** (`rusqlite` with `bundled`/`bundled-full`) — no system SQLite
  dependency, matters for both plain builds and any future packaging (Flatpak etc.).
- Reading-pane typography uses `em`, not `rem`, so it scales relative to `#readingPane`'s own
  `font-size` (the user's reading-size setting) rather than the document root.
- Custom eased/constant-velocity scrolling (`smoothScrollTo`, held-arrow-key handling in
  `app.js`) intentionally replaces native `scrollIntoView` smooth-scroll — it was too slow to
  interrupt/restart smoothly. Don't revert to native smooth-scroll without re-checking why.
- Asset URLs (`weland-asset://asset/<id>`) are only unique *within one open book* — always
  keep them scoped/keyed by the book's file path (see `assetUrl()` in `app.js`) to avoid
  cross-book image bleed from webview caching.
