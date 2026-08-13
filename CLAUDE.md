# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

WERG is a single-file Node.js CLI (`weland.js`) that compiles an EPUB file into a SQLite database representing the book as an AST. There is no build step, test suite, or bundler — it's one ESM script run directly with Node.

## Commands

```bash
npm install                          # install dependencies (better-sqlite3, jsdom, jszip)
node weland.js compile <input.epub> <output.wld>   # compile an EPUB into a SQLite AST database
```

There is no lint, test, or build command configured in `package.json`.

## Architecture

`weland.js` runs as a linear pipeline inside `compileEpub(inputPath, outputPath)`:

1. **Unzip & locate manifest** — reads the EPUB (a zip) via JSZip, parses `META-INF/container.xml` to find the OPF manifest path, then parses the OPF (via JSDOM in `text/xml` mode) to get book metadata, the cover image, and the manifest/spine.
2. **Metadata & cover extraction** — pulls `dc:title`/`creator`/`language`/`description`/`identifier` from the OPF; resolves the cover image using three fallback strategies in order (EPUB3 `properties*='cover-image'`, EPUB2 `<meta name="cover">`, then a loose id/href match).
3. **Spine resolution** — walks `spine > itemref` against the manifest map to build the ordered list of chapter file paths (`chapterPaths`), all normalized with `normalizePath()` (a manual `..`/`.` resolver, since paths are relative to the OPF's directory, not the zip root).
4. **Async parse phase** — for each chapter, parses the XHTML with JSDOM and walks a fixed set of block-level tags (`h1–h6, p, blockquote, ul, ol, img, hr, table`) into a flat, chapter-scoped `processedElements` array. This phase does all zip/async I/O (loading image bytes, hashing) up front, because...
5. **Sync transaction phase** — `db.transaction()` requires a synchronous callback, so all the actual `ast_nodes`/`assets`/`fts_nodes` inserts happen in a second pass (`runCompilation`) over the already-parsed `compiledChapters`, with no further async work. When touching the parse pipeline, preserve this async-then-sync split — moving I/O into the transaction callback will break `better-sqlite3`.
6. **Inline formatting & footnotes** — `extractTextAndSpans()` walks a DOM subtree once to produce both flattened text and a list of `{start, end, type}` character-offset spans for `italic`/`bold`/`code`/`link`, skipping `<sup>` and in-page anchor `<a href="#...">` tags (footnote markers) so they don't pollute the visible text. Footnote targets are then resolved separately by ID lookup (`chapter.doc.getElementById`) and inserted as child `ast_nodes` rows (`parent_id` pointing at the paragraph that referenced them).

### Database schema (see `INIT_SCHEMA` in weland.js)

- `ast_nodes` — the book's content tree: `parent_id` (self-referential, for footnotes), `ordinal` (global insert order, not per-parent), `node_type` (`heading`/`paragraph`/`blockquote`/`list`/`image`/`table`/`thematic_break`/`footnote`), `content` (plain text, null for images/breaks), `attributes` (JSON — shape depends on `node_type`, e.g. `level`/`spans` for headings, `rows`/`source_file` for tables, `asset_id`/`alt`/`caption` for images).
- `assets` — binary blobs (cover + inline images), deduplicated by sha256 `hash` via `INSERT ... ON CONFLICT(hash) DO UPDATE SET id=id RETURNING id`.
- `metadata` — flat key/value store for book-level fields and `cover_asset_id`.
- `fts_nodes` — FTS5 external-content index mirroring `ast_nodes.content`, keyed by `ast_nodes.id` as `content_rowid`.
- `user_annotations` — reader-side highlights/notes/voice/ink annotations anchored to an `ast_nodes` row via character `start_offset`/`end_offset`; not populated by the compiler, this is the schema for whatever app reads the output database.

Note: `ast_nodes.ordinal` increments across the *entire* document (a module-level counter), not per parent/chapter — don't assume it's scoped.
