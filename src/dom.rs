use crate::schema::Span;
use scraper::{ElementRef, Html, Node, Selector};

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
        element_id: Option<String>,
    },
    Paragraph {
        node_type: String, // "paragraph", "blockquote", "list"
        text: String,
        spans: Vec<Span>,
        source_file: String,
        footnotes: Vec<FootnoteRef>,
        element_id: Option<String>,
    },
    ThematicBreak {
        element_id: Option<String>,
    },
    Table {
        text: String,
        rows: Vec<Vec<String>>,
        source_file: String,
        element_id: Option<String>,
    },
    Image {
        src: String,
        alt: String,
        caption: String,
        element_id: Option<String>,
    },
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
    let mut collector = TextCollector::new();
    let mut raw_spans: Vec<Span> = Vec::new();

    fn walk(
        node_ref: ElementRef,
        collector: &mut TextCollector,
        raw_spans: &mut Vec<Span>,
    ) {
        for child in node_ref.children() {
            match child.value() {
                Node::Text(text_node) => {
                    collector.feed_text(text_node);
                }
                Node::Element(el_data) => {
                    let tag = el_data.name().to_lowercase();

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
                                // Skip internal footnote link from main text
                                continue;
                            }
                        }
                    }

                    collector.flush_pending_space();
                    let start = collector.char_len();

                    if let Some(child_el) = ElementRef::wrap(child) {
                        walk(child_el, collector, raw_spans);
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

    walk(element, &mut collector, &mut raw_spans);

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

/// Parses an HTML/XHTML chapter document into a structured list of ChapterElements.
pub fn parse_chapter_html(
    html_content: &str,
    chapter_path: &str,
) -> (Html, Vec<ChapterElement>) {
    let document = Html::parse_document(html_content);

    let body_selector = Selector::parse("body").unwrap();
    let body = match document.select(&body_selector).next() {
        Some(b) => b,
        None => return (document, Vec::new()),
    };

    let element_selector = Selector::parse(
        "h1, h2, h3, h4, h5, h6, p, blockquote, ul, ol, img, hr, table, svg image"
    ).unwrap();

    let mut elements = Vec::new();

    for elem in body.select(&element_selector) {
        let tag = elem.value().name().to_lowercase();
        let element_id = elem
            .value()
            .attr("id")
            .or_else(|| elem.value().attr("name"))
            .map(|s| s.to_string());

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
                    element_id,
                });
            }
            continue;
        }

        // 2. Thematic breaks (<hr>)
        if tag == "hr" {
            elements.push(ChapterElement::ThematicBreak { element_id });
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
                element_id,
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
                        element_id,
                    });
                }
                continue;
            }
        }

        // 5. Paragraphs, Blockquotes, Lists
        let TextAndSpans { text, spans } = extract_text_and_spans(elem);

        if !text.is_empty() {
            let node_type = match tag.as_str() {
                "blockquote" => "blockquote".to_string(),
                "ul" | "ol" => "list".to_string(),
                _ => "paragraph".to_string(),
            };

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
                element_id,
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
