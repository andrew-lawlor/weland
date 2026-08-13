#!/usr/bin/env node

import fs from "node:fs";
import crypto from "node:crypto";
import Database from "better-sqlite3";
import JSZip from "jszip";
import { JSDOM } from "jsdom";

// ============================================================================
// DATABASE SCHEMA
// ============================================================================
const INIT_SCHEMA = `
CREATE TABLE IF NOT EXISTS metadata (
  key TEXT PRIMARY KEY,
  value TEXT
);

CREATE TABLE IF NOT EXISTS ast_nodes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  parent_id INTEGER REFERENCES ast_nodes(id) ON DELETE CASCADE,
                                      ordinal INTEGER NOT NULL,
                                      node_type TEXT NOT NULL,
                                      content TEXT,
                                      attributes JSON
);

CREATE TABLE IF NOT EXISTS assets (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  hash TEXT UNIQUE NOT NULL,
  mime_type TEXT NOT NULL,
  data BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS user_annotations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  node_id INTEGER NOT NULL REFERENCES ast_nodes(id) ON DELETE CASCADE,

                                             -- Precise character selection bounds in node.content
                                             start_offset INTEGER NOT NULL,
                                             end_offset INTEGER NOT NULL,
                                             selected_text TEXT,

                                             -- Annotation Type: 'highlight', 'text_note', 'voice_note', 'ink_sketch'
                                             type TEXT NOT NULL,

                                             -- Payload fields
                                             comment TEXT,                               -- Textual note or voice transcript
                                             asset_id INTEGER REFERENCES assets(id),     -- Linked BLOB (voice note audio, SVG ink vector)

                                             -- Provenance & Metadata
                                             author_name TEXT DEFAULT 'Local Reader',
                                             author_id TEXT,                             -- UUID or PubKey
                                             device_id TEXT,                             -- Optional client hardware identifier
                                             created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                                             updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

CREATE VIRTUAL TABLE IF NOT EXISTS fts_nodes USING fts5(
  content,
  content='ast_nodes',
  content_rowid='id'
);
`;

// ============================================================================
// HELPER UTILITIES
// ============================================================================
function normalizePath(p) {
  const parts = p.split("/");
  const stack = [];
  for (const part of parts) {
    if (part === "." || part === "") continue;
    if (part === "..") {
      stack.pop();
    } else {
      stack.push(part);
    }
  }
  return stack.join("/");
}

function getMimeType(filePath) {
  if (filePath.endsWith(".png")) return "image/png";
  if (filePath.endsWith(".webp")) return "image/webp";
  if (filePath.endsWith(".svg")) return "image/svg+xml";
  return "image/jpeg";
}

/**
 * Recursively walks DOM nodes to extract plain text and record
 * inline formatting character spans (start/end offsets).
 */
function extractTextAndSpans(element) {
  let text = "";
  const spans = [];

  function walk(node) {
    // Node.TEXT_NODE
    if (node.nodeType === 3) {
      text += node.textContent;
    }
    // Node.ELEMENT_NODE
    else if (node.nodeType === 1) {
      const tag = node.tagName.toLowerCase();

      // Ignore inner footnote anchor tags in text calculation (handled separately)
      if (tag === "sup" || (tag === "a" && node.getAttribute("href")?.includes("#"))) {
        return;
      }

      const start = text.length;

      for (const child of node.childNodes) {
        walk(child);
      }

      const end = text.length;
      if (start < end) {
        if (tag === "em" || tag === "i") spans.push({ start, end, type: "italic" });
        if (tag === "strong" || tag === "b") spans.push({ start, end, type: "bold" });
        if (tag === "code") spans.push({ start, end, type: "code" });
        if (tag === "a" && node.getAttribute("href")) {
          spans.push({ start, end, type: "link", href: node.getAttribute("href") });
        }
      }
    }
  }

  walk(element);

  // Collapse whitespace while keeping span offset mapping accurate
  const cleanedText = text.replace(/\s+/g, " ").trim();

  return { text: cleanedText, spans };
}

// ============================================================================
// MAIN COMPILER ENGINE
// ============================================================================
export async function compileEpub(inputPath, outputPath) {
  console.log(`[WERG] Ingesting EPUB: ${inputPath}`);

  // Reset Dev Output
  if (fs.existsSync(outputPath)) {
    fs.unlinkSync(outputPath);
  }

  const db = new Database(outputPath);
  db.exec(INIT_SCHEMA);

  // Load and Unpack Zip File into Memory
  const fileBuffer = fs.readFileSync(inputPath);
  const zip = await JSZip.loadAsync(fileBuffer);

  // Find OPF Manifest Path from META-INF/container.xml
  const containerFile = zip.file("META-INF/container.xml");
  if (!containerFile) throw new Error("Invalid EPUB: META-INF/container.xml missing");
  const containerXml = await containerFile.async("string");

  const containerDom = new JSDOM(containerXml, { contentType: "text/xml" });
  const rootfileEl = containerDom.window.document.querySelector("rootfile");
  const opfPath = rootfileEl ? rootfileEl.getAttribute("full-path") : null;
  if (!opfPath) throw new Error("Could not locate OPF manifest path");

  // Read OPF Manifest to Determine Reading Order (Spine)
  const opfFile = zip.file(opfPath);
  if (!opfFile) throw new Error(`OPF file missing at ${opfPath}`);
  const opfXml = await opfFile.async("string");
  const opfDom = new JSDOM(opfXml, { contentType: "text/xml" });
  const opfDoc = opfDom.window.document;

  const opfDir = opfPath.includes("/") ? opfPath.substring(0, opfPath.lastIndexOf("/")) : "";

  // Prepare SQL Statements
  const stmtMeta = db.prepare(`
  INSERT OR REPLACE INTO metadata (key, value) VALUES (?, ?)
  `);
  const stmtNode = db.prepare(`
  INSERT INTO ast_nodes (parent_id, ordinal, node_type, content, attributes)
  VALUES (?, ?, ?, ?, ?)
  `);
  const stmtFTS = db.prepare(`INSERT INTO fts_nodes (rowid, content) VALUES (?, ?)`);
  const stmtAsset = db.prepare(`
  INSERT INTO assets (hash, mime_type, data) VALUES (?, ?, ?)
  ON CONFLICT(hash) DO UPDATE SET id=id RETURNING id
  `);

  // ==========================================================================
  // EXTRACT BOOK METADATA & COVER IMAGE
  // ==========================================================================
  const getDcTag = (tag) => {
    const el = opfDoc.querySelector(`metadata > ${tag}`) || opfDoc.querySelector(`${tag}`);
    return el ? el.textContent.trim() : null;
  };

  const title = getDcTag("dc\\:title") || getDcTag("title") || "Unknown Title";
  const author = getDcTag("dc\\:creator") || getDcTag("creator") || "Unknown Author";
  const language = getDcTag("dc\\:language") || getDcTag("language") || "en";
  const description = getDcTag("dc\\:description") || getDcTag("description") || "";
  const identifier = getDcTag("dc\\:identifier") || getDcTag("identifier") || "";

  stmtMeta.run("title", title);
  stmtMeta.run("author", author);
  stmtMeta.run("language", language);
  if (description) stmtMeta.run("description", description);
  if (identifier) stmtMeta.run("identifier", identifier);

  // Cover Image Extraction
  let coverHref = null;

  // Strategy A: EPUB 3 (item with property 'cover-image')
  const epub3CoverItem = opfDoc.querySelector("manifest > item[properties*='cover-image']");
  if (epub3CoverItem) {
    coverHref = epub3CoverItem.getAttribute("href");
  }

  // Strategy B: EPUB 2 (<meta name="cover" content="item_id" />)
  if (!coverHref) {
    const metaCoverEl = opfDoc.querySelector("metadata > meta[name='cover']");
    if (metaCoverEl) {
      const coverId = metaCoverEl.getAttribute("content");
      const coverItem = opfDoc.querySelector(`manifest > item[id='${coverId}']`);
      if (coverItem) coverHref = coverItem.getAttribute("href");
    }
  }

  // Strategy C: Fallback search for any manifest item with "cover" in ID/href
  if (!coverHref) {
    const fallbackItem = opfDoc.querySelector("manifest > item[id*='cover'], manifest > item[href*='cover']");
    if (fallbackItem) coverHref = fallbackItem.getAttribute("href");
  }

  if (coverHref) {
    const fullCoverPath = normalizePath(opfDir ? `${opfDir}/${coverHref}` : coverHref);
    const coverFile = zip.file(fullCoverPath);

    if (coverFile) {
      const coverBuffer = await coverFile.async("nodebuffer");
      const hashHex = crypto.createHash("sha256").update(coverBuffer).digest("hex");
      const mimeType = getMimeType(fullCoverPath);

      stmtAsset.run(hashHex, mimeType, coverBuffer);
      const assetRes = db.prepare("SELECT id FROM assets WHERE hash = ?").get(hashHex);

      if (assetRes) {
        stmtMeta.run("cover_asset_id", assetRes.id.toString());
      }
    }
  }

  console.log(`[WERG] Extracted Metadata: "${title}" by ${author}`);

  // ==========================================================================
  // PARSE READING SPINE (CHAPTERS)
  // ==========================================================================
  const manifestMap = new Map();
  for (const item of opfDoc.querySelectorAll("manifest > item")) {
    manifestMap.set(item.getAttribute("id"), {
      href: item.getAttribute("href"),
                    mime: item.getAttribute("media-type"),
    });
  }

  const chapterPaths = [];
  for (const itemref of opfDoc.querySelectorAll("spine > itemref")) {
    const idref = itemref.getAttribute("idref");
    const item = manifestMap.get(idref);
    if (item) {
      const fullPath = opfDir ? `${opfDir}/${item.href}` : item.href;
      chapterPaths.push(normalizePath(fullPath));
    }
  }

  console.log(`[WERG] Found ${chapterPaths.length} chapters in reading spine.`);

  // 1. Pre-load and parse chapters ASYNCHRONOUSLY first
  const compiledChapters = [];

  for (const chapterPath of chapterPaths) {
    const chapterFile = zip.file(chapterPath);
    if (!chapterFile) continue;

    const htmlContent = await chapterFile.async("string");
    const dom = new JSDOM(htmlContent);
    const doc = dom.window.document;
    const body = doc.querySelector("body");
    if (!body) continue;

    const elements = Array.from(body.querySelectorAll("h1, h2, h3, h4, h5, h6, p, blockquote, ul, ol, img, hr, table"));
    const processedElements = [];

    for (const elem of elements) {
      const tagName = elem.tagName.toLowerCase();

      // --- IMAGES ---
      if (tagName === "img") {
        const src = elem.getAttribute("src");
        if (src) {
          const chapterDir = chapterPath.includes("/") ? chapterPath.substring(0, chapterPath.lastIndexOf("/")) : "";
          const imgPath = normalizePath(chapterDir ? `${chapterDir}/${src}` : src);
          const imgFile = zip.file(imgPath);

          if (imgFile) {
            const imgBuffer = await imgFile.async("nodebuffer");
            const hashHex = crypto.createHash("sha256").update(imgBuffer).digest("hex");
            const mimeType = getMimeType(imgPath);

            processedElements.push({
              type: "img",
              hashHex,
              mimeType,
              imgBuffer,
              alt: elem.getAttribute("alt") || "",
                                   caption: elem.getAttribute("title") || "",
            });
          }
        }
        continue;
      }

      // --- TABLES ---
      if (tagName === "table") {
        const rows = [];
        const trs = elem.querySelectorAll("tr");

        for (const tr of trs) {
          const rowData = [];
          const cells = tr.querySelectorAll("th, td");
          for (const cell of cells) {
            rowData.push(cell.textContent.trim().replace(/\s+/g, " "));
          }
          if (rowData.length > 0) rows.push(rowData);
        }

        const plainText = rows.map((r) => r.join(" ")).join(" ");

        processedElements.push({
          type: "table",
          text: plainText,
          rows,
          chapterPath,
        });
        continue;
      }

      // --- TEXT ELEMENTS (Headings, Paragraphs, Blockquotes, Lists) ---
      const { text, spans } = extractTextAndSpans(elem);

      processedElements.push({
        type: "text",
        elem,
        tagName,
        text,
        spans,
        chapterPath,
      });
    }

    compiledChapters.push({ chapterPath, doc, elements: processedElements });
  }

  let ordinal = 0;

  function insertASTNode(nodeType, content, attributes, parentId = null) {
    const attrJson = attributes && Object.keys(attributes).length > 0 ? JSON.stringify(attributes) : null;
    const info = stmtNode.run(parentId, ordinal, nodeType, content, attrJson);
    const nodeId = info.lastInsertRowid;
    ordinal++;

    if (content) {
      stmtFTS.run(nodeId, content);
    }

    return nodeId;
  }

  // 2. Execute purely SYNCHRONOUS database transaction block
  const runCompilation = db.transaction((chapters) => {
    for (const chapter of chapters) {
      for (const item of chapter.elements) {
        if (item.type === "img") {
          stmtAsset.run(item.hashHex, item.mimeType, item.imgBuffer);
          const assetRes = db.prepare("SELECT id FROM assets WHERE hash = ?").get(item.hashHex);

          insertASTNode("image", null, {
            asset_id: assetRes.id,
            alt: item.alt,
            caption: item.caption,
          });
          continue;
        }

        if (item.type === "table") {
          insertASTNode("table", item.text, {
            rows: item.rows,
            source_file: item.chapterPath,
          });
          continue;
        }

        const { elem, tagName, text, spans, chapterPath } = item;

        // Headings
        if (tagName.startsWith("h") && tagName.length === 2) {
          const level = parseInt(tagName.substring(1), 10);
          if (text) {
            insertASTNode("heading", text, { level, spans });
          }
          continue;
        }

        // Scene Breaks
        if (tagName === "hr") {
          insertASTNode("thematic_break", null, {});
          continue;
        }

        // Paragraphs, Blockquotes, Lists
        if (text) {
          let nodeType = "paragraph";
          if (tagName === "blockquote") nodeType = "blockquote";
          if (tagName === "ul" || tagName === "ol") nodeType = "list";

          const parentNodeId = insertASTNode(nodeType, text, { source_file: chapterPath, spans });

          // Extract Footnotes as Child AST Nodes
          const noteRefs = elem.querySelectorAll("a[href*='#'], sup a");
          for (const ref of noteRefs) {
            const href = ref.getAttribute("href");
            if (href && href.includes("#")) {
              const noteId = href.split("#")[1];
              const targetNoteEl = chapter.doc.getElementById(noteId);
              if (targetNoteEl) {
                const { text: noteText, spans: noteSpans } = extractTextAndSpans(targetNoteEl);
                if (noteText) {
                  insertASTNode(
                    "footnote",
                    noteText,
                    {
                      anchor_id: noteId,
                      label: ref.textContent.trim(),
                                spans: noteSpans,
                    },
                    parentNodeId
                  );
                }
              }
            }
          }
        }
      }
    }
  });

  runCompilation(compiledChapters);
  db.close();

  console.log(`[WERG] Successfully compiled database: ${outputPath}`);
}

// ============================================================================
// CLI ENTRYPOINT
// ============================================================================
const args = process.argv.slice(2);
const [command, input, output] = args;

if (command === "compile" && input && output) {
  try {
    await compileEpub(input, output);
  } catch (err) {
    console.error("[WERG ERROR]", err);
    process.exit(1);
  }
} else {
  console.log(`
  *WERG Node.js Compiler CLI

  Usage:
  node weland.js compile <input.epub> <output.sqlite>
  `);
}
