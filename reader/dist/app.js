'use strict';

const { invoke } = window.__TAURI__.core;
const dialogApi = window.__TAURI__.dialog;

let currentBook = null;

// Asset row IDs only reset per-.wld file, so weland-asset://asset/<id> can point
// at completely different bytes across books. The webview's resource cache keys
// purely on URL, so tag the query string with the open book's path to keep the
// URL — not just the id — unique per book and avoid serving a stale image.
function assetUrl(assetId) {
  return `weland-asset://asset/${assetId}?b=${encodeURIComponent(currentBook.path)}`;
}

// Native window.confirm() is a silent no-op on Tauri's Linux (webkit2gtk)
// backend — it never wires up the WebKit script-dialog signal needed to
// actually show it — so destructive actions need this custom modal instead.
function showConfirm(message, { title = 'Are you sure?', confirmLabel = 'Confirm', alertOnly = false } = {}) {
  return new Promise((resolve) => {
    const modal = document.getElementById('confirmModal');
    const okBtn = document.getElementById('confirmOk');
    const cancelBtn = document.getElementById('confirmCancel');
    document.getElementById('confirmTitle').textContent = title;
    document.getElementById('confirmMessage').textContent = message;
    okBtn.textContent = confirmLabel;
    // alertOnly is for pure informational messages (e.g. a completion
    // summary) where there's no real decision to make — Cancel would be
    // a dead end, and the danger-red styling implies a destructive action
    // that isn't happening here.
    cancelBtn.hidden = alertOnly;
    okBtn.classList.toggle('btn-danger', !alertOnly);
    modal.hidden = false;

    function cleanup(result) {
      modal.hidden = true;
      okBtn.removeEventListener('click', onOk);
      cancelBtn.removeEventListener('click', onCancel);
      modal.removeEventListener('click', onBackdrop);
      document.removeEventListener('keydown', onKey);
      resolve(result);
    }
    function onOk() { cleanup(true); }
    function onCancel() { cleanup(false); }
    function onBackdrop(e) { if (e.target === modal) cleanup(false); }
    function onKey(e) { if (e.key === 'Escape') cleanup(false); }

    okBtn.addEventListener('click', onOk);
    cancelBtn.addEventListener('click', onCancel);
    modal.addEventListener('click', onBackdrop);
    document.addEventListener('keydown', onKey);
  });
}

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
  stanza_number: ['<span class="stanza-num">', '</span>'],
  line_number: ['<span class="line-num">', '</span>'],
};

// User-annotation ranges (as opposed to the document's own formatting spans
// above) are rendered as a distinct, visibly tinted inline mark so a new
// highlight/note/voice-note is obvious immediately, not just via the small
// gutter dot.
const ANN_CLASS = { highlight: 'ann-highlight', text_note: 'ann-note', voice_note: 'ann-voice', search_flash: 'ann-flash' };

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

// Builds a real nested <ul>/<ol>/<li> tree from a list node's structured
// `attributes.items` (each item's own text/spans, plus an optional nested
// sublist in the same {ordered, items} shape) — not the flattened
// `node.content` string, which exists only for search/plain-text export.
// Per-item highlight/annotation overlay isn't wired up here; annotations on
// a list node anchor to that flattened content, not to individual items.
function buildListElement(listData) {
  const el = document.createElement(listData.ordered ? 'ol' : 'ul');
  for (const item of (listData && listData.items) || []) {
    const li = document.createElement('li');
    const textEl = document.createElement('span');
    textEl.className = 'node-text';
    textEl.innerHTML = renderSpans(item.text, item.spans, []);
    li.appendChild(textEl);
    if (item.sublist) {
      li.appendChild(buildListElement(item.sublist));
    }
    el.appendChild(li);
  }
  return el;
}

function renderNode(node, extraRanges) {
  const annotations = annotationRanges(node.id).concat(extraRanges || []);
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
    case 'verse_line': {
      const el = textNodeElement('p', node, annotations);
      el.classList.add('verse-line');
      if (node.attributes && node.attributes.stanza_start) {
        el.classList.add('verse-stanza-start');
      }
      if (node.attributes && node.attributes.verse_end) {
        el.classList.add('verse-end');
      }
      wrapper.appendChild(el);
      break;
    }
    case 'list': {
      const el = buildListElement(node.attributes || {});
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
      img.src = assetUrl(assetId);
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
    pop.dataset.annId = String(ann.id);
    pop.style.top = `${top + 1.7}em`;

    const author = document.createElement('div');
    author.className = 'p-author';
    author.innerHTML = `<span>${escapeHtml(KIND_LABELS[ann.annotation_type] || 'Annotation')} · ${escapeHtml(ann.author_name)}</span>`;
    pop.appendChild(author);

    if (ann.annotation_type === 'voice_note' && ann.asset_id) {
      const audio = document.createElement('audio');
      audio.controls = true;
      audio.src = assetUrl(ann.asset_id);
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
      const kind = (KIND_LABELS[ann.annotation_type] || 'annotation').toLowerCase();
      const ok = await showConfirm(`Delete this ${kind}? This can't be undone.`, {
        title: 'Delete annotation',
        confirmLabel: 'Delete',
      });
      if (!ok) return;
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

  attachInlineMarkHover(nodeWrapperEl);
}

// The inline <mark class="ann-mark"> wrapping the annotated text lives far
// from its .popover in the DOM (the popover is only ever adjacent to its
// .gmark gutter dot), so the gutter's plain CSS `:hover + .popover` trick
// can't reach it. Wire it up in JS instead, reusing the exact same popover
// element rather than building a second popup style. A short close delay
// lets the pointer travel from the (possibly distant) highlighted text over
// to the gutter popover to click Edit/Delete without it vanishing first.
function attachInlineMarkHover(nodeWrapperEl) {
  let closeTimer = null;
  const open = (pop) => {
    clearTimeout(closeTimer);
    pop.classList.add('popover-open');
  };
  const scheduleClose = (pop) => {
    clearTimeout(closeTimer);
    closeTimer = setTimeout(() => pop.classList.remove('popover-open'), 250);
  };

  nodeWrapperEl.querySelectorAll('.ann-mark[data-ann-id]').forEach((mark) => {
    const pop = nodeWrapperEl.querySelector(`.popover[data-ann-id="${mark.dataset.annId}"]`);
    if (!pop) return;
    mark.addEventListener('mouseenter', () => open(pop));
    mark.addEventListener('mouseleave', () => scheduleClose(pop));
    pop.addEventListener('mouseenter', () => open(pop));
    pop.addEventListener('mouseleave', () => scheduleClose(pop));
  });
}

// Rebuilds a single node's DOM (text content + gutter marks) from the
// current `annotationsByNode` state — used whenever an annotation on that
// node is created, edited, or removed.
function rerenderNode(nodeId, extraRanges) {
  const node = nodeDataById.get(nodeId);
  const oldWrapper = nodeElById.get(nodeId);
  if (!node || !oldWrapper) return;
  const newWrapper = renderNode(node, extraRanges);
  oldWrapper.replaceWith(newWrapper);
  nodeElById.set(nodeId, newWrapper);
  renderAnnotationMarks(newWrapper, nodeId);
  refreshAnnotationsUi();
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
let allNodeTargets = []; // [{ nodeId, el }] for every node, in document order, for resume-position
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

function easeInOutCubic(t) {
  return t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2;
}

// A hand-rolled scroll instead of scrollIntoView({behavior:'smooth'}) — the
// native version's duration/easing is fixed regardless of distance, which
// reads as slow and janky on a long jump and abrupt on a short one.
// Bumped on every call so a new scroll always supersedes one still in
// flight — without this, holding a key down (repeated keydown faster than
// the animation duration) stacks up multiple rAF loops that fight over
// scrollTop each frame, which looks janky and barely moves.
let scrollAnimId = 0;

function smoothScrollTo(container, targetTop, duration) {
  const startTop = container.scrollTop;
  const delta = targetTop - startTop;
  if (Math.abs(delta) < 1) return;

  if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) {
    container.scrollTop = targetTop;
    return;
  }

  const myId = ++scrollAnimId;
  const start = performance.now();
  function step(now) {
    if (myId !== scrollAnimId) return;
    const t = Math.min(1, (now - start) / duration);
    container.scrollTop = startTop + delta * easeInOutCubic(t);
    if (t < 1) requestAnimationFrame(step);
  }
  requestAnimationFrame(step);
}

function jumpToNode(nodeId) {
  const el = nodeElById.get(nodeId);
  const pane = document.getElementById('readingPane');
  if (!el || !pane) return;

  const targetTop = Math.max(0, pane.scrollTop + (el.getBoundingClientRect().top - pane.getBoundingClientRect().top) - 16);
  const distance = Math.abs(targetTop - pane.scrollTop);
  const duration = Math.min(650, Math.max(220, distance * 0.35));
  smoothScrollTo(pane, targetTop, duration);
}

/* ================= TOC scroll-spy ================= */

function updateActiveTocEntry() {
  if (tocTargets.length === 0) return;
  const pane = document.getElementById('readingPane');
  const threshold = pane.getBoundingClientRect().top + 96;

  // Starts at "none" rather than defaulting to the first TOC target — a book's
  // own TOC/nav often skips un-headed front matter (title page, copyright),
  // so sitting at the very top, before that first entry, is a real state
  // that deserves no highlight, not the first entry lit up as if we'd
  // already reached it.
  let current = null;
  for (const target of tocTargets) {
    if (!target.el) continue;
    if (target.el.getBoundingClientRect().top <= threshold) {
      current = target;
    } else {
      break;
    }
  }

  const newActiveId = current ? current.nodeId : null;
  if (newActiveId === activeTocNodeId) return;
  activeTocNodeId = newActiveId;
  const link = current ? tocLinkByNodeId.get(current.nodeId) : null;
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
  // #readingPane is one persistent element reused across every book — cancel
  // any scroll animation still in flight from whatever was open before
  // (an eased smoothScrollTo jump, or a held-arrow-key that hasn't seen its
  // keyup yet) so it can't keep nudging scrollTop after this book's content
  // replaces the old one. Without this, closing a book mid-scroll and
  // opening a new one can leave it opening already scrolled partway down.
  scrollAnimId++;
  heldScrollDirection = 0;

  currentBook = book;
  annotationsByNode = new Map();
  for (const ann of book.annotations) {
    if (!annotationsByNode.has(ann.node_id)) annotationsByNode.set(ann.node_id, []);
    annotationsByNode.get(ann.node_id).push(ann);
  }

  document.getElementById('bookTitle').textContent = book.metadata.title || 'Untitled';
  document.getElementById('bookByline').textContent = book.metadata.author ? `by ${book.metadata.author}` : '';

  const pane = document.getElementById('readingPane');
  // A fast fling in the previous book can leave WebKitGTK's native kinetic
  // scroll still animating this element's compositor-level scroll position
  // after the switch — a plain `scrollTop = 0` doesn't reliably cancel that
  // momentum, so the new book can end up visibly landing partway down.
  // Toggling overflow off and back on (with a forced reflow in between)
  // destroys that momentum state outright.
  pane.style.overflowY = 'hidden';
  pane.scrollTop = 0;
  void pane.offsetHeight;
  pane.style.overflowY = '';

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
  allNodeTargets = book.nodes.map((node) => ({ nodeId: node.id, el: nodeElById.get(node.id) }));
  activeTocNodeId = null;

  document.getElementById('emptyState').hidden = true;
  document.getElementById('appFrame').hidden = false;
  document.getElementById('annotationsPanel').hidden = true;
  restoreReadingPosition(book.last_position_node_id);
  updateActiveTocEntry();
  updateAnnotationsCount(allAnnotationsInOrder());

  // Safety net: if leftover kinetic momentum from the previous book is
  // still landing a frame or two later (the overflow toggle above should
  // normally prevent this), reassert the correct position once more after
  // things have settled. Bails if another book was opened in the meantime.
  requestAnimationFrame(() => {
    requestAnimationFrame(() => {
      if (currentBook !== book) return;
      if (book.last_position_node_id == null) {
        pane.scrollTop = 0;
      } else {
        restoreReadingPosition(book.last_position_node_id);
      }
    });
  });
}

// Jumps straight to a saved position with no animation — this is restoring
// where the reader already was, not a user-triggered navigation.
function restoreReadingPosition(nodeId) {
  if (nodeId == null) return;
  const el = nodeElById.get(nodeId);
  const pane = document.getElementById('readingPane');
  if (!el) return;
  pane.scrollTop = Math.max(0, pane.scrollTop + (el.getBoundingClientRect().top - pane.getBoundingClientRect().top));
}

// Finds whichever node is nearest the top of the reading pane right now —
// same "sorted insertion points, scan for the last one past the threshold"
// approach as the TOC scroll-spy, just over every node instead of only
// TOC targets.
function findTopVisibleNodeId() {
  if (allNodeTargets.length === 0) return null;
  const pane = document.getElementById('readingPane');
  const threshold = pane.getBoundingClientRect().top + 40;

  let current = allNodeTargets[0];
  for (const target of allNodeTargets) {
    if (!target.el) continue;
    if (target.el.getBoundingClientRect().top <= threshold) {
      current = target;
    } else {
      break;
    }
  }
  return current.nodeId;
}

async function saveReadingPosition() {
  if (!currentBook) return;
  const nodeId = findTopVisibleNodeId();
  if (nodeId == null) return;
  try {
    await invoke('update_reading_position', { path: currentBook.path, nodeId });
  } catch (err) {
    console.error('Failed to save reading position', err);
  }
}

let positionSaveTimeout = null;
document.getElementById('readingPane').addEventListener('scroll', () => {
  clearTimeout(positionSaveTimeout);
  positionSaveTimeout = setTimeout(saveReadingPosition, 600);
});

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

async function openBookAtPath(path) {
  const errorEl = document.getElementById('openError');
  errorEl.hidden = true;
  try {
    const book = await invoke('open_book', { path });
    renderBook(book);
  } catch (err) {
    errorEl.textContent = String(err);
    errorEl.hidden = false;
  }
}

function setCompilingOverlay(visible) {
  document.getElementById('compilingOverlay').hidden = !visible;
}

// Compiles an EPUB (via the same compiler the CLI uses) and opens the
// result. If writing to the default location (next to the source file)
// fails — read-only folder, no permission, etc. — offers a save dialog to
// pick somewhere else and retries there.
function nextPaint() {
  return new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
}

async function importEpub(inputPath, outputPath) {
  const errorEl = document.getElementById('openError');
  errorEl.hidden = true;
  setCompilingOverlay(true);
  // Force the overlay to actually paint before the (heavy) compile call starts.
  await nextPaint();
  try {
    const book = await invoke('import_epub', { inputPath, outputPath: outputPath || null });
    setCompilingOverlay(false);
    renderBook(book);
  } catch (err) {
    setCompilingOverlay(false);
    if (!outputPath) {
      const retryPath = await dialogApi.save({
        defaultPath: inputPath.replace(/\.epub$/i, '.wld'),
        filters: [{ name: 'Weland book', extensions: ['wld'] }],
      });
      if (retryPath) {
        await importEpub(inputPath, retryPath);
        return;
      }
    }
    errorEl.textContent = String(err);
    errorEl.hidden = false;
  }
}

document.getElementById('openBookBtn').addEventListener('click', async () => {
  const path = await dialogApi.open({
    multiple: false,
    filters: [{ name: 'Book (.wld or .epub)', extensions: ['wld', 'epub'] }],
  });
  if (!path) return;
  if (path.toLowerCase().endsWith('.epub')) {
    await importEpub(path);
  } else {
    await openBookAtPath(path);
  }
});

async function exportBook(path, title) {
  const destPath = await dialogApi.save({
    defaultPath: `${title}.wld`,
    filters: [{ name: 'Weland book', extensions: ['wld'] }],
  });
  if (!destPath) return;
  try {
    await invoke('export_book', { path, destPath });
  } catch (err) {
    const errorEl = document.getElementById('openError');
    errorEl.textContent = String(err);
    errorEl.hidden = false;
  }
}

async function exportLibrary() {
  const destDir = await dialogApi.open({ directory: true });
  if (!destDir) return;
  try {
    const result = await invoke('export_library', { destDir });
    const msg = `Exported ${result.exported.length} book(s)` +
      (result.failed.length ? `, ${result.failed.length} failed.` : '.');
    await showConfirm(msg, { title: 'Export complete', confirmLabel: 'OK', alertOnly: true });
  } catch (err) {
    console.error('Failed to export library', err);
  }
}

document.getElementById('exportLibraryBtn').addEventListener('click', exportLibrary);

async function importFolder(rootPath) {
  const errorEl = document.getElementById('openError');
  errorEl.hidden = true;
  const progressEl = document.getElementById('compilingProgress');

  setCompilingOverlay(true);
  progressEl.hidden = false;
  progressEl.textContent = 'Scanning folder…';
  await nextPaint();

  const { listen } = window.__TAURI__.event;
  const unlisten = await listen('bulk-import-progress', (event) => {
    const { current, total, title } = event.payload;
    progressEl.textContent = `Importing ${current} of ${total}: ${title}`;
  });

  try {
    const summary = await invoke('import_folder', { rootPath });
    unlisten();
    setCompilingOverlay(false);
    progressEl.hidden = true;
    await loadLibrary();
    const parts = [];
    if (summary.imported.length) parts.push(`${summary.imported.length} imported`);
    if (summary.skipped.length) parts.push(`${summary.skipped.length} already in library`);
    if (summary.failed.length) parts.push(`${summary.failed.length} failed`);
    const msg = parts.length ? parts.join(', ') + '.' : 'No EPUB files found in that folder.';
    await showConfirm(msg, { title: 'Import folder', confirmLabel: 'OK', alertOnly: true });
  } catch (err) {
    unlisten();
    setCompilingOverlay(false);
    progressEl.hidden = true;
    errorEl.textContent = String(err);
    errorEl.hidden = false;
  }
}

document.getElementById('importFolderBtn').addEventListener('click', async () => {
  const folder = await dialogApi.open({ directory: true });
  if (!folder) return;
  await importFolder(folder);
});

/* ================= Library ================= */

let libraryBooks = [];

function formatLibraryDate(epochSecs) {
  if (!epochSecs) return '';
  return new Date(epochSecs * 1000).toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' });
}

function renderLibrary(filterText) {
  const grid = document.getElementById('libraryGrid');
  const emptyState = document.getElementById('libraryEmptyState');
  const search = document.getElementById('librarySearch');

  if (libraryBooks.length === 0) {
    search.hidden = true;
    grid.hidden = true;
    emptyState.hidden = false;
    document.getElementById('libraryEmptyGif').hidden = false;
    emptyState.querySelector('p').textContent =
      "Open a .wld or .epub file to start reading — it'll show up here next time you launch.";
    return;
  }

  search.hidden = false;
  const q = (filterText || '').toLowerCase();
  const filtered = q
    ? libraryBooks.filter(
        (b) => b.title.toLowerCase().includes(q) || (b.author || '').toLowerCase().includes(q),
      )
    : libraryBooks;

  if (filtered.length === 0) {
    grid.hidden = true;
    emptyState.hidden = false;
    document.getElementById('libraryEmptyGif').hidden = true;
    emptyState.querySelector('p').textContent = `No books match "${filterText}".`;
    return;
  }

  emptyState.hidden = true;
  grid.hidden = false;
  grid.innerHTML = '';

  for (const book of filtered) {
    const li = document.createElement('li');
    li.className = book.available ? 'library-card' : 'library-card library-card-missing';

    const openBtn = document.createElement('button');
    openBtn.className = 'library-card-open';
    openBtn.disabled = !book.available;

    const cover = document.createElement('span');
    const showPlaceholder = () => {
      cover.className = 'library-cover library-cover-placeholder';
      cover.textContent = (book.title || '?').trim().charAt(0).toUpperCase();
    };
    if (book.available) {
      cover.className = 'library-cover';
      // Loaded as a plain background image request rather than embedded in
      // list_library's response — decoding happens off the JS main thread,
      // and a probe Image() lets us fall back to the placeholder glyph if
      // the book has no cover (weland-cover:// 404s) without blocking on it.
      const coverUrl = `weland-cover://cover/${encodeURIComponent(book.path)}`;
      const probe = new Image();
      probe.onload = () => {
        cover.style.backgroundImage = `url("${coverUrl}")`;
      };
      probe.onerror = showPlaceholder;
      probe.src = coverUrl;
    } else {
      showPlaceholder();
    }

    const title = document.createElement('span');
    title.className = 'library-title';
    title.textContent = book.title || 'Untitled';

    const author = document.createElement('span');
    author.className = 'library-author';
    author.textContent = book.author || '';

    const meta = document.createElement('span');
    meta.className = 'library-meta';
    meta.textContent = book.available ? `Opened ${formatLibraryDate(book.last_opened_at)}` : 'File not found';

    openBtn.append(cover, title, author, meta);
    openBtn.addEventListener('click', () => openBookAtPath(book.path));

    const removeBtn = document.createElement('button');
    removeBtn.type = 'button';
    removeBtn.className = 'library-card-remove';
    removeBtn.title = 'Remove from library';
    removeBtn.setAttribute('aria-label', 'Remove from library');
    removeBtn.textContent = '×';
    removeBtn.addEventListener('click', async (e) => {
      e.stopPropagation();
      const ok = await showConfirm(`Remove "${book.title}" from your library? The file itself won't be touched.`, {
        title: 'Remove from library',
        confirmLabel: 'Remove',
      });
      if (!ok) return;
      try {
        await invoke('remove_from_library', { path: book.path });
        libraryBooks = libraryBooks.filter((b) => b.path !== book.path);
        renderLibrary(search.value.trim());
      } catch (err) {
        console.error('Failed to remove from library', err);
      }
    });

    const exportBtn = document.createElement('button');
    exportBtn.type = 'button';
    exportBtn.className = 'library-card-export';
    exportBtn.title = 'Export';
    exportBtn.setAttribute('aria-label', 'Export this book');
    exportBtn.textContent = '⇩';
    exportBtn.disabled = !book.available;
    exportBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      exportBook(book.path, book.title);
    });

    li.append(openBtn, exportBtn, removeBtn);
    grid.appendChild(li);
  }
}

async function loadLibrary() {
  try {
    libraryBooks = await invoke('list_library');
  } catch (err) {
    console.error('Failed to load library', err);
    libraryBooks = [];
  }
  renderLibrary(document.getElementById('librarySearch').value.trim());
}

// Debounced: renderLibrary rebuilds the whole grid (every card, every cover
// image) from scratch, so re-running it on every keystroke while typing
// fast is what made search feel like it was momentarily locking up.
let librarySearchTimeout = null;
document.getElementById('librarySearch').addEventListener('input', (e) => {
  clearTimeout(librarySearchTimeout);
  const value = e.target.value.trim();
  librarySearchTimeout = setTimeout(() => renderLibrary(value), 120);
});

document.getElementById('libraryBtn').addEventListener('click', () => {
  clearTimeout(positionSaveTimeout);
  saveReadingPosition();
  document.getElementById('appFrame').hidden = true;
  document.getElementById('emptyState').hidden = false;

  // Opening this book already bumped its last_opened_at server-side. If it
  // was already in our cached library list, just reflect that locally and
  // re-render instead of paying for a full list_library round-trip (which
  // re-fetches every book's cover, not just this one) to move one card to
  // the top. A book that isn't in the cached list yet (freshly imported or
  // opened straight from a file picker, bypassing the library grid) still
  // needs a real refetch to pick up its metadata and cover for the first time.
  const path = currentBook && currentBook.path;
  const existing = path && libraryBooks.find((b) => b.path === path);
  if (existing) {
    existing.last_opened_at = Math.floor(Date.now() / 1000);
    libraryBooks.sort((a, b) => b.last_opened_at - a.last_opened_at);
    renderLibrary(document.getElementById('librarySearch').value.trim());
  } else {
    loadLibrary();
  }
});

// Session-only: resets to expanded on next launch, nothing persisted.
document.getElementById('tocToggleBtn').addEventListener('click', () => {
  const body = document.getElementById('appBody');
  const collapsed = body.getAttribute('data-toc') === 'collapsed';
  body.setAttribute('data-toc', collapsed ? 'expanded' : 'collapsed');
});

loadLibrary();

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

// Finds the first case-insensitive occurrence of `term` in `content`, in the
// same Unicode-codepoint offset space as span/annotation offsets.
function findCodepointRange(content, term) {
  const chars = Array.from(content || '');
  const termChars = Array.from(term || '');
  if (termChars.length === 0) return null;
  const lower = chars.map((c) => c.toLowerCase());
  const termLower = termChars.map((c) => c.toLowerCase());
  outer: for (let i = 0; i <= lower.length - termLower.length; i++) {
    for (let j = 0; j < termLower.length; j++) {
      if (lower[i + j] !== termLower[j]) continue outer;
    }
    return { start: i, end: i + termLower.length };
  }
  return null;
}

// FTS5 snippet() wraps each matched term in «» — pull those back out so we
// can locate the same text in the node's actual content.
function extractMatchTerms(snippet) {
  const terms = [];
  const re = /«([^»]+)»/g;
  let m;
  while ((m = re.exec(snippet || ''))) terms.push(m[1]);
  return terms;
}

let flashTimeout = null;

function flashSearchMatch(nodeId, terms) {
  const node = nodeDataById.get(nodeId);
  if (!node) return;
  let range = null;
  for (const term of terms) {
    range = findCodepointRange(node.content, term);
    if (range) break;
  }
  if (!range) return;

  clearTimeout(flashTimeout);
  rerenderNode(nodeId, [{ start: range.start, end: range.end, annotation_type: 'search_flash', id: 'search-flash' }]);
  flashTimeout = setTimeout(() => rerenderNode(nodeId), 2200);
}

function renderSearchResults(query, hits) {
  document.getElementById('annotationsPanel').hidden = true;
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
      flashSearchMatch(hit.node_id, extractMatchTerms(hit.snippet));
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

/* ================= Annotations panel ================= */

function annotationSnippet(ann) {
  if (ann.comment) return ann.comment;
  if (ann.selected_text) return `"${ann.selected_text}"`;
  return '(recorded note)';
}

function allAnnotationsInOrder() {
  if (!currentBook) return [];
  const out = [];
  for (const node of currentBook.nodes) {
    const anns = annotationsByNode.get(node.id) || [];
    for (const ann of [...anns].sort((a, b) => a.start_offset - b.start_offset)) {
      out.push(ann);
    }
  }
  return out;
}

function updateAnnotationsCount(anns) {
  const badge = document.getElementById('annotationsCount');
  badge.textContent = String(anns.length);
  badge.hidden = anns.length === 0;
}

function renderAnnotationsPanel() {
  const anns = allAnnotationsInOrder();
  updateAnnotationsCount(anns);

  const panel = document.getElementById('annotationsPanel');
  const list = document.getElementById('annotationsList');
  document.getElementById('annotationsPanelTitle').textContent =
    `${anns.length} annotation${anns.length === 1 ? '' : 's'}`;
  list.innerHTML = '';

  if (anns.length === 0) {
    const li = document.createElement('li');
    li.className = 'ann-empty';
    li.textContent = 'No annotations yet — select some text to add one.';
    list.appendChild(li);
    return;
  }

  for (const ann of anns) {
    const li = document.createElement('li');
    const btn = document.createElement('button');
    const date = (ann.created_at || '').slice(0, 10);
    btn.innerHTML = `<span class="sr-type">${escapeHtml(KIND_LABELS[ann.annotation_type] || 'Annotation')}</span>` +
      `<span class="ann-snippet">${escapeHtml(annotationSnippet(ann))}</span>` +
      `<span class="ann-meta">${escapeHtml(ann.author_name)}${date ? ` · ${escapeHtml(date)}` : ''}</span>`;
    btn.addEventListener('click', () => {
      jumpToNode(ann.node_id);
      panel.hidden = true;
    });
    li.appendChild(btn);
    list.appendChild(li);
  }
}

// Refreshes the titlebar count and (if the panel is currently open) its
// contents. Called from rerenderNode(), which every annotation create /
// edit / delete path already funnels through.
function refreshAnnotationsUi() {
  updateAnnotationsCount(allAnnotationsInOrder());
  if (!document.getElementById('annotationsPanel').hidden) renderAnnotationsPanel();
}

document.getElementById('annotationsBtn').addEventListener('click', () => {
  const panel = document.getElementById('annotationsPanel');
  if (panel.hidden) {
    document.getElementById('searchResults').hidden = true;
    renderAnnotationsPanel();
    panel.hidden = false;
  } else {
    panel.hidden = true;
  }
});

document.getElementById('closeAnnotationsPanel').addEventListener('click', () => {
  document.getElementById('annotationsPanel').hidden = true;
});

/* ================= Day / night reading mode ================= */

document.getElementById('modeToggle').addEventListener('click', () => {
  const frame = document.getElementById('appFrame');
  const dark = frame.getAttribute('data-mode') === 'dark';
  frame.setAttribute('data-mode', dark ? 'light' : 'dark');
});

/* ================= Reading settings (font, size, line spacing) ================= */

const READING_FONTS = [
  { id: 'literata', label: 'Literata', stack: "'Literata', 'Iowan Old Style', 'Palatino Linotype', Georgia, serif" },
  { id: 'lora', label: 'Lora', stack: "'Lora', 'Palatino Linotype', Georgia, serif" },
  { id: 'crimson-pro', label: 'Crimson Pro', stack: "'Crimson Pro', 'Palatino Linotype', Georgia, serif" },
  { id: 'spectral', label: 'Spectral', stack: "'Spectral', 'Palatino Linotype', Georgia, serif" },
  { id: 'im-fell-english', label: 'IM Fell English', stack: "'IM Fell English', 'Palatino Linotype', Georgia, serif" },
  { id: 'unifraktur-maguntia', label: 'Unifraktur', stack: "'UnifrakturMaguntia', 'Palatino Linotype', Georgia, serif" },
  { id: 'source-sans-3', label: 'Source Sans 3', stack: "'Source Sans 3', 'Seravek', 'Segoe UI', sans-serif" },
  { id: 'libre-franklin', label: 'Libre Franklin', stack: "'Libre Franklin', 'Seravek', 'Segoe UI', sans-serif" },
];
const READING_SIZE_MIN = 14;
const READING_SIZE_MAX = 24;
const READING_LEADING_MIN = 1.3;
const READING_LEADING_MAX = 2.2;
const READING_VERSE_SPACING_MIN = 0.5;
const READING_VERSE_SPACING_MAX = 6;

let readingSettings = { font: 'literata', size_px: 17, leading: 1.75, verse_spacing: 2 };

function fontStackFor(fontId) {
  return (READING_FONTS.find((f) => f.id === fontId) || READING_FONTS[0]).stack;
}

function applyReadingSettings() {
  const pane = document.getElementById('readingPane');
  pane.style.setProperty('--read-font', fontStackFor(readingSettings.font));
  pane.style.setProperty('--read-size', `${readingSettings.size_px}px`);
  pane.style.setProperty('--read-leading', String(readingSettings.leading));
  pane.style.setProperty('--verse-stanza-gap', `${readingSettings.verse_spacing}rem`);

  document.getElementById('tsSizeValue').textContent = `${readingSettings.size_px}px`;
  document.getElementById('tsLeadingValue').textContent = readingSettings.leading.toFixed(2);
  document.getElementById('tsVerseSpacingValue').textContent = `${readingSettings.verse_spacing.toFixed(2)}rem`;
  document.querySelectorAll('.ts-font-option').forEach((btn) => {
    btn.classList.toggle('active', btn.dataset.font === readingSettings.font);
  });
}

async function saveReadingSettings() {
  try {
    await invoke('set_reading_settings', {
      font: readingSettings.font,
      sizePx: readingSettings.size_px,
      leading: readingSettings.leading,
      verseSpacing: readingSettings.verse_spacing,
    });
  } catch (err) {
    console.error('Failed to save reading settings', err);
  }
}

const fontOptionsEl = document.getElementById('tsFontOptions');
for (const font of READING_FONTS) {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'ts-font-option';
  btn.dataset.font = font.id;
  btn.style.fontFamily = font.stack;
  btn.textContent = font.label;
  btn.addEventListener('click', () => {
    readingSettings.font = font.id;
    applyReadingSettings();
    saveReadingSettings();
  });
  fontOptionsEl.appendChild(btn);
}

document.getElementById('tsSizeDown').addEventListener('click', () => {
  readingSettings.size_px = Math.max(READING_SIZE_MIN, readingSettings.size_px - 1);
  applyReadingSettings();
  saveReadingSettings();
});
document.getElementById('tsSizeUp').addEventListener('click', () => {
  readingSettings.size_px = Math.min(READING_SIZE_MAX, readingSettings.size_px + 1);
  applyReadingSettings();
  saveReadingSettings();
});
document.getElementById('tsLeadingDown').addEventListener('click', () => {
  readingSettings.leading = Math.max(READING_LEADING_MIN, Math.round((readingSettings.leading - 0.05) * 100) / 100);
  applyReadingSettings();
  saveReadingSettings();
});
document.getElementById('tsLeadingUp').addEventListener('click', () => {
  readingSettings.leading = Math.min(READING_LEADING_MAX, Math.round((readingSettings.leading + 0.05) * 100) / 100);
  applyReadingSettings();
  saveReadingSettings();
});
document.getElementById('tsVerseSpacingDown').addEventListener('click', () => {
  readingSettings.verse_spacing = Math.max(
    READING_VERSE_SPACING_MIN,
    Math.round((readingSettings.verse_spacing - 0.25) * 100) / 100,
  );
  applyReadingSettings();
  saveReadingSettings();
});
document.getElementById('tsVerseSpacingUp').addEventListener('click', () => {
  readingSettings.verse_spacing = Math.min(
    READING_VERSE_SPACING_MAX,
    Math.round((readingSettings.verse_spacing + 0.25) * 100) / 100,
  );
  applyReadingSettings();
  saveReadingSettings();
});

document.getElementById('textSettingsBtn').addEventListener('click', (e) => {
  e.stopPropagation();
  const panel = document.getElementById('textSettingsPanel');
  if (!panel.hidden) {
    panel.hidden = true;
    return;
  }
  const btn = document.getElementById('textSettingsBtn');
  const rect = btn.getBoundingClientRect();
  panel.style.top = `${rect.bottom + 8}px`;
  panel.style.right = `${window.innerWidth - rect.right}px`;
  panel.hidden = false;
});

document.addEventListener('click', (e) => {
  const panel = document.getElementById('textSettingsPanel');
  if (panel.hidden) return;
  if (panel.contains(e.target) || e.target.id === 'textSettingsBtn') return;
  panel.hidden = true;
});

(async function initReadingSettings() {
  try {
    readingSettings = await invoke('get_reading_settings');
  } catch (err) {
    console.error('Failed to load reading settings', err);
  }
  applyReadingSettings();
})();

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

/* ================= Dictionary lookup ================= */

// Double-clicking a word already produces a native single-word selection,
// which the mouseup handler above also sees and reacts to (showing the
// highlight/note/record toolbar) — dblclick fires right after, so just
// override that here rather than teaching mouseup to tell the two apart.
document.getElementById('readingPane').addEventListener('dblclick', async (e) => {
  const sel = window.getSelection();
  if (!sel || sel.isCollapsed || sel.rangeCount === 0) return;
  const word = sel.toString().trim().replace(/^[^\p{L}\p{N}]+|[^\p{L}\p{N}]+$/gu, '');
  if (!word) return;
  hideSelectionToolbar();
  showDictionaryPopover(word, e.clientX, e.clientY);
});

async function showDictionaryPopover(word, x, y) {
  const popover = document.getElementById('dictionaryPopover');
  document.getElementById('dictWord').textContent = word;
  const bodyEl = document.getElementById('dictBody');
  bodyEl.textContent = 'Looking up…';

  popover.style.left = `${Math.max(8, Math.min(x, window.innerWidth - 308))}px`;
  popover.style.top = `${Math.max(8, Math.min(y + 12, window.innerHeight - 200))}px`;
  popover.hidden = false;

  let entries;
  try {
    entries = await invoke('lookup_word', { word });
  } catch (err) {
    console.error('Dictionary lookup failed', err);
    bodyEl.textContent = 'Lookup failed.';
    return;
  }

  renderDictionaryResult(word, entries, 'local');
}

// Shared renderer for both the offline GCIDE result and the online fallback
// result, so the two look the same once something's found.
function renderDictionaryResult(word, entries, source) {
  const bodyEl = document.getElementById('dictBody');
  bodyEl.innerHTML = '';

  if (!entries.length) {
    const msg = document.createElement('p');
    msg.className = 'dict-empty';
    msg.textContent = source === 'online'
      ? `No online definition found for "${word}" either.`
      : `No definition found for "${word}".`;
    bodyEl.appendChild(msg);

    // Never fetch automatically — this app is otherwise fully offline, so
    // reaching out to a third-party API is something the reader opts into
    // per lookup, not a silent fallback.
    if (source !== 'online') {
      const onlineBtn = document.createElement('button');
      onlineBtn.type = 'button';
      onlineBtn.className = 'dict-online-btn';
      onlineBtn.textContent = 'Look up online…';
      onlineBtn.addEventListener('click', () => lookupWordOnline(word));
      bodyEl.appendChild(onlineBtn);
    }
    return;
  }

  for (const entry of entries) {
    const div = document.createElement('div');
    div.className = 'dict-entry';
    div.textContent = entry.definition;
    bodyEl.appendChild(div);
  }

  const attribution = document.createElement('div');
  attribution.className = 'dict-attribution';
  attribution.textContent = source === 'online'
    ? 'Source: Wiktionary, via dictionaryapi.dev'
    : 'Source: GCIDE (GNU Collaborative International Dictionary of English)';
  bodyEl.appendChild(attribution);
}

async function lookupWordOnline(word) {
  const bodyEl = document.getElementById('dictBody');
  bodyEl.innerHTML = '';
  const loading = document.createElement('p');
  loading.className = 'dict-empty';
  loading.textContent = 'Looking up online…';
  bodyEl.appendChild(loading);

  let entries = [];
  try {
    // Done in Rust (reqwest), not the webview's own fetch() — that turned
    // out to be unreliable across repeated calls in webkit2gtk.
    entries = await invoke('lookup_word_online', { word });
  } catch (err) {
    console.error('Online dictionary lookup failed', err);
    bodyEl.innerHTML = '';
    const msg = document.createElement('p');
    msg.className = 'dict-empty';
    msg.textContent = 'Online lookup failed — check your connection.';
    bodyEl.appendChild(msg);
    return;
  }

  renderDictionaryResult(word, entries, 'online');
}

document.getElementById('dictClose').addEventListener('click', () => {
  document.getElementById('dictionaryPopover').hidden = true;
});

// A click on the "Look up online" button replaces the popover's own body
// content (bodyEl.innerHTML = '') while this same click is still bubbling —
// which detaches e.target before it reaches document, so the outside-click
// handler below would see `popover.contains(e.target)` as false and close
// the popover instantly, right as the fetch was starting. Stop it from ever
// bubbling that far, regardless of what a click handler inside the popover
// mutates.
document.getElementById('dictionaryPopover').addEventListener('click', (e) => {
  e.stopPropagation();
});

document.addEventListener('click', (e) => {
  const popover = document.getElementById('dictionaryPopover');
  if (popover.hidden || popover.contains(e.target)) return;
  popover.hidden = true;
});

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
  document.getElementById('annotationsPanel').hidden = true;
  document.getElementById('textSettingsPanel').hidden = true;
  document.getElementById('dictionaryPopover').hidden = true;
});

/* ================= Keyboard navigation ================= */

function isTypingInField() {
  const el = document.activeElement;
  if (!el) return false;
  return el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.isContentEditable;
}

function clamp(value, min, max) {
  return Math.min(max, Math.max(min, value));
}

// A held arrow key fires OS keydown-repeat faster than any short eased
// animation can finish, so each restart gets cut off mid-ramp-up and net
// movement crawls. Once a key is confirmed held (e.repeat), switch to real
// constant-velocity scrolling driven by our own rAF loop instead of trying
// to animate a series of discrete steps.
const HELD_SCROLL_SPEED = 900; // px/sec
let heldScrollDirection = 0;
let heldScrollRafId = null;
let heldScrollLastTime = 0;

function heldScrollStep(now) {
  if (heldScrollDirection === 0) {
    heldScrollRafId = null;
    return;
  }
  const pane = document.getElementById('readingPane');
  const dt = (now - heldScrollLastTime) / 1000;
  heldScrollLastTime = now;
  const maxScroll = pane.scrollHeight - pane.clientHeight;
  pane.scrollTop = clamp(pane.scrollTop + heldScrollDirection * HELD_SCROLL_SPEED * dt, 0, maxScroll);
  heldScrollRafId = requestAnimationFrame(heldScrollStep);
}

function startHeldScroll(direction) {
  scrollAnimId++; // cancel any in-flight eased jump so it can't fight this
  if (heldScrollDirection === direction) return;
  heldScrollDirection = direction;
  if (heldScrollRafId == null) {
    heldScrollLastTime = performance.now();
    heldScrollRafId = requestAnimationFrame(heldScrollStep);
  }
}

function stopHeldScroll(direction) {
  if (heldScrollDirection === direction) heldScrollDirection = 0;
}

document.addEventListener('keyup', (e) => {
  if (e.key === 'ArrowDown') stopHeldScroll(1);
  else if (e.key === 'ArrowUp') stopHeldScroll(-1);
});

// Safety net: if focus leaves the window mid-hold (e.g. alt-tab), no keyup
// ever fires and the scroll would otherwise run forever.
window.addEventListener('blur', () => {
  heldScrollDirection = 0;
});

document.addEventListener('keydown', (e) => {
  // Cmd/Ctrl+F focuses search regardless of what's currently focused.
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'f') {
    if (document.getElementById('appFrame').hidden) return;
    e.preventDefault();
    const input = document.getElementById('searchInput');
    input.focus();
    input.select();
    return;
  }

  if (isTypingInField()) return;
  if (document.getElementById('appFrame').hidden) return;

  const pane = document.getElementById('readingPane');
  const maxScroll = pane.scrollHeight - pane.clientHeight;
  const pageStep = pane.clientHeight * 0.9;
  const lineStep = 110;

  switch (e.key) {
    case ' ':
      e.preventDefault();
      smoothScrollTo(pane, clamp(pane.scrollTop + (e.shiftKey ? -pageStep : pageStep), 0, maxScroll), 320);
      break;
    case 'PageDown':
      e.preventDefault();
      smoothScrollTo(pane, clamp(pane.scrollTop + pageStep, 0, maxScroll), 320);
      break;
    case 'PageUp':
      e.preventDefault();
      smoothScrollTo(pane, clamp(pane.scrollTop - pageStep, 0, maxScroll), 320);
      break;
    case 'ArrowDown':
      e.preventDefault();
      if (e.repeat) startHeldScroll(1);
      else smoothScrollTo(pane, clamp(pane.scrollTop + lineStep, 0, maxScroll), 160);
      break;
    case 'ArrowUp':
      e.preventDefault();
      if (e.repeat) startHeldScroll(-1);
      else smoothScrollTo(pane, clamp(pane.scrollTop - lineStep, 0, maxScroll), 160);
      break;
    case 'Home':
      e.preventDefault();
      smoothScrollTo(pane, 0, 400);
      break;
    case 'End':
      e.preventDefault();
      smoothScrollTo(pane, maxScroll, 400);
      break;
  }
});
