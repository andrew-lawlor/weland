//! Renders `ast_nodes` into a `GtkTextView`: headings/paragraphs with inline
//! bold/italic `TextTag`s, blockquotes, verse, lists, tables, and images
//! placed inline via `GtkTextChildAnchor` + `gtk::Picture` — so images are
//! part of the text flow and scroll with it natively.

use gtk4::{self as gtk, glib, pango, prelude::*, Picture, Separator, TextBuffer, TextIter, TextTag, TextView};
use serde_json::Value;
use weland::schema::AstNode;

use crate::node_index::NodeIndex;

/// An image node whose `GtkTextChildAnchor`/`Picture` placeholder is already
/// in the buffer, but whose pixel data hasn't been decoded yet — decoding
/// synchronously for every image up front is what made opening a
/// 125-image book take several seconds before the window even painted.
/// The caller (`app::build_ui`) decodes these lazily off a `glib` idle
/// callback instead.
pub struct PendingImage {
    pub asset_id: i64,
    pub picture: Picture,
}

struct SpanAttr {
    start: usize,
    end: usize,
    ty: String,
}

fn parse_spans(attributes: &Option<Value>) -> Vec<SpanAttr> {
    let Some(attrs) = attributes else { return Vec::new() };
    let Some(arr) = attrs.get("spans").and_then(|v| v.as_array()) else { return Vec::new() };
    arr.iter()
        .filter_map(|s| {
            let start = s.get("start")?.as_u64()? as usize;
            let end = s.get("end")?.as_u64()? as usize;
            let ty = s.get("type")?.as_str()?.to_string();
            Some(SpanAttr { start, end, ty })
        })
        .collect()
}

pub struct Tags {
    pub h1: TextTag,
    pub h2: TextTag,
    pub paragraph: TextTag,
    bold: TextTag,
    italic: TextTag,
    // Also doubles as the verse line-number tag's styling — settings_ui.rs
    // toggles its `invisible` property for the "show verse numbers" setting.
    pub dim: TextTag,
    pub blockquote: TextTag,
    pub verse: TextTag,
    pub list_item: TextTag,
    table: TextTag,
}

pub fn build_tags(buffer: &TextBuffer) -> Tags {
    let h1 = buffer
        .create_tag(
            Some("h1"),
            &[
                ("weight", &700i32),
                ("scale", &1.6f64),
                ("foreground", &"#e0af68"),
                ("pixels-above-lines", &18i32),
                ("pixels-below-lines", &10i32),
            ],
        )
        .expect("create h1 tag");
    let h2 = buffer
        .create_tag(
            Some("h2"),
            &[
                ("weight", &700i32),
                ("scale", &1.25f64),
                ("foreground", &"#7dcfff"),
                ("pixels-above-lines", &14i32),
                ("pixels-below-lines", &8i32),
            ],
        )
        .expect("create h2 tag");
    let paragraph = buffer
        .create_tag(Some("paragraph"), &[("pixels-below-lines", &10i32)])
        .expect("create paragraph tag");
    let bold = buffer
        .create_tag(Some("bold"), &[("weight", &700i32)])
        .expect("create bold tag");
    let italic = buffer
        .create_tag(Some("italic"), &[("style", &pango::Style::Italic)])
        .expect("create italic tag");
    let dim = buffer
        .create_tag(Some("dim"), &[("foreground", &"#565f89")])
        .expect("create dim tag");
    let blockquote = buffer
        .create_tag(
            Some("blockquote"),
            &[
                ("style", &pango::Style::Italic),
                ("foreground", &"#9aa5ce"),
                ("left-margin", &28i32),
                ("pixels-below-lines", &8i32),
            ],
        )
        .expect("create blockquote tag");
    let verse = buffer
        .create_tag(
            Some("verse"),
            &[("style", &pango::Style::Italic), ("left-margin", &18i32)],
        )
        .expect("create verse tag");
    let list_item = buffer
        .create_tag(
            Some("list_item"),
            &[("left-margin", &24i32), ("pixels-below-lines", &2i32)],
        )
        .expect("create list_item tag");
    let table = buffer
        .create_tag(
            Some("table"),
            &[("family", &"monospace"), ("foreground", &"#9ece6a")],
        )
        .expect("create table tag");

    Tags { h1, h2, paragraph, bold, italic, dim, blockquote, verse, list_item, table }
}

fn breakpoints(len: usize, spans: &[SpanAttr]) -> Vec<usize> {
    let mut pts: Vec<usize> = vec![0, len];
    for s in spans {
        pts.push(s.start.min(len));
        pts.push(s.end.min(len));
    }
    pts.sort_unstable();
    pts.dedup();
    pts
}

fn active_span_tags<'a>(mid: usize, spans: &[SpanAttr], tags: &'a Tags) -> Vec<&'a TextTag> {
    spans
        .iter()
        .filter(|s| mid >= s.start && mid < s.end)
        .filter_map(|s| match s.ty.as_str() {
            "bold" => Some(&tags.bold),
            "italic" => Some(&tags.italic),
            // Both the compiler's leading stanza numbers and trailing line
            // numbers are "verse numbers" as far as the reading-settings
            // toggle is concerned — the old Tauri reader hides both through
            // one `.hide-verse-numbers` CSS rule (`app.js`/`styles.css`).
            // This port only ever wired up `line_number`; `stanza_number`
            // (the more common of the two in practice -- e.g. every 5th
            // line numbered in the margin) fell through to plain body text
            // and never responded to the toggle at all.
            "line_number" | "stanza_number" => Some(&tags.dim),
            _ => None,
        })
        .collect()
}

/// Inserts `content` at `iter`, splitting on span boundaries so overlapping
/// inline formatting (bold/italic/etc.) renders correctly, and advances `iter`
/// past the inserted text.
fn insert_runs(buffer: &TextBuffer, iter: &mut TextIter, content: &str, spans: &[SpanAttr], tags: &Tags, base: &[&TextTag]) {
    let chars: Vec<char> = content.chars().collect();
    let len = chars.len();
    if len == 0 {
        return;
    }
    let pts = breakpoints(len, spans);
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        let run: String = chars[a..b].iter().collect();
        let mut all_tags: Vec<&TextTag> = base.to_vec();
        all_tags.extend(active_span_tags(a, spans, tags));
        if all_tags.is_empty() {
            buffer.insert(iter, &run);
        } else {
            buffer.insert_with_tags(iter, &run, &all_tags);
        }
    }
}

/// Inserts a placeholder `Picture` (no pixel data yet, so no decode cost)
/// and queues it in `pending` for the caller to fill in lazily.
fn insert_image(text_view: &TextView, buffer: &TextBuffer, iter: &mut TextIter, asset_id: i64, pending: &mut Vec<PendingImage>) {
    let picture = Picture::new();
    picture.set_can_shrink(true);
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_margin_top(10);
    picture.set_margin_bottom(10);

    let anchor = buffer.create_child_anchor(iter);
    text_view.add_child_at_anchor(&picture, &anchor);
    buffer.insert(iter, "\n");

    crate::image_viewer::wire_click_to_open(text_view, &picture);

    pending.push(PendingImage { asset_id, picture });
}

fn insert_separator(text_view: &TextView, buffer: &TextBuffer, iter: &mut TextIter) {
    let sep = Separator::new(gtk::Orientation::Horizontal);
    sep.set_margin_top(10);
    sep.set_margin_bottom(10);
    let anchor = buffer.create_child_anchor(iter);
    text_view.add_child_at_anchor(&sep, &anchor);
    buffer.insert(iter, "\n");
}

pub fn build_document(
    text_view: &TextView,
    buffer: &TextBuffer,
    nodes: &[AstNode],
    tags: &Tags,
    index: &mut NodeIndex,
    pending_images: &mut Vec<PendingImage>,
) {
    let mut iter = buffer.end_iter();

    for node in nodes {
        index.record(buffer, &iter, node.id);

        let content = node.content.clone().unwrap_or_default();
        match node.node_type.as_str() {
            "heading" => {
                let level = node.attributes.as_ref().and_then(|a| a.get("level")).and_then(|v| v.as_i64()).unwrap_or(1);
                let spans = parse_spans(&node.attributes);
                let tag = if level <= 1 { &tags.h1 } else { &tags.h2 };
                insert_runs(buffer, &mut iter, &content, &spans, tags, &[tag]);
                buffer.insert(&mut iter, "\n");
            }
            "paragraph" => {
                let spans = parse_spans(&node.attributes);
                insert_runs(buffer, &mut iter, &content, &spans, tags, &[&tags.paragraph]);
                buffer.insert(&mut iter, "\n");
            }
            "blockquote" => {
                let spans = parse_spans(&node.attributes);
                insert_runs(buffer, &mut iter, &content, &spans, tags, &[&tags.blockquote]);
                buffer.insert(&mut iter, "\n");
            }
            "verse_line" => {
                let spans = parse_spans(&node.attributes);
                let stanza_start = node.attributes.as_ref().and_then(|a| a.get("stanza_start")).and_then(|v| v.as_bool()).unwrap_or(false);
                let verse_end = node.attributes.as_ref().and_then(|a| a.get("verse_end")).and_then(|v| v.as_bool()).unwrap_or(false);
                if stanza_start {
                    buffer.insert(&mut iter, "\n");
                }
                insert_runs(buffer, &mut iter, &content, &spans, tags, &[&tags.verse]);
                buffer.insert(&mut iter, "\n");
                if verse_end {
                    buffer.insert(&mut iter, "\n");
                }
            }
            "list" => {
                let ordered = node.attributes.as_ref().and_then(|a| a.get("ordered")).and_then(|v| v.as_bool()).unwrap_or(false);
                let items = node.attributes.as_ref().and_then(|a| a.get("items")).and_then(|v| v.as_array()).cloned().unwrap_or_default();
                for (i, item) in items.iter().enumerate() {
                    let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let item_spans: Vec<SpanAttr> = item
                        .get("spans")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|s| {
                                    let start = s.get("start")?.as_u64()? as usize;
                                    let end = s.get("end")?.as_u64()? as usize;
                                    let ty = s.get("type")?.as_str()?.to_string();
                                    Some(SpanAttr { start, end, ty })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let prefix = if ordered { format!("{}. ", i + 1) } else { "\u{2022} ".to_string() };
                    buffer.insert_with_tags(&mut iter, &prefix, &[&tags.list_item]);
                    insert_runs(buffer, &mut iter, &text, &item_spans, tags, &[&tags.list_item]);
                    buffer.insert(&mut iter, "\n");
                }
                buffer.insert(&mut iter, "\n");
            }
            "table" => {
                let rows = node.attributes.as_ref().and_then(|a| a.get("rows")).and_then(|v| v.as_array()).cloned().unwrap_or_default();
                for row in rows {
                    let cells: Vec<String> = row.as_array().map(|arr| arr.iter().map(|c| c.as_str().unwrap_or("").to_string()).collect()).unwrap_or_default();
                    let joined = cells.join("  |  ");
                    buffer.insert_with_tags(&mut iter, &joined, &[&tags.table]);
                    buffer.insert(&mut iter, "\n");
                }
                buffer.insert(&mut iter, "\n");
            }
            "thematic_break" => {
                insert_separator(text_view, buffer, &mut iter);
            }
            "image" => {
                if let Some(asset_id) = node.attributes.as_ref().and_then(|a| a.get("asset_id")).and_then(|v| v.as_i64()) {
                    insert_image(text_view, buffer, &mut iter, asset_id, pending_images);
                }
            }
            _ => {}
        }
    }
}

/// Cap on a decoded image's displayed height — without this, a single
/// full-page plate would dominate the reading pane.
const MAX_IMAGE_HEIGHT: i32 = 340;

/// A `GtkTextChildAnchor` widget is allocated exactly its own natural size
/// within the text flow — `picture.set_halign(Center)` is a no-op there,
/// since there's no extra space around it to center *into*. Real centering
/// means reserving that extra space ourselves via symmetric start/end
/// margins sized off the current view width, which changes both on window
/// resize and as each image's real size becomes known after lazy decode —
/// so this just recomputes on every frame rather than chasing the "right"
/// one-shot signal for either case.
///
/// Also does the sizing itself, not just centering: a `Picture`'s natural
/// size is its source pixbuf's own resolution (same trap as the library
/// grid's covers — see `library.rs`'s `decode_cover`), so a wide plate (e.g.
/// Walden's imprint page) requested at its intrinsic width forced the
/// `TextView` wider than the pane and produced horizontal scroll. Capping to
/// `MAX_IMAGE_HEIGHT` alone isn't enough for a wide-but-short image, so this
/// also clamps to the view's current usable width, whichever is more
/// restrictive. `paintable()` reads back `None` before that image has
/// decoded, so those are skipped until their turn.
pub fn wire_image_centering(text_view: &TextView, pictures: Vec<Picture>) {
    text_view.add_tick_callback(move |tv, _clock| {
        let usable = tv.width() - tv.left_margin() - tv.right_margin();
        if usable <= 0 {
            return glib::ControlFlow::Continue;
        }
        for picture in &pictures {
            let Some(paintable) = picture.paintable() else { continue };
            let (iw, ih) = (paintable.intrinsic_width(), paintable.intrinsic_height());
            if iw <= 0 || ih <= 0 {
                continue;
            }

            let mut target_h = MAX_IMAGE_HEIGHT.min(ih);
            let mut target_w = (iw as i64 * target_h as i64 / ih as i64) as i32;
            if target_w > usable {
                target_w = usable;
                target_h = (ih as i64 * target_w as i64 / iw as i64) as i32;
            }
            picture.set_size_request(target_w.max(1), target_h.max(1));

            let margin = ((usable - target_w) / 2).max(0);
            picture.set_margin_start(margin);
            picture.set_margin_end(margin);
        }
        glib::ControlFlow::Continue
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gtk4::TextBuffer;

    // Not a #[test] itself -- see node_index.rs's identical note on why every
    // GTK-touching check in this crate runs from one shared #[test] entry
    // point instead of independently claiming a thread.
    pub(crate) fn check_stanza_and_line_numbers_both_use_dim_tag() {
        let buffer = TextBuffer::new(None);
        let tags = build_tags(&buffer);
        let spans = vec![
            SpanAttr { start: 0, end: 2, ty: "stanza_number".to_string() },
            SpanAttr { start: 2, end: 5, ty: "line_number".to_string() },
            SpanAttr { start: 5, end: 8, ty: "bold".to_string() },
        ];

        let stanza_tags = active_span_tags(0, &spans, &tags);
        assert_eq!(stanza_tags.len(), 1);
        assert!(
            std::ptr::eq(stanza_tags[0], &tags.dim),
            "leading stanza numbers must render with the same tag the verse-numbers setting toggles"
        );

        let line_tags = active_span_tags(2, &spans, &tags);
        assert_eq!(line_tags.len(), 1);
        assert!(std::ptr::eq(line_tags[0], &tags.dim));

        let bold_tags = active_span_tags(5, &spans, &tags);
        assert_eq!(bold_tags.len(), 1);
        assert!(!std::ptr::eq(bold_tags[0], &tags.dim));
    }
}
