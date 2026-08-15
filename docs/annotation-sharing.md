# Sharing marginalia between readers — design sketch

Status: **not implemented, not scheduled**. Captured for later reference.

## Origin

Brainstormed with Gemini around three architectural patterns for "multiplayer" /
sync between Weland readers, then evaluated here. Full options considered:

1. **Annotation layer export/import** ("Pattern 1") — treat marginalia as a portable
   overlay on top of an immutable book, transferred as a plain file (email, USB,
   Syncthing, whatever). No server.
2. **Magic link / QR + ephemeral relay** ("Pattern 2") — E2EE blob relay, one-click
   share via link. Requires operating a lightweight server.
3. **Centralized vault / `weland-server`** ("Pattern 3") — real-time multi-user sync
   over WebSocket/SSE for active reading groups. A genuinely different product
   (accounts, live sync, moderation of others' content).

## Decision: build Pattern 1, defer 2 and 3

Pattern 1 is the only one that's close to free given the existing architecture — a
`.wld` is already a self-contained SQLite file with annotations living alongside the
book, so "export/import annotations" is a thin feature on schema that already mostly
exists (`author_id`, `device_id`, `created_at`, `updated_at` are already columns on
`user_annotations`, just unused today).

It's also thematically on-brand: the project's whole pitch is "your annotations
carried forward, not locked in." Extending that from *across time* (you, later) to
*across people* (someone else, now) is the same idea, not scope creep.

Patterns 2 and 3 both require operating a server — a different commitment than a
file format. Don't build either speculatively; revisit only if Pattern 1 has real
users hitting its ceiling.

## Key simplification: no new file format

Don't invent a `.wld-layer` sidecar format. Since a `.wld` **is** SQLite, "export for
a friend" just produces a normal `.wld`: the whole book plus only the exporting
reader's own annotations. It opens exactly like any other book, because it is one.

The only new smarts are on **import**: the reader computes/compares a content
fingerprint (see schema below) to recognize "this is a copy of a book I already
have" and offers *"merge just the annotations into your existing copy?"* instead of
adding a duplicate library entry. One file type for everything — matches the
existing "your book, your annotations, one file" story, and doubles as an onboarding
hook (hand someone a book *and* your thoughts on it in a single file).

## Imported annotations are a layer, not a flatten

Don't `INSERT` an imported author's rows directly into the recipient's own
`user_annotations` indistinguishably — that makes them permanently intermingled and
impossible to cleanly toggle or remove later. Instead, imported annotations belong
to a distinct, toggleable **layer**. The annotations panel becomes a list of layers
("Your notes" / "Alice's notes"), each independently show/hideable and removable.
Re-importing an updated layer just replaces that layer's rows wholesale — no need
for per-annotation UUIDs/dedup logic.

## Privacy

"Export my annotations" must not be all-or-nothing by default — a stray private note
shouldn't get swept into a share accidentally. Solve with a per-annotation
`include_in_export` flag (default on), surfaced as checkboxes in an export dialog.

## Schema sketch

Against the current `src/schema.rs`.

**New table — `annotation_layers`.** Every `.wld` gets an implicit "local" layer
(id 1) for the reader's own notes; imported marginalia gets its own row.

```sql
CREATE TABLE IF NOT EXISTS annotation_layers (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source TEXT NOT NULL DEFAULT 'local',   -- 'local' | 'imported'
  label TEXT NOT NULL,                    -- e.g. "Alice's notes" (editable)
  author_name TEXT,
  author_id TEXT,                         -- mirrors user_annotations.author_id
  source_fingerprint TEXT,                -- content_fingerprint of the book this
                                           -- layer's annotations were exported from —
                                           -- checked against local fingerprint before
                                           -- import, not needed after that
  imported_at DATETIME,                   -- NULL for the local layer
  visible BOOLEAN NOT NULL DEFAULT 1      -- sidebar toggle state, persisted
);

-- seed row every .wld gets at compile time:
INSERT INTO annotation_layers (id, source, label) VALUES (1, 'local', 'My notes');
```

**Alter `user_annotations`:**

```sql
ALTER TABLE user_annotations ADD COLUMN layer_id INTEGER NOT NULL
  REFERENCES annotation_layers(id) ON DELETE CASCADE DEFAULT 1;

ALTER TABLE user_annotations ADD COLUMN include_in_export BOOLEAN NOT NULL DEFAULT 1;

CREATE INDEX IF NOT EXISTS idx_user_annotations_layer ON user_annotations(layer_id);
CREATE INDEX IF NOT EXISTS idx_annotation_layers_fingerprint ON annotation_layers(source_fingerprint);
```

Export query, roughly: `WHERE include_in_export = 1 AND layer_id = 1` — only your
own, exportable notes; an imported layer is never re-exported from a third party's
copy.

**No new table for the fingerprint.** `metadata` is already a generic key/value
table, so this is just a new well-known key, computed once by the compiler over the
canonical `ast_nodes` content in ordinal order:

```
INSERT INTO metadata (key, value) VALUES ('content_fingerprint', '<sha256 hex>');
```

Import compares this to decide "new library book" vs. "merge as a layer into my
existing copy."

**`assets` needs no schema change.** It's already hash-deduped
(`hash TEXT UNIQUE NOT NULL`), so when a merged layer drags in a voice-note BLOB,
import just re-runs the existing insert-with-dedup path and remaps `asset_id` on the
incoming rows to whatever id that hash resolves to locally.

## The one real wrinkle: migrating already-compiled files

This is the **first** schema change that has to apply to `.wld` files that already
exist on disk, not just freshly-compiled ones. Today `INIT_SCHEMA`
(`src/schema.rs`) only ever runs once, at compile time
(`compiler.rs::conn.execute_batch(INIT_SCHEMA)`) — there is no migration path for
files a user already has.

This feature needs one: a `PRAGMA user_version` check plus an idempotent "upgrade
schema" step run when the reader opens a book (`reader/src-tauri/src/commands.rs::
open_book`), so old files gain `annotation_layers` / `layer_id` /
`include_in_export` in place the first time a newer reader opens them, without
disturbing existing annotations (which all implicitly become layer 1, "My notes").
That migration mechanism is itself reusable infrastructure worth having regardless
of what ships next.

## Explicitly deferred / not needed yet

- **Cryptographic attribution** (`author_id` as an Ed25519 pubkey, signed
  annotations) — `author_id` already exists as free text, so this can layer in
  later with no schema change. No identity/keypair UX exists in the app today;
  don't build it speculatively.
- **Per-annotation UUID for fine-grained merge/dedup** — not needed under the
  "replace a layer wholesale on re-import" model above. Would only become necessary
  if we ever want partial/incremental layer updates instead of whole-layer replace.
- **Patterns 2 (magic link relay) and 3 (live sync server)** — see "Decision" above.
