use crate::schema::Span;
use scraper::{ElementRef, Html, Node, Selector};
use serde::Serialize;

/// Result of extracting normalized text and inline formatting spans from a DOM subtree.
#[derive(Debug, Clone, Default)]
pub struct TextAndSpans {
    pub text: String,
    pub spans: Vec<Span>,
}

/// Represents an intermediate element extracted from an HTML/XHTML chapter document.
#[derive(Debug, Clone)]
pub enum ChapterElement {
    Heading {
        level: u8,
        text: String,
        spans: Vec<Span>,
        element_ids: Vec<String>,
    },
    Paragraph {
        node_type: String, // "paragraph", "blockquote", "verse_line"
        text: String,
        spans: Vec<Span>,
        source_file: String,
        footnotes: Vec<FootnoteRef>,
        element_ids: Vec<String>,
        // Only meaningful for "verse_line": true if this line starts a new
        // stanza (its <p> has a different parent than the previous verse
        // line's), so the reader can add breathing room between stanzas
        // without also spacing out every line within one.
        stanza_start: bool,
        // Only meaningful for "verse_line": true if this is the last line
        // of its verse run, so the reader can restore the gap before
        // whatever (usually prose) comes next — verse lines are otherwise
        // zero-margin for tight in-stanza spacing.
        verse_end: bool,
    },
    ThematicBreak {
        element_ids: Vec<String>,
    },
    Table {
        text: String,
        rows: Vec<Vec<String>>,
        source_file: String,
        element_ids: Vec<String>,
    },
    Image {
        src: String,
        alt: String,
        caption: String,
        element_ids: Vec<String>,
    },
    List {
        ordered: bool,
        text: String,
        spans: Vec<Span>,
        items: Vec<ListItem>,
        source_file: String,
        element_ids: Vec<String>,
    },
}

/// A nested, ordered/unordered list, as found either at the top level of a
/// chapter or inside a single `<li>` (a sublist).
#[derive(Debug, Clone, Serialize)]
pub struct ListNode {
    pub ordered: bool,
    pub items: Vec<ListItem>,
}

/// A single `<li>`: its own inline text/spans (not including any nested
/// sublist's text), plus that sublist if it has one.
#[derive(Debug, Clone, Serialize)]
pub struct ListItem {
    pub text: String,
    pub spans: Vec<Span>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sublist: Option<Box<ListNode>>,
}

/// Represents a reference to a footnote found inside a block element.
#[derive(Debug, Clone)]
pub struct FootnoteRef {
    pub anchor_id: String,
    pub label: String,
}

/// Represents a resolved footnote ready to be inserted as a child AST node.
#[derive(Debug, Clone)]
pub struct ResolvedFootnote {
    pub anchor_id: String,
    pub label: String,
    pub text: String,
    pub spans: Vec<Span>,
}

/// Strips HTML tags and decodes entities from metadata fields, preserving paragraph breaks.
pub fn sanitize_metadata_text(input: &str) -> String {
    if !input.contains('<') && !input.contains('&') {
        return input.trim().to_string();
    }

    let fragment = Html::parse_fragment(input);
    let mut lines: Vec<String> = Vec::new();
    let mut current_line = String::new();

    // Tags recognized as real formatting/structure and stripped. Anything else
    // (e.g. a stray "<encoded markup>" that only looks like a tag) is rendered
    // back as literal text instead of being silently swallowed.
    fn is_known_tag(tag: &str) -> bool {
        matches!(
            tag,
            "p" | "div"
                | "br"
                | "li"
                | "ul"
                | "ol"
                | "table"
                | "tbody"
                | "thead"
                | "tr"
                | "blockquote"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "b"
                | "strong"
                | "i"
                | "em"
                | "u"
                | "span"
                | "small"
                | "sup"
                | "sub"
                | "s"
                | "strike"
                | "del"
                | "ins"
                | "mark"
                | "abbr"
                | "cite"
                | "q"
                | "code"
                | "tt"
                | "big"
                | "font"
                | "a"
        )
    }

    // Reconstructs an unrecognized element as literal inline text (tag name and
    // bare attributes, no synthetic closing tag), recursing so any recognized
    // tags nested inside it still get stripped.
    fn render_literal(node: ElementRef, out: &mut String) {
        for child in node.children() {
            match child.value() {
                Node::Text(t) => out.push_str(t),
                Node::Element(el) => {
                    let tag = el.name().to_lowercase();
                    if let Some(child_el) = ElementRef::wrap(child) {
                        if is_known_tag(&tag) {
                            render_literal(child_el, out);
                        } else {
                            out.push('<');
                            out.push_str(&tag);
                            for (name, value) in el.attrs() {
                                out.push(' ');
                                out.push_str(name);
                                if !value.is_empty() {
                                    out.push('=');
                                    out.push_str(value);
                                }
                            }
                            out.push('>');
                            render_literal(child_el, out);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn walk_node(node: ElementRef, current_line: &mut String, lines: &mut Vec<String>) {
        for child in node.children() {
            match child.value() {
                Node::Text(t) => {
                    let clean = t.trim();
                    if !clean.is_empty() {
                        if !current_line.is_empty() && !current_line.ends_with(' ') {
                            current_line.push(' ');
                        }
                        current_line.push_str(clean);
                    }
                }
                Node::Element(el) => {
                    let tag = el.name().to_lowercase();

                    if !is_known_tag(&tag) {
                        if let Some(child_el) = ElementRef::wrap(child) {
                            let mut literal = String::new();
                            literal.push('<');
                            literal.push_str(&tag);
                            for (name, value) in el.attrs() {
                                literal.push(' ');
                                literal.push_str(name);
                                if !value.is_empty() {
                                    literal.push('=');
                                    literal.push_str(value);
                                }
                            }
                            literal.push('>');
                            render_literal(child_el, &mut literal);

                            let clean = literal.trim();
                            if !clean.is_empty() {
                                if !current_line.is_empty() && !current_line.ends_with(' ') {
                                    current_line.push(' ');
                                }
                                current_line.push_str(clean);
                            }
                        }
                        continue;
                    }

                    let is_block = matches!(
                        tag.as_str(),
                        "p" | "div" | "br" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "tr"
                    );

                    if is_block && !current_line.trim().is_empty() {
                        lines.push(current_line.trim().to_string());
                        current_line.clear();
                    }

                    if let Some(child_el) = ElementRef::wrap(child) {
                        walk_node(child_el, current_line, lines);
                    }

                    if is_block && !current_line.trim().is_empty() {
                        lines.push(current_line.trim().to_string());
                        current_line.clear();
                    }
                }
                _ => {}
            }
        }
    }

    walk_node(fragment.root_element(), &mut current_line, &mut lines);

    if !current_line.trim().is_empty() {
        lines.push(current_line.trim().to_string());
    }

    if lines.is_empty() {
        input.trim().to_string()
    } else {
        lines.join("\n\n")
    }
}

/// State machine for normalizing whitespace while accurately recording span character offsets.
struct TextCollector {
    chars: Vec<char>,
    pending_space: bool,
}

impl TextCollector {
    fn new() -> Self {
        Self {
            chars: Vec::new(),
            pending_space: false,
        }
    }

    /// Current character index in the normalized string.
    fn char_len(&self) -> usize {
        self.chars.len()
    }

    /// Flushes any pending space so it's placed before the opening of an inline tag
    fn flush_pending_space(&mut self) {
        if self.pending_space && !self.chars.is_empty() {
            self.chars.push(' ');
            self.pending_space = false;
        }
    }

    /// Feeds raw text chunk into the collector, collapsing consecutive whitespace.
    fn feed_text(&mut self, text: &str) {
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !self.chars.is_empty() {
                    self.pending_space = true;
                }
            } else {
                if self.pending_space {
                    self.chars.push(' ');
                    self.pending_space = false;
                }
                self.chars.push(ch);
            }
        }
    }

    /// Inserts an explicit line break (from `<br>`), replacing any pending
    /// collapsed space rather than emitting both. A no-op before any content
    /// or immediately after another break, so consecutive `<br><br>` (or a
    /// `<br>` right at the start) doesn't stack up blank lines.
    fn feed_line_break(&mut self) {
        self.pending_space = false;
        if self.chars.is_empty() || self.chars.last() == Some(&'\n') {
            return;
        }
        self.chars.push('\n');
    }

    /// Converts the collected characters to a String and returns the final length.
    fn finish(mut self) -> (String, usize) {
        // Trim trailing space if any
        while let Some(&last) = self.chars.last() {
            if last.is_whitespace() {
                self.chars.pop();
            } else {
                break;
            }
        }
        let final_len = self.chars.len();
        let text: String = self.chars.into_iter().collect();
        (text, final_len)
    }
}

/// Recursively walks a DOM subtree to extract plain text with collapsed whitespace
/// and precise inline span character ranges.
pub fn extract_text_and_spans(element: ElementRef) -> TextAndSpans {
    extract_text_and_spans_opts(element, false)
}

/// Same as `extract_text_and_spans`, but stops at a nested `<ul>`/`<ol>` boundary
/// instead of walking into it. Used to get a single `<li>`'s own text without
/// also absorbing its sublist's text (which is extracted separately, into its
/// own nested `ListItem`s, by `build_list_items`).
fn extract_list_item_text(element: ElementRef) -> TextAndSpans {
    extract_text_and_spans_opts(element, true)
}

fn extract_text_and_spans_opts(element: ElementRef, stop_at_lists: bool) -> TextAndSpans {
    let mut collector = TextCollector::new();
    let mut raw_spans: Vec<Span> = Vec::new();

    fn walk(
        node_ref: ElementRef,
        collector: &mut TextCollector,
        raw_spans: &mut Vec<Span>,
        stop_at_lists: bool,
    ) {
        for child in node_ref.children() {
            match child.value() {
                Node::Text(text_node) => {
                    collector.feed_text(text_node);
                }
                Node::Element(el_data) => {
                    let tag = el_data.name().to_lowercase();

                    if stop_at_lists && (tag == "ul" || tag == "ol") {
                        continue;
                    }

                    // <br> was previously a silent no-op (dropped entirely) —
                    // the common single-paragraph-per-stanza convention for
                    // verse/addresses/lyrics (lines separated by <br> inside
                    // one <p>) collapsed into one run-on line-wrapped blob.
                    if tag == "br" {
                        collector.feed_line_break();
                        continue;
                    }

                    // Ignore inner footnote anchor tags in text calculation (handled separately)
                    if tag == "sup" {
                        let child_el = ElementRef::wrap(child);
                        if let Some(c_el) = child_el {
                            // If sup contains a link or footnote reference, skip it in main flow
                            if c_el.value().attr("class").map(|c| c.contains("footnote") || c.contains("noteref")).unwrap_or(false)
                                || c_el.children().any(|c| {
                                    if let Node::Element(e) = c.value() {
                                        e.name().eq_ignore_ascii_case("a")
                                            && e.attr("href").map(|h| h.contains('#')).unwrap_or(false)
                                    } else {
                                        false
                                    }
                                })
                            {
                                continue;
                            }
                        }
                    }

                    if tag == "a" {
                        if let Some(href) = el_data.attr("href") {
                            if href.contains('#') {
                                // Only skip anchors that actually look like footnote
                                // markers (explicit noteref/footnote class, or short
                                // numeric/symbol text like "1" or "†") — NOT any
                                // internally-linked anchor. Some EPUBs (e.g. Calibre
                                // conversions) wrap entire chapter headings in a
                                // self-referencing `<a href="...#chap-1">`; treating
                                // every `#` href as a footnote marker silently deleted
                                // those headings' text entirely.
                                let is_noteref_class = el_data
                                    .attr("class")
                                    .map(|c| c.contains("footnote") || c.contains("noteref"))
                                    .unwrap_or(false);
                                let looks_like_marker = ElementRef::wrap(child)
                                    .map(|a_el| {
                                        let marker_text: String = a_el.text().collect();
                                        let trimmed = marker_text.trim();
                                        !trimmed.is_empty()
                                            && trimmed.chars().count() <= 3
                                            && trimmed
                                                .chars()
                                                .all(|c| c.is_ascii_digit() || "*†‡§¶".contains(c))
                                    })
                                    .unwrap_or(false);
                                if is_noteref_class || looks_like_marker {
                                    continue;
                                }
                            }
                        }
                    }

                    collector.flush_pending_space();
                    let start = collector.char_len();

                    if let Some(child_el) = ElementRef::wrap(child) {
                        walk(child_el, collector, raw_spans, stop_at_lists);
                    }

                    let end = collector.char_len();

                    if start < end {
                        let span_type_and_href = match tag.as_str() {
                            "em" | "i" => Some(("italic".to_string(), None)),
                            "strong" | "b" => Some(("bold".to_string(), None)),
                            "code" | "tt" | "kbd" | "samp" => Some(("code".to_string(), None)),
                            "s" | "strike" | "del" => Some(("strikethrough".to_string(), None)),
                            "u" | "ins" => Some(("underline".to_string(), None)),
                            "mark" => Some(("highlight".to_string(), None)),
                            "small" => Some(("small".to_string(), None)),
                            "sub" => Some(("subscript".to_string(), None)),
                            "sup" => Some(("superscript".to_string(), None)),
                            "a" => {
                                el_data.attr("href").map(|href| ("link".to_string(), Some(href.to_string())))
                            }
                            _ => None,
                        };

                        if let Some((stype, href)) = span_type_and_href {
                            raw_spans.push(Span {
                                start,
                                end,
                                span_type: stype,
                                href,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    walk(element, &mut collector, &mut raw_spans, stop_at_lists);

    let (text, final_len) = collector.finish();

    // Adjust any spans that may have extended into trailing trimmed whitespace
    let mut clean_spans = Vec::new();
    for mut span in raw_spans {
        if span.start >= final_len {
            continue;
        }
        if span.end > final_len {
            span.end = final_len;
        }
        if span.start < span.end {
            clean_spans.push(span);
        }
    }

    TextAndSpans {
        text,
        spans: clean_spans,
    }
}

/// Some EPUBs (e.g. this book's publisher) mark up verse as one `<p>` per
/// line with no distinguishing class name or semantic marker at all — just
/// generic classes shared with ordinary prose paragraphs. There's no
/// reliable markup signal in that case, only a structural one: verse lines
/// run much shorter than prose paragraphs, and cluster together. Detects
/// maximal runs of short, verse-*candidate* `<p>`s at least `MIN_RUN` long
/// (long enough that it's very unlikely to just be a couple of short
/// ordinary sentences) and flags them all as verse — regardless of which
/// `<div>` groups them, since a single poem commonly spans several
/// differently-parented stanza wrappers.
///
/// Short length alone is a weak signal on its own — a Table of Contents
/// page and a run of quick dialogue are also runs of short `<p>`s, and were
/// getting misdetected as verse before the disqualifying signals below were
/// added. But a single disqualifying line can't be allowed to reject it
/// *outright* either — a poem is free to quote a character speaking
/// mid-stanza, and that opening-quote line shouldn't fall out of the run
/// and read as an out-of-place paragraph next to lines that are otherwise
/// clearly verse. So the boundary of a candidate run is decided by length
/// alone; whether the run as a whole gets accepted as verse is a separate
/// question, decided by what *fraction* of it is disqualifying (a run of
/// dialogue or Contents entries is disqualifying almost line for line; a
/// poem with one quoted line in it is not). Returns, indexed by the Nth
/// `<p>` encountered in document order (same order and same "every <p>
/// counts" rule the caller in `parse_chapter_html` uses to walk them in
/// lockstep): (is this line verse, does it start a new stanza).
fn detect_verse_paragraphs(body: ElementRef) -> (Vec<bool>, Vec<bool>, Vec<bool>) {
    const SHORT_LEN: usize = 60;
    const MIN_RUN: usize = 3;

    let p_selector = Selector::parse("p").unwrap();

    struct Entry<'a> {
        parent: Option<ElementRef<'a>>,
        is_short: bool,
        // A <p> wrapping nothing but e.g. a decorative section-break image
        // (`<p class="image"><img .../></p>`) — real content contributes
        // no text at all. Transparent for run/stanza purposes: it neither
        // breaks a verse run nor counts as a parent/stanza boundary, same
        // as it would if it weren't a <p> at all (most publishers put such
        // images directly in the flow, not wrapped in a <p>; this one
        // does, so it still needs an entry here to keep index alignment
        // with the caller's own "every <p> counts" counter).
        is_empty: bool,
        // Looks like dialogue (opens with a quotation mark) or a Contents
        // entry (one whole-line hyperlink) — a signal against the *run*
        // being verse at all, not grounds to drop this one line from an
        // otherwise-clearly-verse run.
        is_disqualifying: bool,
        starts_with_number: bool,
    }

    let entries: Vec<Entry> = body
        .select(&p_selector)
        .map(|p| {
            let TextAndSpans { text, spans } = extract_text_and_spans(p);
            let len = text.chars().count();
            Entry {
                parent: p.parent().and_then(ElementRef::wrap),
                is_short: len > 0 && len <= SHORT_LEN,
                is_empty: len == 0,
                is_disqualifying: starts_with_opening_quote(&text) || is_whole_line_link(len, &spans),
                starts_with_number: leading_number_len(&text).is_some(),
            }
        })
        .collect();

    let mut is_verse = vec![false; entries.len()];
    let mut is_stanza_start = vec![false; entries.len()];
    let mut is_verse_end = vec![false; entries.len()];

    let mut i = 0;
    while i < entries.len() {
        if entries[i].is_short {
            let start = i;
            let mut j = i;
            // An empty entry (e.g. a <p> wrapping only a decorative image)
            // doesn't break the run — look through it, same as if it
            // weren't a <p> at all.
            while j < entries.len() && (entries[j].is_short || entries[j].is_empty) {
                j += 1;
            }
            let short_count = entries[start..j].iter().filter(|e| e.is_short).count();
            let disqualified_count =
                entries[start..j].iter().filter(|e| e.is_short && e.is_disqualifying).count();
            if short_count >= MIN_RUN && disqualified_count * 2 < short_count {
                for k in start..j {
                    if entries[k].is_short {
                        is_verse[k] = true;
                    }
                }
                // Every verse_line gets margin: 0 for tight in-stanza
                // spacing (see styles.css), which also zeroes out the gap
                // after the run's last line — mark it so the reader can
                // restore breathing room before whatever (usually prose)
                // comes next.
                if let Some(last_short) = (start..j).rev().find(|&k| entries[k].is_short) {
                    is_verse_end[last_short] = true;
                }
                // Two independent stanza-boundary signals, since publishers
                // vary: some wrap each stanza in its own element (parent
                // changes at the boundary — this book's "raven" passage
                // does this), others run every stanza flat with no wrapper
                // at all and only mark the boundary with a leading stanza
                // number on the line itself (this book's Völuspá does
                // this, in the very same file). Either one, alone, is
                // enough to call it a new stanza — except a parent change
                // right after a group of just one line, which is more
                // often a markup artifact (e.g. a decorative section-break
                // image forcing an isolated line into its own wrapper
                // between two real stanzas, as in The Odyssey) than an
                // actual one-line stanza. A leading number is unaffected by
                // this — it's independently reliable regardless of group size.
                // Empty entries are skipped entirely here too, so they
                // can't masquerade as a stanza boundary on their own.
                let mut last_parent = None;
                let mut group_len = 0usize;
                for k in start..j {
                    if !entries[k].is_short {
                        continue;
                    }
                    let parent_changed = entries[k].parent != last_parent;
                    if parent_changed {
                        let previous_group_was_real_stanza = group_len >= 2;
                        if k == start || previous_group_was_real_stanza || entries[k].starts_with_number {
                            is_stanza_start[k] = true;
                        }
                        group_len = 0;
                    } else if entries[k].starts_with_number {
                        is_stanza_start[k] = true;
                    }
                    group_len += 1;
                    last_parent = entries[k].parent;
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }

    (is_verse, is_stanza_start, is_verse_end)
}

/// True if `text` opens with a quotation mark — i.e. this paragraph is
/// dialogue, not a candidate verse line. Checks straight and the common
/// curly/typographic quote characters (single and double) in either
/// direction, since EPUBs vary.
fn starts_with_opening_quote(text: &str) -> bool {
    matches!(
        text.chars().next(),
        Some('"' | '\'' | '\u{2018}' | '\u{2019}' | '\u{201C}' | '\u{201D}' | '\u{00AB}' | '\u{00BB}')
    )
}

/// True if this paragraph's entire text is covered by a single `link` span
/// — i.e. it's one whole-line hyperlink, like a Table of Contents entry
/// (`<p><a href="...">Chapter 1: ...</a></p>`), not a candidate verse line.
fn is_whole_line_link(text_len: usize, spans: &[Span]) -> bool {
    text_len > 0
        && spans
            .iter()
            .any(|s| s.span_type == "link" && s.start == 0 && s.end >= text_len)
}

/// If `text` opens with a short (1-4 digit) number, returns its character
/// length — the common way numbered-stanza verse marks where a new stanza
/// begins (e.g. "8 They played games in the grass,"), independent of
/// whatever element wraps it. Also used to carve that number off into its
/// own span so the reader can style it distinctly from the verse text.
fn leading_number_len(text: &str) -> Option<usize> {
    let digit_count = text.chars().take_while(|c| c.is_ascii_digit()).count();
    (1..=4).contains(&digit_count).then_some(digit_count)
}

/// If `text` ends with a short (1-4 digit) number, returns its character
/// length — classical verse translations commonly gutter-mark every Nth
/// line with its running line number (e.g. "...for our modern times.10"),
/// often glued directly onto the line with no separating space. Used to
/// carve that number off into its own span, same idea as
/// `leading_number_len` for stanza numbers, so it reads as a marker instead
/// of a typo stuck to the last word.
fn trailing_number_len(text: &str) -> Option<usize> {
    let digit_count = text.chars().rev().take_while(|c| c.is_ascii_digit()).count();
    (1..=4).contains(&digit_count).then_some(digit_count)
}

/// True if `elem` sits inside some other list's `<li>` — i.e. it's a sublist,
/// not a top-level list in its own right.
fn is_nested_in_list_item(elem: ElementRef) -> bool {
    for ancestor in elem.ancestors() {
        if let Some(el) = ElementRef::wrap(ancestor) {
            let name = el.value().name();
            if name.eq_ignore_ascii_case("li") {
                return true;
            }
            if name.eq_ignore_ascii_case("body") {
                return false;
            }
        }
    }
    false
}

/// True if `elem` contains no nested block-level element — i.e. it holds
/// only inline content (text, spans, links, `<br>`) and should be treated as
/// a paragraph-equivalent leaf, not a structural wrapper. Used for two
/// unrelated wrapper patterns: vintage EPUB2 conversions that mark every
/// paragraph as `<div class="...">` instead of `<p>` (a wrapper div
/// containing those leaf divs must NOT also be treated as one giant
/// paragraph), and books that wrap each verse line as
/// `<blockquote><p>...</p></blockquote>` purely for CSS indentation (the
/// wrapping blockquote must NOT also capture its child `<p>`'s text a second
/// time). Either way, a non-leaf wrapper is skipped so its block children
/// fall through and stand on their own.
fn is_leaf_content_div(elem: ElementRef, block_selector: &Selector) -> bool {
    elem.select(block_selector).next().is_none()
}

/// Collects the `id`/`name` attributes of every ancestor element between
/// `elem` and `<body>` (exclusive), innermost first. EPUBs commonly wrap each
/// addressable unit (a poem's canto, a play's scene) in a `<section id="...">`
/// with no id anywhere inside it, so a TOC/nav link to that id has nothing to
/// resolve to unless the first content element inside the wrapper claims the
/// wrapper's id too.
fn ancestor_element_ids(elem: ElementRef) -> Vec<String> {
    let mut ids = Vec::new();
    for ancestor in elem.ancestors() {
        let Some(el) = ElementRef::wrap(ancestor) else { continue };
        if el.value().name().eq_ignore_ascii_case("body") {
            break;
        }
        if let Some(id) = el.value().attr("id").or_else(|| el.value().attr("name")) {
            ids.push(id.to_string());
        }
    }
    ids
}

/// Recursively builds the structured item tree for a `<ul>`/`<ol>`: each
/// `<li>`'s own text/spans (stopping at any nested sublist boundary), plus
/// that sublist — if present — built the same way.
fn build_list_items(list_elem: ElementRef) -> Vec<ListItem> {
    let mut items = Vec::new();

    for child in list_elem.children() {
        let Some(li) = ElementRef::wrap(child) else { continue };
        if !li.value().name().eq_ignore_ascii_case("li") {
            continue;
        }

        let TextAndSpans { text, spans } = extract_list_item_text(li);

        let mut sublist: Option<Box<ListNode>> = None;
        for sub_child in li.children() {
            if let Some(sub_el) = ElementRef::wrap(sub_child) {
                let sub_tag = sub_el.value().name();
                if sub_tag.eq_ignore_ascii_case("ul") || sub_tag.eq_ignore_ascii_case("ol") {
                    sublist = Some(Box::new(ListNode {
                        ordered: sub_tag.eq_ignore_ascii_case("ol"),
                        items: build_list_items(sub_el),
                    }));
                    break;
                }
            }
        }

        if !text.is_empty() || sublist.is_some() {
            items.push(ListItem { text, spans, sublist });
        }
    }

    items
}

/// Parses an HTML/XHTML chapter document into a structured list of ChapterElements.
/// Removes `<span epub:type="pagebreak" ...>` print-pagination markers from
/// the raw HTML text before it's parsed — both the self-closing form
/// (`<span .../>`) and the paired form with a short visible page number
/// inside (`<span ...>{126}</span>`).
///
/// This has to happen here, on the raw text, rather than by walking the
/// parsed DOM and skipping matching elements: `span` isn't a "void" HTML
/// element, so html5ever (via `scraper`) doesn't honor the `/>`
/// self-closing syntax on it — `<span .../>` parses as an *unclosed
/// opening* tag, and every real word of text that followed it in the
/// source becomes its child, all the way to the end of the enclosing
/// paragraph. An element-level "skip this node" check then silently
/// discarded that swallowed real content along with the marker (this
/// exact bug shipped once already). Stripping the marker out of the raw
/// text first means that malformed tree never gets built in the first
/// place.
fn strip_pagebreak_markers(html: &str) -> String {
    const NEEDLE: &str = "epub:type=\"pagebreak";
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(tag_start) = rest.find("<span") {
        let Some(tag_end_rel) = rest[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + tag_end_rel;
        let tag_text = &rest[tag_start..=tag_end];

        if !tag_text.contains(NEEDLE) {
            out.push_str(&rest[..=tag_end]);
            rest = &rest[tag_end + 1..];
            continue;
        }

        out.push_str(&rest[..tag_start]);
        let self_closing = tag_text[..tag_text.len() - 1].trim_end().ends_with('/');
        rest = if self_closing {
            &rest[tag_end + 1..]
        } else if let Some(close_rel) = rest[tag_end + 1..].find("</span>") {
            &rest[tag_end + 1 + close_rel + "</span>".len()..]
        } else {
            // No closing tag at all — drop only up through here rather
            // than risk swallowing the rest of the document defensively.
            &rest[tag_end + 1..]
        };
    }
    out.push_str(rest);
    out
}

pub fn parse_chapter_html(
    html_content: &str,
    chapter_path: &str,
) -> (Html, Vec<ChapterElement>) {
    let cleaned = strip_pagebreak_markers(html_content);
    let document = Html::parse_document(&cleaned);

    let body_selector = Selector::parse("body").unwrap();
    let body = match document.select(&body_selector).next() {
        Some(b) => b,
        None => return (document, Vec::new()),
    };

    let element_selector = Selector::parse(
        "h1, h2, h3, h4, h5, h6, p, blockquote, ul, ol, img, hr, table, svg image, div"
    ).unwrap();
    let block_selector = Selector::parse(
        "div, p, h1, h2, h3, h4, h5, h6, ul, ol, table, blockquote"
    ).unwrap();

    let mut elements = Vec::new();
    let (verse_flags, stanza_start_flags, verse_end_flags) = detect_verse_paragraphs(body);
    let mut p_index = 0;

    for elem in body.select(&element_selector) {
        let tag = elem.value().name().to_lowercase();
        // EPUBs commonly wrap each addressable unit (a canto, a scene) in a
        // `<section id="...">` with no id anywhere inside it — TOC/nav links
        // point straight at that wrapper id, so the first content element
        // inside it needs to answer to it too, not just its own id (if any).
        let mut element_ids: Vec<String> = elem
            .value()
            .attr("id")
            .or_else(|| elem.value().attr("name"))
            .map(|s| s.to_string())
            .into_iter()
            .collect();
        element_ids.extend(ancestor_element_ids(elem));

        // 1. Standalone Images (HTML <img> and SVG <image>)
        if tag == "img" || tag == "image" {
            let src = elem
                .value()
                .attr("src")
                .or_else(|| elem.value().attr("xlink:href"))
                .or_else(|| elem.value().attr("href"));

            if let Some(src_val) = src {
                let alt = elem.value().attr("alt").unwrap_or("").to_string();
                let caption = elem.value().attr("title").unwrap_or("").to_string();

                elements.push(ChapterElement::Image {
                    src: src_val.to_string(),
                    alt,
                    caption,
                    element_ids,
                });
            }
            continue;
        }

        // 2. Thematic breaks (<hr>)
        if tag == "hr" {
            elements.push(ChapterElement::ThematicBreak { element_ids });
            continue;
        }

        // 3. Tables
        if tag == "table" {
            let tr_selector = Selector::parse("tr").unwrap();
            let cell_selector = Selector::parse("th, td").unwrap();

            let mut rows = Vec::new();
            for tr in elem.select(&tr_selector) {
                let mut row_data = Vec::new();
                for cell in tr.select(&cell_selector) {
                    let cell_text = cell.text().collect::<Vec<_>>().join(" ");
                    let clean_cell = cell_text.split_whitespace().collect::<Vec<_>>().join(" ");
                    row_data.push(clean_cell);
                }
                if !row_data.is_empty() {
                    rows.push(row_data);
                }
            }

            let plain_text = rows
                .iter()
                .map(|r| r.join(" "))
                .collect::<Vec<_>>()
                .join(" ");

            elements.push(ChapterElement::Table {
                text: plain_text,
                rows,
                source_file: chapter_path.to_string(),
                element_ids,
            });
            continue;
        }

        // 4. Headings (h1..h6)
        if tag.starts_with('h') && tag.len() == 2 {
            if let Ok(level) = tag[1..].parse::<u8>() {
                let TextAndSpans { text, spans } = extract_text_and_spans(elem);
                if !text.is_empty() {
                    elements.push(ChapterElement::Heading {
                        level,
                        text,
                        spans,
                        element_ids,
                    });
                }
                continue;
            }
        }

        // 5. Lists (ul/ol)
        //
        // `body.select(&element_selector)` matches every ul/ol regardless of
        // nesting depth, so a sublist inside another list's <li> would
        // otherwise turn up here a second time as its own top-level list —
        // skip it; it's already captured recursively by build_list_items
        // below, as part of its parent item.
        if tag == "ul" || tag == "ol" {
            if is_nested_in_list_item(elem) {
                continue;
            }
            let TextAndSpans { text, spans } = extract_text_and_spans(elem);
            if !text.is_empty() {
                elements.push(ChapterElement::List {
                    ordered: tag == "ol",
                    text,
                    spans,
                    items: build_list_items(elem),
                    source_file: chapter_path.to_string(),
                    element_ids,
                });
            }
            continue;
        }

        // 5.5. Leaf <div> paragraphs — vintage EPUB2 books that never use
        // <p> at all, marking every paragraph as a styled <div> instead.
        // A disqualified div (a structural wrapper, or one already owned by
        // a list item) is skipped here; a qualifying one just falls through
        // to the paragraph handling below exactly like a <p> would.
        if tag == "div"
            && (!is_leaf_content_div(elem, &block_selector) || is_nested_in_list_item(elem))
        {
            continue;
        }

        // 5.6. Blockquote wrappers around block-level content — e.g.
        // `<blockquote><p>...</p></blockquote>` repeated per verse line,
        // used purely for CSS indentation. `body.select(&element_selector)`
        // already matches the inner <p> as its own element (correctly
        // classified as verse_line/paragraph); if the blockquote is also
        // pushed as a node, its `extract_text_and_spans` recurses into that
        // same <p> and duplicates the text. Only a blockquote with pure
        // inline content (no nested block) is a leaf worth keeping as its
        // own `blockquote` node.
        if tag == "blockquote" && !is_leaf_content_div(elem, &block_selector) {
            continue;
        }

        // 6. Paragraphs, Blockquotes, Verse lines
        //
        // p_index must advance for every <p> match here, whether or not it
        // ends up empty/skipped below, to stay aligned with
        // detect_verse_paragraphs's own "every <p> counts" indexing.
        let (is_verse, stanza_start, verse_end) = if tag == "p" {
            let v = verse_flags.get(p_index).copied().unwrap_or(false);
            let s = stanza_start_flags.get(p_index).copied().unwrap_or(false);
            let e = verse_end_flags.get(p_index).copied().unwrap_or(false);
            p_index += 1;
            (v, s, e)
        } else {
            (false, false, false)
        };

        let TextAndSpans { text, mut spans } = extract_text_and_spans(elem);

        if !text.is_empty() {
            let node_type = if tag == "blockquote" {
                "blockquote".to_string()
            } else if is_verse {
                "verse_line".to_string()
            } else {
                "paragraph".to_string()
            };

            // Carve the leading stanza number and/or trailing line number
            // (if present) off into their own spans so the reader can style
            // them distinctly from the verse text, rather than either
            // looking like a stray digit stuck to the first/last word.
            if is_verse {
                if let Some(len) = leading_number_len(&text) {
                    spans.push(Span {
                        start: 0,
                        end: len,
                        span_type: "stanza_number".to_string(),
                        href: None,
                    });
                }
                let total_len = text.chars().count();
                if let Some(len) = trailing_number_len(&text) {
                    // Guard a line that's entirely a short number (matching
                    // both checks) from double-counting the same digits as
                    // two overlapping spans.
                    if len < total_len {
                        spans.push(Span {
                            start: total_len - len,
                            end: total_len,
                            span_type: "line_number".to_string(),
                            href: None,
                        });
                    }
                }
            }

            // Extract footnote links referenced inside this element
            let mut footnotes = Vec::new();
            let note_link_selector = Selector::parse("a[href*='#'], sup a").unwrap();

            for note_link in elem.select(&note_link_selector) {
                if let Some(href) = note_link.value().attr("href") {
                    if let Some(pos) = href.find('#') {
                        let anchor_id = href[pos + 1..].to_string();
                        let label = note_link.text().collect::<Vec<_>>().join("").trim().to_string();
                        if !anchor_id.is_empty() {
                            footnotes.push(FootnoteRef { anchor_id, label });
                        }
                    }
                }
            }

            elements.push(ChapterElement::Paragraph {
                node_type,
                text,
                spans,
                source_file: chapter_path.to_string(),
                footnotes,
                element_ids,
                stanza_start,
                verse_end,
            });
        }
    }

    (document, elements)
}

/// Resolves a footnote element by target ID in the document tree.
pub fn resolve_footnote(document: &Html, anchor_id: &str, label: &str) -> Option<ResolvedFootnote> {
    // Try CSS selector #anchor_id, escaping if necessary
    let selector_str = format!("#{}", anchor_id);
    if let Ok(sel) = Selector::parse(&selector_str) {
        if let Some(target_el) = document.select(&sel).next() {
            let TextAndSpans { text, spans } = extract_text_and_spans(target_el);
            if !text.is_empty() {
                return Some(ResolvedFootnote {
                    anchor_id: anchor_id.to_string(),
                    label: label.to_string(),
                    text,
                    spans,
                });
            }
        }
    }

    // Fallback: search all elements with id or name attribute matching anchor_id
    for node in document.tree.nodes() {
        if let Some(el) = ElementRef::wrap(node) {
            let matches = el.value().attr("id") == Some(anchor_id)
                || el.value().attr("name") == Some(anchor_id);

            if matches {
                let TextAndSpans { text, spans } = extract_text_and_spans(el);
                if !text.is_empty() {
                    return Some(ResolvedFootnote {
                        anchor_id: anchor_id.to_string(),
                        label: label.to_string(),
                        text,
                        spans,
                    });
                }
            }
        }
    }

    None
}
