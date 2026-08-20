//! Selection → popover → highlight/note UI, layered on top of Phase 6's
//! headless `AnnotationIndex` and the Phase 2 node index. Kept in its own
//! module, separate from `annotations.rs`'s data layer, so a bug is always
//! obviously either "wrong SQL/data" or "wrong GTK wiring" — never both at
//! once (see the rewrite plan's phase notes).
//!
//! Only `heading`/`paragraph`/`blockquote`/`verse_line` nodes are
//! annotatable for now — their `content` is one flat string that lines up
//! with what's actually in the buffer (bar the one-character adjustment for
//! a verse stanza's leading blank line, see `content_start_offset`).
//! `list`/`table` nodes build their buffer text out of multiple pieces
//! (bullets, cell separators) with no single `content` string to anchor
//! offsets into, and `image`/`thematic_break` have no text at all — so
//! selections touching them are just declined, same as a selection that
//! crosses a node boundary.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::{
    self as gtk, gdk, glib, pango, prelude::*, Align, Box as GtkBox, Button, Entry, Label, MediaFile, Orientation,
    PolicyType, Popover, ScrolledWindow, TextBuffer, TextIter, TextTag, TextView, TextWindowType,
};
use rusqlite::Connection;
use weland::db;
use weland::schema::{AstNode, UserAnnotation};

use crate::annotations::AnnotationIndex;
use crate::dictionary_ui;
use crate::node_index::NodeIndex;
use crate::recording;

const ANNOTATABLE_TYPES: &[&str] = &["heading", "paragraph", "blockquote", "verse_line"];
// TODO(Phase 9): replace with the reader's own display name once settings UI exists.
const AUTHOR_NAME: &str = "Reader";

pub struct AnnotationTags {
    pub highlight: TextTag,
    pub text_note: TextTag,
    pub voice_note: TextTag,
}

pub fn build_annotation_tags(buffer: &TextBuffer) -> AnnotationTags {
    let highlight = buffer
        .create_tag(Some("ann_highlight"), &[("background-rgba", &gdk::RGBA::new(0.878, 0.686, 0.407, 0.35))])
        .expect("create highlight tag");
    let text_note = buffer
        .create_tag(
            Some("ann_text_note"),
            &[("underline", &pango::Underline::Single), ("underline-rgba", &gdk::RGBA::new(0.49, 0.647, 0.808, 1.0))],
        )
        .expect("create text_note tag");
    let voice_note = buffer
        .create_tag(
            Some("ann_voice_note"),
            &[("underline", &pango::Underline::Single), ("underline-rgba", &gdk::RGBA::new(0.635, 0.412, 0.847, 1.0))],
        )
        .expect("create voice_note tag");
    AnnotationTags { highlight, text_note, voice_note }
}

fn tag_for<'a>(tags: &'a AnnotationTags, annotation_type: &str) -> Option<&'a TextTag> {
    match annotation_type {
        "highlight" => Some(&tags.highlight),
        "text_note" => Some(&tags.text_note),
        "voice_note" => Some(&tags.voice_note),
        _ => None,
    }
}

/// Bundles everything the click/selection handlers (and the annotations
/// list panel) need, behind one `Rc` so wiring a signal only means cloning
/// one handle instead of six.
pub struct AnnotationState {
    conn: Connection,
    tags: AnnotationTags,
    nodes: Vec<AstNode>,
    index: Rc<NodeIndex>,
    text_view: TextView,
    annotations: RefCell<AnnotationIndex>,
    // The list panel's own contents box, filled in once `build_annotation_list_panel`
    // runs — `None` until then, since a reader window with no annotation
    // list built (there isn't one yet) still needs a working `AnnotationState`.
    list_container: RefCell<Option<GtkBox>>,
    // A `gtk::MediaFile` stops playback the instant it's dropped, so the one
    // currently playing (if any) needs somewhere to live past the click
    // handler that started it. Starting a new playback just drops/replaces
    // whatever was here, which also doubles as "only one voice note plays at
    // a time."
    now_playing: RefCell<Option<gtk::MediaFile>>,
    // The one annotation-related popover currently open (create/view/note-
    // composer/recording — whichever), if any. A click landing on the text
    // view while one of these is open should just dismiss it, not *also* be
    // evaluated as a fresh annotation click — otherwise a click meant to
    // close the current popover routinely lands on/near a different
    // annotation (often the one on the line above, since popovers commonly
    // render above their anchor) and pops that one open instead.
    current_popover: RefCell<Option<Popover>>,
}

impl AnnotationState {
    pub fn new(
        conn: Connection,
        tags: AnnotationTags,
        nodes: Vec<AstNode>,
        index: Rc<NodeIndex>,
        text_view: TextView,
        annotations: AnnotationIndex,
    ) -> Rc<Self> {
        Rc::new(Self {
            conn,
            tags,
            nodes,
            index,
            text_view,
            annotations: RefCell::new(annotations),
            list_container: RefCell::new(None),
            now_playing: RefCell::new(None),
            current_popover: RefCell::new(None),
        })
    }

    // Shared with `search_ui.rs`, which needs the same node list, node
    // index, and text view search already has — reusing this instead of
    // threading a parallel copy through app.rs.
    pub fn nodes(&self) -> &[AstNode] {
        &self.nodes
    }

    pub fn index(&self) -> &Rc<NodeIndex> {
        &self.index
    }

    pub fn text_view(&self) -> &TextView {
        &self.text_view
    }
}

/// Buffer offset a node's content actually starts at. Equal to the node's
/// own recorded mark for everything except a verse line that opens a
/// stanza — `document::build_document` inserts a blank line there *before*
/// the line's own text, but `NodeIndex::record` runs before that insertion,
/// so the mark sits one character earlier than the content itself.
pub(crate) fn content_start_offset(buffer: &TextBuffer, node: &AstNode, index: &NodeIndex) -> Option<i32> {
    let mark = index.mark_for_node(node.id)?;
    let base = buffer.iter_at_mark(mark).offset();
    let stanza_start = node.node_type == "verse_line"
        && node.attributes.as_ref().and_then(|a| a.get("stanza_start")).and_then(|v| v.as_bool()).unwrap_or(false);
    Some(if stanza_start { base + 1 } else { base })
}

/// Node boundaries in buffer-offset order, parallel to `nodes` — node `i`
/// spans `[boundaries[i], boundaries[i+1])` (or to the buffer's end for the
/// last node).
fn node_boundaries(buffer: &TextBuffer, nodes: &[AstNode], index: &NodeIndex) -> Vec<i32> {
    nodes.iter().map(|n| index.mark_for_node(n.id).map(|m| buffer.iter_at_mark(m).offset()).unwrap_or(i32::MAX)).collect()
}

fn node_at_offset<'a>(buffer: &TextBuffer, nodes: &'a [AstNode], boundaries: &[i32], offset: i32) -> Option<(&'a AstNode, i32, i32)> {
    let i = boundaries.partition_point(|&b| b <= offset).checked_sub(1)?;
    let node = &nodes[i];
    let node_end = boundaries.get(i + 1).copied().unwrap_or_else(|| buffer.end_iter().offset());
    Some((node, boundaries[i], node_end))
}

/// Paints every already-loaded annotation's `[start_offset, end_offset)`
/// (node-relative character offsets — see `content_start_offset`) as a tag
/// over the matching buffer range. Call once after `build_document`, so
/// every node's mark already exists.
pub fn render_existing(buffer: &TextBuffer, tags: &AnnotationTags, nodes: &[AstNode], index: &NodeIndex, annotation_index: &AnnotationIndex) {
    for node in nodes {
        let anns = annotation_index.for_node(node.id);
        if anns.is_empty() {
            continue;
        }
        let Some(content_start) = content_start_offset(buffer, node, index) else { continue };
        for ann in anns {
            let Some(tag) = tag_for(tags, &ann.annotation_type) else { continue };
            let start_iter = buffer.iter_at_offset(content_start + ann.start_offset as i32);
            let end_iter = buffer.iter_at_offset(content_start + ann.end_offset as i32);
            buffer.apply_tag(tag, &start_iter, &end_iter);
        }
    }
}

#[derive(Clone)]
struct SelectionAnchor {
    node_id: i64,
    start_offset: i64,
    end_offset: i64,
    selected_text: String,
    buffer_start: i32,
    buffer_end: i32,
}

/// `None` covers both "nothing selected" and the two cases the plan flagged
/// as this phase's real risk: a selection that crosses a node boundary, or
/// that lands on a node type with no flat `content` string to anchor into.
fn resolve_selection_anchor(buffer: &TextBuffer, nodes: &[AstNode], index: &NodeIndex) -> Option<SelectionAnchor> {
    let (start_iter, end_iter) = buffer.selection_bounds()?;
    let sel_start = start_iter.offset();
    let sel_end = end_iter.offset();
    if sel_start >= sel_end {
        return None;
    }

    let boundaries = node_boundaries(buffer, nodes, index);
    let (node, node_start, node_end) = node_at_offset(buffer, nodes, &boundaries, sel_start)?;
    if sel_end > node_end || !ANNOTATABLE_TYPES.contains(&node.node_type.as_str()) {
        return None;
    }
    let _ = node_start;

    let content_start = content_start_offset(buffer, node, index)?;
    let start_offset = (sel_start - content_start).max(0) as i64;
    let end_offset = (sel_end - content_start).max(0) as i64;
    let selected_text = buffer.text(&start_iter, &end_iter, false).to_string();

    Some(SelectionAnchor { node_id: node.id, start_offset, end_offset, selected_text, buffer_start: sel_start, buffer_end: sel_end })
}

/// The existing annotation (if any) covering `offset`, plus its buffer
/// range for tag removal/positioning.
fn find_annotation_at(buffer: &TextBuffer, offset: i32, nodes: &[AstNode], index: &NodeIndex, annotation_index: &AnnotationIndex) -> Option<(UserAnnotation, i32, i32)> {
    let boundaries = node_boundaries(buffer, nodes, index);
    let (node, _, _) = node_at_offset(buffer, nodes, &boundaries, offset)?;
    let content_start = content_start_offset(buffer, node, index)?;
    let rel = offset - content_start;
    if rel < 0 {
        return None;
    }
    for ann in annotation_index.for_node(node.id) {
        if rel >= ann.start_offset as i32 && rel < ann.end_offset as i32 {
            return Some((ann.clone(), content_start + ann.start_offset as i32, content_start + ann.end_offset as i32));
        }
    }
    None
}

/// Positions a popover-anchor rectangle over `[start, end)` in widget
/// (window) coordinates. Only accurate for a single-line range — a
/// selection wrapping across lines just gets the first line's extent, which
/// is an acceptable approximation for where to point a popover.
fn rect_for_range(text_view: &TextView, buffer: &TextBuffer, start: i32, end: i32) -> gdk::Rectangle {
    let start_iter = buffer.iter_at_offset(start);
    let loc = text_view.iter_location(&start_iter);
    let (wx, wy) = text_view.buffer_to_window_coords(TextWindowType::Widget, loc.x(), loc.y());
    let width = if end > start {
        let end_iter = buffer.iter_at_offset(end);
        let end_loc = text_view.iter_location(&end_iter);
        (end_loc.x() - loc.x()).max(4)
    } else {
        4
    };
    gdk::Rectangle::new(wx, wy, width, loc.height().max(4))
}

/// True when `[start, end)` spans exactly one whole word — the signature of
/// a double-click word-selection, not a manual drag (which would need to
/// coincidentally land on exactly these bounds to match). Checking just
/// `starts_word()`/`ends_word()` isn't enough on its own, since a drag
/// spanning several *whole* words (e.g. "The villein") also starts and ends
/// on word boundaries; walking one word forward from `start` and comparing
/// against `end` additionally rules out anything wider than one word.
fn selection_is_single_word(start: &TextIter, end: &TextIter) -> bool {
    if !start.starts_word() || !end.ends_word() {
        return false;
    }
    let mut probe = start.clone();
    probe.forward_word_end();
    probe.offset() == end.offset()
}

fn create_annotation(
    buffer: &TextBuffer,
    state: &Rc<AnnotationState>,
    anchor: &SelectionAnchor,
    annotation_type: &str,
    comment: Option<String>,
    asset_id: Option<i64>,
) {
    let new = db::NewAnnotation {
        node_id: anchor.node_id,
        start_offset: anchor.start_offset,
        end_offset: anchor.end_offset,
        selected_text: Some(anchor.selected_text.clone()),
        annotation_type: annotation_type.to_string(),
        comment,
        asset_id,
        author_name: AUTHOR_NAME.to_string(),
    };
    if db::insert_annotation(&state.conn, new).is_err() {
        return;
    }
    if let Some(tag) = tag_for(&state.tags, annotation_type) {
        let start_iter = buffer.iter_at_offset(anchor.buffer_start);
        let end_iter = buffer.iter_at_offset(anchor.buffer_end);
        buffer.apply_tag(tag, &start_iter, &end_iter);
        buffer.place_cursor(&end_iter);
    }
    if let Ok(fresh) = AnnotationIndex::load(&state.conn) {
        *state.annotations.borrow_mut() = fresh;
    }
    refresh_annotation_list(state);
}

/// Remembers `popover` as the one currently open, and forgets it again the
/// moment GTK reports it closed (whether from an explicit `popdown()` or its
/// own autohide-on-outside-click) — `handle_release` consults this to make
/// a click's *first* job "dismiss whatever's open," never "also maybe open
/// a different one."
fn track_popover(state: &Rc<AnnotationState>, popover: &Popover) {
    *state.current_popover.borrow_mut() = Some(popover.clone());
    let state = state.clone();
    popover.connect_closed(move |_| {
        *state.current_popover.borrow_mut() = None;
    });
}

/// Swaps `popover`'s child for a text entry + Save button; `on_save` fires
/// with the trimmed, non-empty comment text. Shared between "new note" and
/// "edit existing note", which differ only in `initial_text` and what
/// `on_save` does with the result.
/// Closes `old_popover` and opens a brand new one holding a text entry +
/// Save button at the same anchor rect. Mutating an already-popped-up
/// popover's child in place (`set_child` on a live `Popover`) turned out to
/// leave it reporting `visible() == true` while painting nothing at all —
/// a screenshot taken mid-bug showed no popover content whatsoever, even
/// though every widget was constructed and parented correctly. Never
/// reusing a shown popover's surface sidesteps that entirely, and matches
/// how `show_create_popover`/`show_view_popover` already build their full
/// content before the one and only `popup()` call.
fn show_comment_composer(
    text_view: &TextView,
    old_popover: &Popover,
    rect: gdk::Rectangle,
    initial_text: &str,
    state: &Rc<AnnotationState>,
    on_save: impl Fn(String) + 'static,
) {
    old_popover.popdown();

    let popover = Popover::new();
    popover.set_parent(text_view);
    // GTK's own autohide-on-outside-click reacts to button *press*, a full
    // event cycle before our own dismiss logic (which watches release) ever
    // runs — so with autohide left on, one click could close the popover
    // via GTK's grab *and then* still get evaluated by our release handler
    // as "no popover was open," opening a different one on the very same
    // click. Disabling it makes `handle_release`'s own tracking (see
    // `AnnotationState::current_popover`) the single source of truth for
    // when one of these closes.
    popover.set_autohide(false);
    popover.set_pointing_to(Some(&rect));

    let entry = Entry::builder().placeholder_text("Note\u{2026}").text(initial_text).width_chars(24).build();
    let save_btn = Button::with_label("Save");
    let row = GtkBox::new(Orientation::Horizontal, 6);
    row.append(&entry);
    row.append(&save_btn);
    popover.set_child(Some(&row));

    let popover_c = popover.clone();
    let entry_c = entry.clone();
    save_btn.connect_clicked(move |_| {
        let comment = entry_c.text().trim().to_string();
        if !comment.is_empty() {
            on_save(comment);
        }
        popover_c.popdown();
    });

    popover.popup();
    track_popover(state, &popover);
    entry.grab_focus();
}

fn show_create_popover(text_view: &TextView, buffer: &TextBuffer, anchor: SelectionAnchor, state: Rc<AnnotationState>) {
    let rect = rect_for_range(text_view, buffer, anchor.buffer_start, anchor.buffer_end);
    let popover = Popover::new();
    popover.set_parent(text_view);
    popover.set_autohide(false);
    popover.set_pointing_to(Some(&rect));

    let row = GtkBox::new(Orientation::Horizontal, 6);
    let highlight_btn = Button::with_label("Highlight");
    let note_btn = Button::with_label("Note\u{2026}");
    let record_btn = Button::with_label("Record\u{2026}");
    row.append(&highlight_btn);
    row.append(&note_btn);
    row.append(&record_btn);
    popover.set_child(Some(&row));

    {
        let popover = popover.clone();
        let buffer = buffer.clone();
        let state = state.clone();
        let anchor = anchor.clone();
        highlight_btn.connect_clicked(move |_| {
            create_annotation(&buffer, &state, &anchor, "highlight", None, None);
            popover.popdown();
        });
    }
    {
        let text_view = text_view.clone();
        let popover_outer = popover.clone();
        let rect = rect.clone();
        let buffer = buffer.clone();
        let state = state.clone();
        let anchor = anchor.clone();
        note_btn.connect_clicked(move |_| {
            let buffer = buffer.clone();
            let anchor = anchor.clone();
            let state_for_save = state.clone();
            show_comment_composer(&text_view, &popover_outer, rect.clone(), "", &state, move |comment| {
                create_annotation(&buffer, &state_for_save, &anchor, "text_note", Some(comment), None);
            });
        });
    }
    {
        let text_view = text_view.clone();
        let popover_outer = popover.clone();
        let rect = rect.clone();
        let buffer = buffer.clone();
        let state = state.clone();
        let anchor = anchor.clone();
        record_btn.connect_clicked(move |_| {
            show_recording_popover(&text_view, &popover_outer, rect.clone(), &buffer, &state, &anchor);
        });
    }

    popover.popup();
    track_popover(&state, &popover);
}

/// Closes `old_popover`, starts microphone capture immediately, and opens a
/// fresh popover with a live elapsed-time label and a Stop button — same
/// never-mutate-a-live-popover discipline as `show_comment_composer`, for
/// the same reason (see its doc comment).
fn show_recording_popover(
    text_view: &TextView,
    old_popover: &Popover,
    rect: gdk::Rectangle,
    buffer: &TextBuffer,
    state: &Rc<AnnotationState>,
    anchor: &SelectionAnchor,
) {
    old_popover.popdown();

    let session = match recording::start_voice_recording() {
        Ok(session) => session,
        Err(e) => {
            eprintln!("failed to start recording: {e}");
            return;
        }
    };
    let session = Rc::new(RefCell::new(Some(session)));

    let popover = Popover::new();
    popover.set_parent(text_view);
    popover.set_autohide(false);
    popover.set_pointing_to(Some(&rect));

    let elapsed_label = Label::new(Some("Recording\u{2026} 0:00"));
    let stop_btn = Button::with_label("Stop");
    let row = GtkBox::new(Orientation::Horizontal, 6);
    row.append(&elapsed_label);
    row.append(&stop_btn);
    popover.set_child(Some(&row));

    {
        let session = session.clone();
        let elapsed_label = elapsed_label.clone();
        let seconds = Rc::new(RefCell::new(0u32));
        glib::timeout_add_local(Duration::from_secs(1), move || {
            if session.borrow().is_none() {
                return glib::ControlFlow::Break;
            }
            *seconds.borrow_mut() += 1;
            let s = *seconds.borrow();
            elapsed_label.set_text(&format!("Recording\u{2026} {}:{:02}", s / 60, s % 60));
            glib::ControlFlow::Continue
        });
    }

    let popover_c = popover.clone();
    let buffer_c = buffer.clone();
    let state_c = state.clone();
    let anchor_c = anchor.clone();
    stop_btn.connect_clicked(move |_| {
        let Some(rec_session) = session.borrow_mut().take() else { return };
        match recording::stop_voice_recording(rec_session) {
            Ok(ogg_bytes) => match db::insert_voice_asset(&state_c.conn, "audio/ogg; codecs=opus", &ogg_bytes) {
                Ok(asset_id) => create_annotation(&buffer_c, &state_c, &anchor_c, "voice_note", None, Some(asset_id)),
                Err(e) => eprintln!("failed to save voice note asset: {e}"),
            },
            Err(e) => eprintln!("failed to stop recording: {e}"),
        }
        popover_c.popdown();
    });

    popover.popup();
    track_popover(state, &popover);
}

fn show_view_popover(text_view: &TextView, buffer: &TextBuffer, ann: UserAnnotation, buffer_start: i32, buffer_end: i32, state: Rc<AnnotationState>) {
    let rect = rect_for_range(text_view, buffer, buffer_start, buffer_end);
    let popover = Popover::new();
    popover.set_parent(text_view);
    popover.set_autohide(false);
    popover.set_pointing_to(Some(&rect));

    let container = GtkBox::new(Orientation::Vertical, 6);
    if let Some(comment) = &ann.comment {
        let label = Label::new(Some(comment));
        label.set_wrap(true);
        container.append(&label);
    } else if let Some(text) = &ann.selected_text {
        let label = Label::new(Some(&format!("\u{201c}{text}\u{201d}")));
        label.set_wrap(true);
        label.add_css_class("dim-label");
        container.append(&label);
    }

    let actions = GtkBox::new(Orientation::Horizontal, 6);
    if ann.annotation_type == "text_note" {
        let edit_btn = Button::with_label("Edit");
        let text_view_c = text_view.clone();
        let popover_c = popover.clone();
        let rect_c = rect.clone();
        let state_c = state.clone();
        let ann_id = ann.id;
        let existing_comment = ann.comment.clone().unwrap_or_default();
        edit_btn.connect_clicked(move |_| {
            let state_for_save = state_c.clone();
            show_comment_composer(&text_view_c, &popover_c, rect_c.clone(), &existing_comment, &state_c, move |comment| {
                if db::update_annotation_comment(&state_for_save.conn, ann_id, &comment).is_ok() {
                    if let Ok(fresh) = AnnotationIndex::load(&state_for_save.conn) {
                        *state_for_save.annotations.borrow_mut() = fresh;
                    }
                    refresh_annotation_list(&state_for_save);
                }
            });
        });
        actions.append(&edit_btn);
    }
    if ann.annotation_type == "voice_note" {
        if let Some(asset_id) = ann.asset_id {
            let play_btn = Button::with_label("Play");
            let state_c = state.clone();
            let container_c = container.clone();
            play_btn.connect_clicked(move |_| play_voice_note(&state_c, &container_c, asset_id));
            actions.append(&play_btn);
        }
    }

    let delete_btn = Button::with_label("Delete");
    {
        let popover_c = popover.clone();
        let buffer_c = buffer.clone();
        let state_c = state.clone();
        let ann_id = ann.id;
        let ann_type = ann.annotation_type.clone();
        delete_btn.connect_clicked(move |_| {
            if db::delete_annotation(&state_c.conn, ann_id).is_ok() {
                if let Some(tag) = tag_for(&state_c.tags, &ann_type) {
                    let start_iter = buffer_c.iter_at_offset(buffer_start);
                    let end_iter = buffer_c.iter_at_offset(buffer_end);
                    buffer_c.remove_tag(tag, &start_iter, &end_iter);
                }
                if let Ok(fresh) = AnnotationIndex::load(&state_c.conn) {
                    *state_c.annotations.borrow_mut() = fresh;
                }
                refresh_annotation_list(&state_c);
            }
            popover_c.popdown();
        });
    }
    actions.append(&delete_btn);
    container.append(&actions);
    popover.set_child(Some(&container));

    popover.popup();
    track_popover(&state, &popover);
}

/// Writes the voice note's Ogg Opus bytes out to a scratch temp file and
/// plays it via `GtkMediaFile` — de-risked per the rewrite plan by
/// confirming file-path playback works before ever trying in-memory bytes,
/// which `GtkMediaFile` has no direct API for anyway (it's built around
/// `GFile`/filenames, not byte buffers). The asset is already
/// content-hash-deduped by `db::insert_voice_asset`, so re-playing the same
/// note just overwrites the same temp path each time rather than
/// accumulating files.
///
/// `GtkMediaFile` is GStreamer-backed and built primarily for video-in-a-
/// widget use — calling `play()` on one that's never attached to anything
/// as a `GdkPaintable` builds the pipeline (it logs its usual GstPlay
/// warnings) but never actually reaches running playback. Attaching it to a
/// small `Picture` appended into the popover's own container gives it a
/// real consumer, and ties its lifetime to the popover as a side effect.
fn play_voice_note(state: &Rc<AnnotationState>, container: &GtkBox, asset_id: i64) {
    let Ok((_, data)) = db::load_asset(&state.conn, asset_id) else { return };
    let path = std::env::temp_dir().join(format!("weland-voice-note-{asset_id}.ogg"));
    if std::fs::write(&path, &data).is_err() {
        return;
    }
    let media = MediaFile::for_filename(&path);

    let sink = gtk::Picture::new();
    sink.set_size_request(32, 32);
    sink.set_paintable(Some(&media));
    container.append(&sink);

    media.play();
    // Dropping the previous MediaFile (if any) stops it — replacing this
    // slot doubles as "starting a new playback stops whatever was playing."
    *state.now_playing.borrow_mut() = Some(media);
}

/// Watches for the primary button being released over `text_view`, then
/// shows a create-annotation popover over a valid fresh selection, or a
/// view/edit/delete popover over an existing annotation the click landed
/// on.
///
/// Deliberately *not* a second `GtkGestureClick`: stacking one alongside
/// GtkTextView's own internal click gesture measurably broke native
/// click-drag text selection (confirmed by A/B testing — removing the extra
/// GestureClick entirely restored working drag-select; changing its
/// propagation phase to Capture did not help). A `GtkEventControllerLegacy`
/// only observes the raw event stream and never participates in gesture
/// recognition/arbitration, so it can't interfere. The check itself runs
/// from a deferred idle callback, not inline in the event handler, so it
/// always sees the buffer's state *after* GtkTextView has finished its own
/// handling of that same release.
pub fn wire_annotation_interactions(text_view: &TextView, buffer: &TextBuffer, state: Rc<AnnotationState>) {
    text_view.set_has_tooltip(true);
    {
        let buffer = buffer.clone();
        let state = state.clone();
        text_view.connect_query_tooltip(move |tv, x, y, _keyboard_mode, tooltip| {
            let (bx, by) = tv.window_to_buffer_coords(TextWindowType::Widget, x, y);
            let Some(iter) = tv.iter_at_location(bx, by) else { return false };
            let hit = {
                let anns = state.annotations.borrow();
                find_annotation_at(&buffer, iter.offset(), &state.nodes, &state.index, &anns)
            };
            let Some((ann, _, _)) = hit else { return false };
            let text = ann
                .comment
                .clone()
                .or_else(|| ann.selected_text.clone())
                .unwrap_or_else(|| ann.annotation_type.clone());
            tooltip.set_text(Some(&text));
            true
        });
    }

    let legacy = gtk::EventControllerLegacy::new();
    legacy.set_propagation_phase(gtk::PropagationPhase::Bubble);
    text_view.add_controller(legacy.clone());

    let text_view_for_event = text_view.clone();
    let buffer_for_event = buffer.clone();
    legacy.connect_event(move |_controller, event| {
        if event.event_type() == gdk::EventType::ButtonRelease {
            if let Some(button_event) = event.downcast_ref::<gdk::ButtonEvent>() {
                if button_event.button() == gdk::BUTTON_PRIMARY {
                    // `event.position()` is surface-relative (the whole
                    // top-level window), not relative to `text_view` — using
                    // it directly baked the sidebar's width/toolbar height
                    // into every click as a rightward/downward offset,
                    // making annotations increasingly unclickable the
                    // further right they sat on their line. Translate
                    // through the root widget into text_view's own
                    // coordinate space first.
                    if let Some((x, y)) = event.position() {
                        let text_view = text_view_for_event.clone();
                        let buffer = buffer_for_event.clone();
                        let state = state.clone();
                        glib::idle_add_local_once(move || {
                            let (tx, ty) = text_view
                                .root()
                                .and_then(|root| root.translate_coordinates(&text_view, x, y))
                                .unwrap_or((x, y));
                            handle_release(&text_view, &buffer, tx, ty, &state);
                        });
                    }
                }
            }
        }
        glib::Propagation::Proceed
    });
}

fn handle_release(text_view: &TextView, buffer: &TextBuffer, x: f64, y: f64, state: &Rc<AnnotationState>) {
    // If a popover from a previous click is still open, this click's first
    // job is dismissing it. A genuine fresh selection (drag) still goes on
    // to open its own create-popover below, but a bare click, once it's
    // closed the old popover, is done -- without this, a click meant only
    // to dismiss the current popover routinely got evaluated as a brand new
    // annotation click too, and since popovers commonly render above their
    // anchor, that new click just as routinely landed on whatever
    // annotation was on the line above, popping a different popover open in
    // the old one's place.
    // `.take()` ends the `RefCell` borrow as soon as this statement
    // finishes, *before* `popdown()` runs — `popdown()` synchronously fires
    // `closed`, which re-enters `current_popover.borrow_mut()` via
    // `track_popover`'s handler, so calling it while still inside the
    // `borrow_mut()` chain above (e.g. `.borrow_mut().take().map(|p|
    // p.popdown())`) panics with "already mutably borrowed."
    let previous_popover = state.current_popover.borrow_mut().take();
    let had_open_popover = previous_popover.is_some();
    if let Some(popover) = previous_popover {
        popover.popdown();
    }

    if buffer.has_selection() {
        // A selection spanning exactly one whole word is what GTK's own
        // double-click-to-select-word produces — natively and
        // pixel-accurately, unlike hand-rolling word boundaries from a
        // click position (an earlier version of the dictionary lookup did
        // exactly that on a right-click and was unreliable right at word
        // edges). Route it to the dictionary instead of the annotate
        // popover; anything else (a drag spanning multiple words, or a
        // partial-word drag) still means "annotate."
        if let Some((start, end)) = buffer.selection_bounds() {
            if selection_is_single_word(&start, &end) {
                let word = buffer.text(&start, &end, false).to_string();
                let rect = rect_for_range(text_view, buffer, start.offset(), end.offset());
                dictionary_ui::show_word_lookup(text_view, rect, &word);
                return;
            }
        }
        if let Some(anchor) = resolve_selection_anchor(buffer, &state.nodes, &state.index) {
            show_create_popover(text_view, buffer, anchor, state.clone());
        }
        return;
    }

    if had_open_popover {
        return;
    }

    let (bx, by) = text_view.window_to_buffer_coords(TextWindowType::Widget, x as i32, y as i32);
    let hit = {
        let anns = state.annotations.borrow();
        find_annotation_near(text_view, buffer, bx, by, &state.nodes, &state.index, &anns)
    };
    if let Some((ann, start, end)) = hit {
        show_view_popover(text_view, buffer, ann, start, end, state.clone());
    }
}

/// Finds the annotation nearest a click, tolerating real-world imprecision:
/// tries the exact clicked position first, then — if that misses — any
/// annotation in the same node whose rendered extent touches the click's
/// line, picking the nearest by horizontal distance. A short annotation (a
/// handful of characters, sometimes only part of a word — e.g. a highlight
/// on just "lenteous" out of "plenteous") is an unreasonably narrow pixel
/// target otherwise: confirmed via a live debug session where clicks 250+ px
/// away from an 8-character annotation's actual rendered position were the
/// norm, not the exception, while still landing on the correct line.
fn find_annotation_near(
    text_view: &TextView,
    buffer: &TextBuffer,
    bx: i32,
    by: i32,
    nodes: &[AstNode],
    index: &NodeIndex,
    annotation_index: &AnnotationIndex,
) -> Option<(UserAnnotation, i32, i32)> {
    let iter = text_view.iter_at_location(bx, by)?;
    if let Some(hit) = find_annotation_at(buffer, iter.offset(), nodes, index, annotation_index) {
        return Some(hit);
    }

    let boundaries = node_boundaries(buffer, nodes, index);
    let (node, _, _) = node_at_offset(buffer, nodes, &boundaries, iter.offset())?;
    let content_start = content_start_offset(buffer, node, index)?;
    let click_line = iter.line();

    let mut best: Option<(i32, &UserAnnotation, i32, i32)> = None;
    for ann in annotation_index.for_node(node.id) {
        let start = content_start + ann.start_offset as i32;
        let end = content_start + ann.end_offset as i32;
        let start_iter = buffer.iter_at_offset(start);
        let end_iter = buffer.iter_at_offset(end.max(start + 1));
        if start_iter.line() != click_line && end_iter.line() != click_line {
            continue;
        }
        let start_x = text_view.iter_location(&start_iter).x();
        let end_x = text_view.iter_location(&end_iter).x();
        let dist = if bx < start_x { start_x - bx } else { (bx - end_x).max(0) };
        if best.as_ref().map(|(d, ..)| dist < *d).unwrap_or(true) {
            best = Some((dist, ann, start, end));
        }
    }
    best.map(|(_, ann, start, end)| (ann.clone(), start, end))
}

/// Builds the "Annotations" sidebar panel: every annotation in the book,
/// each entry jumping to its node the same way a TOC entry does. Kept
/// current by `refresh_annotation_list`, called after every create/edit/
/// delete alongside the `AnnotationIndex` reload those already do.
pub fn build_annotation_list_panel(state: &Rc<AnnotationState>) -> ScrolledWindow {
    let list = GtkBox::new(Orientation::Vertical, 4);
    list.set_margin_top(8);
    list.set_margin_bottom(8);
    list.set_margin_start(8);
    list.set_margin_end(8);

    *state.list_container.borrow_mut() = Some(list.clone());
    refresh_annotation_list(state);

    ScrolledWindow::builder().child(&list).hscrollbar_policy(PolicyType::Never).width_request(220).build()
}

fn refresh_annotation_list(state: &Rc<AnnotationState>) {
    let Some(list) = state.list_container.borrow().clone() else { return };
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }

    let guard = state.annotations.borrow();
    let mut rows: Vec<(&AstNode, &UserAnnotation)> = Vec::new();
    for node in &state.nodes {
        for ann in guard.for_node(node.id) {
            rows.push((node, ann));
        }
    }

    if rows.is_empty() {
        let empty = Label::new(Some("No annotations yet \u{2014} select text to highlight or add a note."));
        empty.set_wrap(true);
        empty.set_halign(Align::Start);
        empty.add_css_class("dim-label");
        list.append(&empty);
        return;
    }

    for (node, ann) in rows {
        let kind = Label::new(Some(match ann.annotation_type.as_str() {
            "highlight" => "Highlight",
            "text_note" => "Note",
            "voice_note" => "Voice Note",
            other => other,
        }));
        kind.set_halign(Align::Start);
        kind.add_css_class("heading");

        let snippet_text = ann.comment.clone().or_else(|| ann.selected_text.clone()).unwrap_or_default();
        let snippet = Label::new(Some(&snippet_text));
        snippet.set_halign(Align::Start);
        snippet.set_wrap(true);
        snippet.set_lines(2);
        snippet.set_ellipsize(pango::EllipsizeMode::End);

        let entry_box = GtkBox::new(Orientation::Vertical, 2);
        entry_box.append(&kind);
        entry_box.append(&snippet);

        let button = Button::builder().child(&entry_box).has_frame(false).build();
        let node_id = node.id;
        let text_view = state.text_view.clone();
        let index = state.index.clone();
        button.connect_clicked(move |_| {
            if let Some(mark) = index.mark_for_node(node_id) {
                text_view.scroll_to_mark(mark, 0.0, true, 0.0, 0.0);
            }
        });

        list.append(&button);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gtk4::TextBuffer;
    use rusqlite::OpenFlags;
    use tempfile::tempdir;
    use weland::db as weland_db;

    const SCRATCH_BOOK: &str =
        "/tmp/claude-1000/-home-andrew-Documents-Rust-weland/839cf43e-b477-43ff-8379-19470349a793/scratchpad/books/beowulf.wld";

    // Not a #[test] itself — must run from the crate's single GTK-backed
    // #[test] entry point (see node_index.rs's tests module for why).
    pub(crate) fn check_selection_anchoring_and_annotation_lookup() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("book.wld");
        std::fs::copy(SCRATCH_BOOK, &path)
            .unwrap_or_else(|e| panic!("copy scratch fixture {SCRATCH_BOOK}: {e} (run `cargo test` once to recompile fixtures first)"));
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();

        let nodes = weland_db::load_ast_nodes(&conn).unwrap();
        let buffer = TextBuffer::new(None);
        let text_view = TextView::with_buffer(&buffer);
        let tags = crate::document::build_tags(&buffer);
        let mut index = NodeIndex::new();
        let mut pending_images = Vec::new();
        crate::document::build_document(&text_view, &buffer, &nodes, &tags, &mut index, &mut pending_images);

        let paragraph = nodes.iter().find(|n| n.node_type == "paragraph").expect("fixture must contain a paragraph node");
        let content = paragraph.content.clone().unwrap_or_default();
        assert!(content.chars().count() >= 4, "need a paragraph with at least 4 characters to test a sub-range selection");

        let content_start = content_start_offset(&buffer, paragraph, &index).unwrap();
        let sel_start = buffer.iter_at_offset(content_start);
        let sel_end = buffer.iter_at_offset(content_start + 3);
        buffer.select_range(&sel_start, &sel_end);

        let anchor = resolve_selection_anchor(&buffer, &nodes, &index).expect("a 3-char selection inside a paragraph must resolve");
        assert_eq!(anchor.node_id, paragraph.id);
        assert_eq!(anchor.start_offset, 0);
        assert_eq!(anchor.end_offset, 3);
        assert_eq!(anchor.selected_text, content.chars().take(3).collect::<String>());

        // A selection crossing into the very next node's mark must be rejected.
        let next_mark_offset = {
            let idx = nodes.iter().position(|n| n.id == paragraph.id).unwrap();
            nodes.get(idx + 1).and_then(|n| index.mark_for_node(n.id)).map(|m| buffer.iter_at_mark(m).offset())
        };
        if let Some(next_offset) = next_mark_offset {
            let cross_start = buffer.iter_at_offset(content_start);
            let cross_end = buffer.iter_at_offset(next_offset + 1);
            buffer.select_range(&cross_start, &cross_end);
            assert!(resolve_selection_anchor(&buffer, &nodes, &index).is_none(), "a selection crossing a node boundary must not resolve");
        }

        // A stanza-opening verse line's content starts one character after
        // its own recorded mark (the blank line inserted before it).
        let stanza_start_line = nodes.iter().find(|n| {
            n.node_type == "verse_line" && n.attributes.as_ref().and_then(|a| a.get("stanza_start")).and_then(|v| v.as_bool()).unwrap_or(false)
        });
        if let Some(verse) = stanza_start_line {
            let mark_offset = buffer.iter_at_mark(index.mark_for_node(verse.id).unwrap()).offset();
            let content_start = content_start_offset(&buffer, verse, &index).unwrap();
            assert_eq!(content_start, mark_offset + 1, "a stanza-start verse line's content must start one char after its mark");
        }

        // A selection on a non-annotatable node type (list/table/etc.) must
        // not resolve, even though it's a single-node, in-bounds selection.
        if let Some(list_node) = nodes.iter().find(|n| n.node_type == "list" || n.node_type == "table") {
            if let Some(mark) = index.mark_for_node(list_node.id) {
                let start = buffer.iter_at_mark(mark);
                let mut end = start.clone();
                end.forward_char();
                if start != end {
                    buffer.select_range(&start, &end);
                    assert!(resolve_selection_anchor(&buffer, &nodes, &index).is_none(), "a selection on a {} node must not resolve", list_node.node_type);
                }
            }
        }

        // find_annotation_at round-trip against a real inserted annotation.
        let new = weland_db::NewAnnotation {
            node_id: paragraph.id,
            start_offset: 0,
            end_offset: 3,
            selected_text: Some(content.chars().take(3).collect()),
            annotation_type: "highlight".to_string(),
            comment: None,
            asset_id: None,
            author_name: "Test".to_string(),
        };
        let saved = weland_db::insert_annotation(&conn, new).unwrap();
        let annotation_index = AnnotationIndex::load(&conn).unwrap();

        let hit = find_annotation_at(&buffer, content_start + 1, &nodes, &index, &annotation_index);
        assert!(hit.is_some(), "a point inside the annotation's range must find it");
        assert_eq!(hit.unwrap().0.id, saved.id);

        let miss = find_annotation_at(&buffer, content_start + 3, &nodes, &index, &annotation_index);
        assert!(miss.is_none(), "the offset right after the annotation's end must not match (end_offset is exclusive)");
    }

    // Not a #[test] itself — see the note on `check_marks_stay_anchored_...`
    // in node_index.rs for why every GTK-touching check in this crate runs
    // from one shared entry point.
    pub(crate) fn check_selection_is_single_word() {
        let buffer = TextBuffer::new(None);
        let mut iter = buffer.end_iter();
        buffer.insert(&mut iter, "The villein recognized the speaker.");

        // Exactly "villein" (offsets 4..11) — what a double-click produces.
        let (start, end) = (buffer.iter_at_offset(4), buffer.iter_at_offset(11));
        assert!(selection_is_single_word(&start, &end), "a whole-word selection must be detected as a single word");

        // "The villein" (offsets 0..11) — two whole words back to back both
        // satisfy starts_word()/ends_word() individually, so this must NOT
        // be misdetected as a single word.
        let (start, end) = (buffer.iter_at_offset(0), buffer.iter_at_offset(11));
        assert!(!selection_is_single_word(&start, &end), "a two-word selection must not be treated as a single word");

        // "villei" (offsets 4..10) — a partial-word drag, ends mid-word.
        let (start, end) = (buffer.iter_at_offset(4), buffer.iter_at_offset(10));
        assert!(!selection_is_single_word(&start, &end), "a partial-word selection must not be treated as a single word");
    }
}
