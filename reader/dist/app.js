'use strict';

const { invoke } = window.__TAURI__.core;
const dialogApi = window.__TAURI__.dialog;

let currentBook = null;
let nodeElById = new Map();
let nodeDataById = new Map();
let annotationsByNode = new Map();
let currentAuthorName = 'Reader';

/* ================= HTML / span rendering ================= */

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

const SPAN_TAGS = {
  italic: ['<em>', '</em>'],
  bold: ['<strong>', '</strong>'],
  code: ['<code>', '</code>'],
  strikethrough: ['<s>', '</s>'],
  underline: ['<u>', '</u>'],
  highlight: ['<mark>', '</mark>'],
};

// User-annotation ranges (as opposed to the document's own formatting spans
// above) are rendered as a distinct, visibly tinted inline mark so a new
// highlight/note/voice-note is obvious immediately, not just via the small
// gutter dot.
const ANN_CLASS = { highlight: 'ann-highlight', text_note: 'ann-note', voice_note: 'ann-voice' };

// Wraps ranges of `content` in formatting tags per `spans` ({start, end, type,
// href?} in Unicode-codepoint offsets — the same coordinate space the compiler
// stored them in), mirroring format_markdown_spans in toolkit.rs but emitting
// HTML instead of Markdown. `annotations` ({start, end, annotation_type, id})
// layers user annotation ranges on top in the same coordinate space.
function renderSpans(content, spans, annotations) {
  const chars = Array.from(content || '');
  if ((!spans || spans.length === 0) && (!annotations || annotations.length === 0)) {
    return escapeHtml(chars.join(''));
  }

  const insertions = [];
  for (const span of spans || []) {
    if (span.start < 0 || span.end > chars.length || span.start >= span.end) continue;
    if (span.type === 'link' && span.href) {
      insertions.push([span.start, `<a href="${escapeHtml(span.href)}" target="_blank" rel="noopener">`]);
      insertions.push([span.end, '</a>']);
    } else if (SPAN_TAGS[span.type]) {
      const [open, close] = SPAN_TAGS[span.type];
      insertions.push([span.start, open]);
      insertions.push([span.end, close]);
    }
  }
  for (const ann of annotations || []) {
    if (ann.start < 0 || ann.end > chars.length || ann.start >= ann.end) continue;
    const cls = ANN_CLASS[ann.annotation_type] || 'ann-note';
    insertions.push([ann.start, `<mark class="ann-mark ${cls}" data-ann-id="${ann.id}">`]);
    insertions.push([ann.end, '</mark>']);
  }
  insertions.sort((a, b) => a[0] - b[0]);

  let html = '';
  let cursor = 0;
  for (const [idx, tag] of insertions) {
    html += escapeHtml(chars.slice(cursor, idx).join(''));
    html += tag;
    cursor = idx;
  }
  html += escapeHtml(chars.slice(cursor).join(''));
  return html;
}

/* ================= Node rendering ================= */

function annotationRanges(nodeId) {
  return (annotationsByNode.get(nodeId) || []).map((ann) => ({
    start: ann.start_offset,
    end: ann.end_offset,
    annotation_type: ann.annotation_type,
    id: ann.id,
  }));
}

function textNodeElement(tag, node, annotations) {
  const el = document.createElement(tag);
  el.className = 'node-text';
  el.innerHTML = renderSpans(node.content, node.attributes && node.attributes.spans, annotations);
  return el;
}

function renderNode(node) {
  const annotations = annotationRanges(node.id);
  const wrapper = document.createElement('div');
  wrapper.className = 'node';
  wrapper.id = `node-${node.id}`;
  wrapper.dataset.nodeId = String(node.id);

  switch (node.node_type) {
    case 'heading': {
      const level = Math.min(6, Math.max(1, (node.attributes && node.attributes.level) || 1));
      wrapper.appendChild(textNodeElement(`h${level}`, node, annotations));
      break;
    }
    case 'blockquote':
      wrapper.appendChild(textNodeElement('blockquote', node, annotations));
      break;
    case 'list': {
      const el = textNodeElement('p', node, annotations);
      el.classList.add('node-list');
      wrapper.appendChild(el);
      break;
    }
    case 'thematic_break':
      wrapper.appendChild(document.createElement('hr'));
      break;
    case 'table': {
      const table = document.createElement('table');
      const rows = (node.attributes && node.attributes.rows) || [];
      rows.forEach((cols, rowIdx) => {
        const tr = document.createElement('tr');
        (cols || []).forEach((cellText) => {
          const cell = document.createElement(rowIdx === 0 ? 'th' : 'td');
          cell.textContent = cellText;
          tr.appendChild(cell);
        });
        table.appendChild(tr);
      });
      wrapper.appendChild(table);
      break;
    }
    case 'image': {
      const figure = document.createElement('figure');
      const img = document.createElement('img');
      const assetId = node.attributes && node.attributes.asset_id;
      img.src = `weland-asset://asset/${assetId}`;
      img.alt = (node.attributes && node.attributes.alt) || '';
      figure.appendChild(img);
      const caption = node.attributes && node.attributes.caption;
      if (caption) {
        const fc = document.createElement('figcaption');
        fc.textContent = caption;
        figure.appendChild(fc);
      }
      wrapper.appendChild(figure);
      break;
    }
    default:
      // paragraph, footnote, anything else with plain content + spans
      wrapper.appendChild(textNodeElement('p', node, annotations));
  }

  return wrapper;
}

/* ================= Annotation gutter marks ================= */

const KIND_ICONS = {
  highlight:
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"><path d="M4 20l4-1 10-10-3-3L5 16l-1 4Z"/></svg>',
  text_note:
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2v10Z"/></svg>',
  voice_note:
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"><path d="M12 15a3 3 0 0 0 3-3V6a3 3 0 0 0-6 0v6a3 3 0 0 0 3 3Z"/><path d="M5 11a7 7 0 0 0 14 0M12 18v3"/></svg>',
};

const KIND_LABELS = { highlight: 'Highlight', text_note: 'Note', voice_note: 'Voice note' };

function renderAnnotationMarks(nodeWrapperEl, nodeId) {
  nodeWrapperEl.querySelectorAll('.gmark, .popover').forEach((el) => el.remove());
  const anns = annotationsByNode.get(nodeId) || [];

  anns.forEach((ann, idx) => {
    const top = 0.28 + idx * 1.9;

    const btn = document.createElement('button');
    btn.className = 'gmark';
    btn.dataset.kind = ann.annotation_type;
    btn.style.top = `${top}em`;
    btn.innerHTML = KIND_ICONS[ann.annotation_type] || KIND_ICONS.text_note;
    btn.setAttribute('aria-label', KIND_LABELS[ann.annotation_type] || 'Annotation');

    const pop = document.createElement('div');
    pop.className = 'popover';
    pop.style.top = `${top + 1.7}em`;

    const author = document.createElement('div');
    author.className = 'p-author';
    author.innerHTML = `<span>${escapeHtml(KIND_LABELS[ann.annotation_type] || 'Annotation')} · ${escapeHtml(ann.author_name)}</span>`;
    pop.appendChild(author);

    if (ann.annotation_type === 'voice_note' && ann.asset_id) {
      const audio = document.createElement('audio');
      audio.controls = true;
      audio.src = `weland-asset://asset/${ann.asset_id}`;
      pop.appendChild(audio);
    } else if (ann.comment) {
      const p = document.createElement('div');
      p.textContent = ann.comment;
      pop.appendChild(p);
    } else if (ann.selected_text) {
      const p = document.createElement('div');
      p.textContent = `"${ann.selected_text}"`;
      pop.appendChild(p);
    }

    const actions = document.createElement('div');
    actions.className = 'p-actions';

    if (ann.annotation_type === 'text_note') {
      const editBtn = document.createElement('button');
      editBtn.type = 'button';
      editBtn.className = 'p-action';
      editBtn.textContent = 'Edit';
      editBtn.addEventListener('click', (e) => {
        e.preventDefault();
        openNoteComposer(
          { nodeId: ann.node_id, start: ann.start_offset, end: ann.end_offset, text: ann.selected_text },
          pop.getBoundingClientRect(),
          ann,
        );
      });
      actions.appendChild(editBtn);
    }

    const delBtn = document.createElement('button');
    delBtn.type = 'button';
    delBtn.className = 'p-action p-action-danger';
    delBtn.textContent = 'Delete';
    delBtn.addEventListener('click', async (e) => {
      e.preventDefault();
      if (!confirm(`Delete this ${(KIND_LABELS[ann.annotation_type] || 'annotation').toLowerCase()}?`)) return;
      try {
        await invoke('delete_annotation', { id: ann.id });
        removeAnnotationLocally(ann.id, ann.node_id);
      } catch (err) {
        console.error('Failed to delete annotation', err);
      }
    });
    actions.appendChild(delBtn);

    pop.appendChild(actions);

    nodeWrapperEl.appendChild(btn);
    nodeWrapperEl.appendChild(pop);
  });
}

// Rebuilds a single node's DOM (text content + gutter marks) from the
// current `annotationsByNode` state — used whenever an annotation on that
// node is created, edited, or removed.
function rerenderNode(nodeId) {
  const node = nodeDataById.get(nodeId);
  const oldWrapper = nodeElById.get(nodeId);
  if (!node || !oldWrapper) return;
  const newWrapper = renderNode(node);
  oldWrapper.replaceWith(newWrapper);
  nodeElById.set(nodeId, newWrapper);
  renderAnnotationMarks(newWrapper, nodeId);
}

function removeAnnotationLocally(id, nodeId) {
  const list = annotationsByNode.get(nodeId) || [];
  annotationsByNode.set(nodeId, list.filter((a) => a.id !== id));
  rerenderNode(nodeId);
}

function addAnnotation(ann) {
  if (!annotationsByNode.has(ann.node_id)) annotationsByNode.set(ann.node_id, []);
  annotationsByNode.get(ann.node_id).push(ann);
  // Rebuild the whole node, not just the gutter marks: the new annotation
  // range needs an inline <mark> tinted into the node's own text content too.
  rerenderNode(ann.node_id);
}

/* ================= Table of contents ================= */

let tocLinkByNodeId = new Map();
let tocTargets = []; // [{ nodeId, el }] in document order, for scroll-spy
let activeTocNodeId = null;

function setActiveTocLink(link) {
  document.querySelectorAll('.toc a.active').forEach((el) => el.classList.remove('active'));
  if (link) link.classList.add('active');
}

function renderToc(tocEntries) {
  tocLinkByNodeId = new Map();
  const byParent = new Map();
  for (const entry of tocEntries) {
    const key = entry.parent_id == null ? 'root' : entry.parent_id;
    if (!byParent.has(key)) byParent.set(key, []);
    byParent.get(key).push(entry);
  }

  function buildList(parentKey) {
    const children = byParent.get(parentKey) || [];
    if (children.length === 0) return null;
    const ol = document.createElement('ol');
    for (const entry of children) {
      const li = document.createElement('li');
      const a = document.createElement('a');
      a.href = '#';
      a.textContent = entry.title;
      a.addEventListener('click', (e) => {
        e.preventDefault();
        activeTocNodeId = entry.target_node_id;
        setActiveTocLink(a);
        if (entry.target_node_id != null) jumpToNode(entry.target_node_id);
      });
      if (entry.target_node_id != null) tocLinkByNodeId.set(entry.target_node_id, a);
      li.appendChild(a);
      const nested = buildList(entry.id);
      if (nested) li.appendChild(nested);
      ol.appendChild(li);
    }
    return ol;
  }

  const list = document.getElementById('tocList');
  list.innerHTML = '';
  const built = buildList('root');
  if (built) while (built.firstChild) list.appendChild(built.firstChild);
}

function jumpToNode(nodeId) {
  const el = nodeElById.get(nodeId);
  if (el) el.scrollIntoView({ behavior: 'smooth', block: 'start' });
}

/* ================= TOC scroll-spy ================= */

function updateActiveTocEntry() {
  if (tocTargets.length === 0) return;
  const pane = document.getElementById('readingPane');
  const threshold = pane.getBoundingClientRect().top + 96;

  let current = tocTargets[0];
  for (const target of tocTargets) {
    if (!target.el) continue;
    if (target.el.getBoundingClientRect().top <= threshold) {
      current = target;
    } else {
      break;
    }
  }

  if (current.nodeId === activeTocNodeId) return;
  activeTocNodeId = current.nodeId;
  const link = tocLinkByNodeId.get(current.nodeId);
  setActiveTocLink(link);
  if (link) link.scrollIntoView({ block: 'nearest' });
}

let tocScrollSpyScheduled = false;
document.getElementById('readingPane').addEventListener('scroll', () => {
  if (tocScrollSpyScheduled) return;
  tocScrollSpyScheduled = true;
  requestAnimationFrame(() => {
    tocScrollSpyScheduled = false;
    updateActiveTocEntry();
  });
});

/* ================= Book loading ================= */

function renderBook(book) {
  currentBook = book;
  annotationsByNode = new Map();
  for (const ann of book.annotations) {
    if (!annotationsByNode.has(ann.node_id)) annotationsByNode.set(ann.node_id, []);
    annotationsByNode.get(ann.node_id).push(ann);
  }

  document.getElementById('bookTitle').textContent = book.metadata.title || 'Untitled';
  document.getElementById('bookByline').textContent = book.metadata.author ? `by ${book.metadata.author}` : '';

  const article = document.getElementById('readingArticle');
  article.innerHTML = '';
  nodeElById = new Map();
  nodeDataById = new Map();
  for (const node of book.nodes) {
    nodeDataById.set(node.id, node);
    const el = renderNode(node);
    article.appendChild(el);
    nodeElById.set(node.id, el);
    renderAnnotationMarks(el, node.id);
  }

  renderToc(book.toc);
  tocTargets = book.nodes
    .filter((node) => tocLinkByNodeId.has(node.id))
    .map((node) => ({ nodeId: node.id, el: nodeElById.get(node.id) }));
  activeTocNodeId = null;

  document.getElementById('emptyState').hidden = true;
  document.getElementById('appFrame').hidden = false;
  updateActiveTocEntry();
}

/* ================= Author name ================= */

function setAuthorButtonLabel() {
  document.getElementById('authorBtn').textContent = currentAuthorName;
}

function openAuthorModal(prefill, allowCancel) {
  const modal = document.getElementById('authorModal');
  document.getElementById('authorNameInput').value = prefill;
  document.getElementById('authorCancel').hidden = !allowCancel;
  modal.hidden = false;
  const input = document.getElementById('authorNameInput');
  input.focus();
  input.select();
}

function closeAuthorModal() {
  document.getElementById('authorModal').hidden = true;
}

document.getElementById('authorCancel').addEventListener('click', closeAuthorModal);

document.getElementById('authorSave').addEventListener('click', async () => {
  const name = document.getElementById('authorNameInput').value.trim();
  if (!name) return;
  try {
    await invoke('set_author_name', { name });
    currentAuthorName = name;
    setAuthorButtonLabel();
  } catch (err) {
    console.error('Failed to save author name', err);
  }
  closeAuthorModal();
});

document.getElementById('authorBtn').addEventListener('click', () => {
  openAuthorModal(currentAuthorName, true);
});

(async function initAuthor() {
  try {
    const info = await invoke('get_author_name');
    currentAuthorName = info.name;
    setAuthorButtonLabel();
    if (!info.is_saved) {
      openAuthorModal(info.name, false);
    }
  } catch (err) {
    console.error('Failed to load author name', err);
    setAuthorButtonLabel();
  }
})();

document.getElementById('openBookBtn').addEventListener('click', async () => {
  const errorEl = document.getElementById('openError');
  errorEl.hidden = true;
  try {
    const path = await dialogApi.open({
      multiple: false,
      filters: [{ name: 'Weland book', extensions: ['wld'] }],
    });
    if (!path) return;
    const book = await invoke('open_book', { path });
    renderBook(book);
  } catch (err) {
    errorEl.textContent = String(err);
    errorEl.hidden = false;
  }
});

/* ================= Search ================= */

const searchInput = document.getElementById('searchInput');
let searchDebounce;

searchInput.addEventListener('input', () => {
  clearTimeout(searchDebounce);
  const q = searchInput.value.trim();
  if (!q) {
    document.getElementById('searchResults').hidden = true;
    return;
  }
  searchDebounce = setTimeout(() => runSearch(q), 250);
});

searchInput.addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    clearTimeout(searchDebounce);
    runSearch(searchInput.value.trim());
  }
});

async function runSearch(query) {
  if (!query || !currentBook) return;
  try {
    const hits = await invoke('search_book', { query, limit: 20 });
    renderSearchResults(query, hits);
  } catch (err) {
    console.error('Search failed', err);
  }
}

function renderSearchResults(query, hits) {
  const panel = document.getElementById('searchResults');
  const list = document.getElementById('searchResultsList');
  document.getElementById('searchResultsTitle').textContent =
    `${hits.length} result${hits.length === 1 ? '' : 's'} for "${query}"`;
  list.innerHTML = '';

  for (const hit of hits) {
    const li = document.createElement('li');
    const btn = document.createElement('button');
    const snippetHtml = escapeHtml(hit.snippet).split('«').join('<mark>').split('»').join('</mark>');
    btn.innerHTML = `<span class="sr-type">${escapeHtml(hit.node_type)}</span>${snippetHtml}`;
    btn.addEventListener('click', () => {
      jumpToNode(hit.node_id);
      panel.hidden = true;
    });
    li.appendChild(btn);
    list.appendChild(li);
  }
  panel.hidden = false;
}

document.getElementById('closeSearchResults').addEventListener('click', () => {
  document.getElementById('searchResults').hidden = true;
});

/* ================= Day / night reading mode ================= */

document.getElementById('modeToggle').addEventListener('click', () => {
  const frame = document.getElementById('appFrame');
  const dark = frame.getAttribute('data-mode') === 'dark';
  frame.setAttribute('data-mode', dark ? 'light' : 'dark');
});

/* ================= Text selection -> annotate ================= */

let pendingSelection = null; // { nodeId, start, end, text }

function closestNodeText(node) {
  const el = node.nodeType === Node.ELEMENT_NODE ? node : node.parentElement;
  return el ? el.closest('.node-text') : null;
}

// Converts a DOM (node, offset) position into a Unicode-codepoint offset
// within `root`'s full text — the same coordinate space `content` character
// offsets already use, so no new indexing scheme is introduced.
function textOffsetWithin(root, targetNode, targetOffset) {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let total = 0;
  let current;
  while ((current = walker.nextNode())) {
    if (current === targetNode) {
      return total + Array.from(current.textContent.slice(0, targetOffset)).length;
    }
    total += Array.from(current.textContent).length;
  }
  return total;
}

document.getElementById('readingPane').addEventListener('mouseup', () => {
  const sel = window.getSelection();
  if (!sel || sel.isCollapsed || sel.rangeCount === 0) {
    hideSelectionToolbar();
    return;
  }
  const range = sel.getRangeAt(0);
  const startText = closestNodeText(range.startContainer);
  const endText = closestNodeText(range.endContainer);
  if (!startText || startText !== endText) {
    // A selection spanning two nodes isn't representable — annotations
    // anchor to exactly one node_id in the schema.
    hideSelectionToolbar();
    return;
  }

  const nodeWrapper = startText.closest('.node');
  const nodeId = Number(nodeWrapper.dataset.nodeId);
  const a = textOffsetWithin(startText, range.startContainer, range.startOffset);
  const b = textOffsetWithin(startText, range.endContainer, range.endOffset);
  if (a === b) {
    hideSelectionToolbar();
    return;
  }

  pendingSelection = { nodeId, start: Math.min(a, b), end: Math.max(a, b), text: sel.toString() };
  showSelectionToolbar(range.getBoundingClientRect());
});

function showSelectionToolbar(rect) {
  const toolbar = document.getElementById('selectionToolbar');
  toolbar.style.left = `${rect.left + rect.width / 2}px`;
  toolbar.style.top = `${Math.max(8, rect.top - 44)}px`;
  toolbar.style.transform = 'translateX(-50%)';
  toolbar.hidden = false;
}

function hideSelectionToolbar() {
  document.getElementById('selectionToolbar').hidden = true;
}

document.getElementById('selectionToolbar').addEventListener('click', async (e) => {
  const action = e.target.closest('button')?.dataset.action;
  if (!action || !pendingSelection) return;

  const sel = { ...pendingSelection };
  const toolbarRect = document.getElementById('selectionToolbar').getBoundingClientRect();
  hideSelectionToolbar();

  if (action === 'highlight') {
    try {
      const ann = await invoke('create_highlight', {
        nodeId: sel.nodeId,
        startOffset: sel.start,
        endOffset: sel.end,
        selectedText: sel.text,
        authorName: currentAuthorName,
      });
      addAnnotation(ann);
    } catch (err) {
      console.error('Failed to save highlight', err);
    }
  } else if (action === 'note') {
    openNoteComposer(sel, toolbarRect);
  } else if (action === 'record') {
    startRecording(sel);
  }
});

/* ================= Note composer ================= */

let notePending = null;
let noteEditId = null; // set when editing an existing note instead of creating one

function openNoteComposer(sel, anchorRect, existingAnn) {
  notePending = sel;
  noteEditId = existingAnn ? existingAnn.id : null;
  const composer = document.getElementById('noteComposer');
  composer.style.left = `${anchorRect.left}px`;
  composer.style.top = `${anchorRect.top + 6}px`;
  document.getElementById('noteText').value = existingAnn ? existingAnn.comment || '' : '';
  document.getElementById('noteSave').textContent = existingAnn ? 'Update' : 'Save';
  composer.hidden = false;
  document.getElementById('noteText').focus();
}

document.getElementById('noteCancel').addEventListener('click', () => {
  document.getElementById('noteComposer').hidden = true;
  notePending = null;
  noteEditId = null;
});

document.getElementById('noteSave').addEventListener('click', async () => {
  const text = document.getElementById('noteText').value.trim();
  if (!text || !notePending) return;
  try {
    if (noteEditId != null) {
      const ann = await invoke('update_note', { id: noteEditId, comment: text });
      const list = annotationsByNode.get(ann.node_id) || [];
      const idx = list.findIndex((a) => a.id === ann.id);
      if (idx !== -1) list[idx] = ann;
      rerenderNode(ann.node_id);
    } else {
      const ann = await invoke('create_text_note', {
        nodeId: notePending.nodeId,
        startOffset: notePending.start,
        endOffset: notePending.end,
        selectedText: notePending.text,
        comment: text,
        authorName: currentAuthorName,
      });
      addAnnotation(ann);
    }
  } catch (err) {
    console.error('Failed to save note', err);
  }
  document.getElementById('noteComposer').hidden = true;
  notePending = null;
  noteEditId = null;
});

/* ================= Voice notes ================= */

let mediaRecorder = null;
let recordedChunks = [];
let recordingPending = null;
let recordingStartedAt = 0;
let recordingTimer = null;

async function startRecording(sel) {
  recordingPending = sel;
  try {
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    recordedChunks = [];
    mediaRecorder = new MediaRecorder(stream);
    mediaRecorder.ondataavailable = (e) => {
      if (e.data.size > 0) recordedChunks.push(e.data);
    };
    mediaRecorder.onstop = onRecordingStop;
    mediaRecorder.start();

    recordingStartedAt = Date.now();
    document.getElementById('recTime').textContent = '0:00';
    document.getElementById('voicePill').hidden = false;
    recordingTimer = setInterval(() => {
      const secs = Math.floor((Date.now() - recordingStartedAt) / 1000);
      document.getElementById('recTime').textContent = `${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, '0')}`;
    }, 250);
  } catch (err) {
    console.error('Microphone access failed', err);
    recordingPending = null;
  }
}

document.getElementById('stopRecordingBtn').addEventListener('click', () => {
  if (mediaRecorder && mediaRecorder.state !== 'inactive') mediaRecorder.stop();
});

async function onRecordingStop() {
  clearInterval(recordingTimer);
  document.getElementById('voicePill').hidden = true;
  mediaRecorder.stream.getTracks().forEach((t) => t.stop());

  if (!recordingPending) return;
  const sel = recordingPending;
  recordingPending = null;

  const mimeType = mediaRecorder.mimeType || 'audio/webm';
  const blob = new Blob(recordedChunks, { type: mimeType });
  const audioBase64 = await blobToBase64(blob);

  try {
    const ann = await invoke('save_voice_note', {
      nodeId: sel.nodeId,
      startOffset: sel.start,
      endOffset: sel.end,
      selectedText: sel.text,
      audioBase64,
      mimeType,
      authorName: currentAuthorName,
    });
    addAnnotation(ann);
  } catch (err) {
    console.error('Failed to save voice note', err);
  }
}

function blobToBase64(blob) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onloadend = () => {
      const dataUrl = reader.result;
      resolve(dataUrl.substring(dataUrl.indexOf(',') + 1));
    };
    reader.onerror = reject;
    reader.readAsDataURL(blob);
  });
}

/* ================= Misc ================= */

document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape') return;
  hideSelectionToolbar();
  document.getElementById('noteComposer').hidden = true;
  document.getElementById('searchResults').hidden = true;
});
