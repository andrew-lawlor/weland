use anyhow::{anyhow, Context, Result};
use percent_encoding::percent_decode_str;
use roxmltree::Document;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use zip::ZipArchive;

/// Represents an item in the EPUB manifest.
#[derive(Debug, Clone)]
pub struct ManifestItem {
    pub id: String,
    pub href: String,
    pub media_type: String,
    pub properties: Option<String>,
}

/// Represents a parsed hierarchical Table of Contents item.
#[derive(Debug, Clone)]
pub struct RawTocItem {
    pub title: String,
    pub href: String,
    pub children: Vec<RawTocItem>,
}

/// Represents extracted EPUB metadata.
#[derive(Debug, Clone, Default)]
pub struct EpubMetadata {
    pub title: String,
    pub author: String,
    pub language: String,
    pub description: Option<String>,
    pub identifier: Option<String>,
    pub publisher: Option<String>,
    pub date: Option<String>,
    pub rights: Option<String>,
    pub cover_href: Option<String>,
    pub toc_href: Option<String>,
}

/// Serializes an XML element's inner content back into a markup string, so that
/// real child elements (e.g. `<b>`, `<i>`) round-trip as literal tags. Text content
/// is passed through as roxmltree decoded it (one XML-entity decode pass), which
/// means text that was itself double-escaped in the source (e.g. a description
/// field containing `&lt;b&gt;...&lt;/b&gt;` so it renders as HTML in reading apps)
/// comes out as literal `<b>...</b>` markup here too. Either way, downstream
/// HTML-aware sanitization sees real tag syntax for real formatting and can strip it.
fn inner_xml_text(node: roxmltree::Node) -> Option<String> {
    fn walk(node: roxmltree::Node, out: &mut String) {
        for child in node.children() {
            if child.is_text() {
                if let Some(t) = child.text() {
                    out.push_str(t);
                }
            } else if child.is_element() {
                let tag = child.tag_name().name();
                out.push('<');
                out.push_str(tag);
                out.push('>');
                walk(child, out);
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
        }
    }

    let mut result = String::new();
    walk(node, &mut result);
    let trimmed = result.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Helper to normalize paths inside a zip archive (POSIX style, no leading slash, resolving . and ..).
pub fn normalize_zip_path(path: &str) -> String {
    let decoded = percent_decode_str(path).decode_utf8_lossy().to_string();
    let clean = decoded.split('#').next().unwrap_or(&decoded);
    let clean = clean.split('?').next().unwrap_or(clean);

    let parts = clean.replace('\\', "/");
    let mut stack: Vec<&str> = Vec::new();

    for part in parts.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            stack.pop();
        } else {
            stack.push(part);
        }
    }

    stack.join("/")
}

/// Joins a base directory path with a relative target path and normalizes it, preserving #fragment if present.
pub fn resolve_relative_path(base_dir: &str, relative_path: &str) -> String {
    let (path_part, frag_part) = match relative_path.find('#') {
        Some(pos) => (&relative_path[..pos], Some(&relative_path[pos + 1..])),
        None => (relative_path, None),
    };

    let combined = if base_dir.is_empty() {
        path_part.to_string()
    } else {
        format!("{}/{}", base_dir, path_part)
    };

    let norm = normalize_zip_path(&combined);
    match frag_part {
        Some(frag) if !frag.is_empty() => format!("{}#{}", norm, frag),
        _ => norm,
    }
}

/// Returns the parent directory of a normalized zip path.
pub fn get_parent_dir(path: &str) -> String {
    if let Some(pos) = path.rfind('/') {
        path[..pos].to_string()
    } else {
        String::new()
    }
}

/// Determines the MIME type based on file path and extension.
pub fn get_mime_type(file_path: &str) -> String {
    let lower = file_path.to_lowercase();
    if lower.ends_with(".png") {
        "image/png".to_string()
    } else if lower.ends_with(".webp") {
        "image/webp".to_string()
    } else if lower.ends_with(".svg") {
        "image/svg+xml".to_string()
    } else if lower.ends_with(".gif") {
        "image/gif".to_string()
    } else if lower.ends_with(".avif") {
        "image/avif".to_string()
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg".to_string()
    } else if lower.ends_with(".xhtml") || lower.ends_with(".html") || lower.ends_with(".htm") {
        "application/xhtml+xml".to_string()
    } else if lower.ends_with(".css") {
        "text/css".to_string()
    } else if lower.ends_with(".ncx") {
        "application/x-dtbncx+xml".to_string()
    } else if lower.ends_with(".mp3") {
        "audio/mpeg".to_string()
    } else if lower.ends_with(".wav") {
        "audio/wav".to_string()
    } else if lower.ends_with(".m4a") {
        "audio/mp4".to_string()
    } else {
        mime_guess::from_path(file_path)
            .first_raw()
            .unwrap_or("application/octet-stream")
            .to_string()
    }
}

/// In-memory or streaming reader for an EPUB ZIP archive.
pub struct EpubArchive {
    archive: ZipArchive<File>,
    pub opf_path: String,
    pub opf_dir: String,
    pub metadata: EpubMetadata,
    pub manifest: HashMap<String, ManifestItem>,
    pub spine_paths: Vec<String>,
}

impl EpubArchive {
    /// Opens an EPUB file and parses container.xml and the OPF package document.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::open(path.as_ref())
            .with_context(|| format!("Failed to open EPUB file: {}", path.as_ref().display()))?;
        let mut archive = ZipArchive::new(file)
            .with_context(|| "Failed to parse zip archive from EPUB file")?;

        // 1. Parse META-INF/container.xml
        let opf_path = Self::read_container_opf_path(&mut archive)?;
        let opf_dir = get_parent_dir(&opf_path);

        // 2. Read and parse OPF XML
        let opf_content = Self::read_archive_string(&mut archive, &opf_path)
            .with_context(|| format!("Failed to read OPF file at: {}", opf_path))?;

        let (metadata, manifest, spine_paths) = Self::parse_opf(&opf_content, &opf_dir)?;

        Ok(Self {
            archive,
            opf_path,
            opf_dir,
            metadata,
            manifest,
            spine_paths,
        })
    }

    /// Reads `META-INF/container.xml` to locate the OPF package rootfile.
    fn read_container_opf_path(archive: &mut ZipArchive<File>) -> Result<String> {
        let container_xml = Self::read_archive_string(archive, "META-INF/container.xml")
            .context("Invalid EPUB: META-INF/container.xml missing or unreadable")?;

        let doc = Document::parse(&container_xml)
            .context("Failed to parse META-INF/container.xml as valid XML")?;

        let rootfile_node = doc
            .descendants()
            .find(|n| n.tag_name().name() == "rootfile" && n.has_attribute("full-path"))
            .ok_or_else(|| anyhow!("Could not locate <rootfile full-path=\"...\"> in container.xml"))?;

        let full_path = rootfile_node
            .attribute("full-path")
            .ok_or_else(|| anyhow!("Rootfile missing full-path attribute"))?;

        Ok(normalize_zip_path(full_path))
    }

    /// Parses the OPF document extracting metadata, manifest, spine, and cover image.
    fn parse_opf(
        opf_xml: &str,
        opf_dir: &str,
    ) -> Result<(EpubMetadata, HashMap<String, ManifestItem>, Vec<String>)> {
        let doc = Document::parse(opf_xml).context("Failed to parse OPF package document as valid XML")?;

        // Metadata extraction
        let mut title = None;
        let mut author = None;
        let mut language = None;
        let mut description = None;
        let mut identifier = None;
        let mut publisher = None;
        let mut date = None;
        let mut rights = None;
        let mut epub2_meta_cover_id = None;

        if let Some(metadata_node) = doc.descendants().find(|n| n.tag_name().name() == "metadata") {
            for child in metadata_node.children().filter(|n| n.is_element()) {
                let tag_name = child.tag_name().name();
                let text = child.text().map(|s| s.trim().to_string());
                let markup_text = inner_xml_text(child);

                match tag_name {
                    "title" => {
                        if title.is_none() && markup_text.is_some() {
                            title = markup_text;
                        }
                    }
                    "creator" => {
                        if author.is_none() && markup_text.is_some() {
                            author = markup_text;
                        }
                    }
                    "language" => {
                        if language.is_none() && text.is_some() {
                            language = text;
                        }
                    }
                    "description" => {
                        if description.is_none() && markup_text.is_some() {
                            description = markup_text;
                        }
                    }
                    "identifier" => {
                        if identifier.is_none() && text.is_some() {
                            identifier = text;
                        }
                    }
                    "publisher" => {
                        if publisher.is_none() && markup_text.is_some() {
                            publisher = markup_text;
                        }
                    }
                    "date" => {
                        if date.is_none() && text.is_some() {
                            date = text;
                        }
                    }
                    "rights" => {
                        if rights.is_none() && markup_text.is_some() {
                            rights = markup_text;
                        }
                    }
                    "meta" => {
                        if let (Some(name), Some(content)) =
                            (child.attribute("name"), child.attribute("content"))
                        {
                            if name.eq_ignore_ascii_case("cover") {
                                epub2_meta_cover_id = Some(content.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // Manifest extraction
        let mut manifest = HashMap::new();
        let mut manifest_items_list = Vec::new();

        if let Some(manifest_node) = doc.descendants().find(|n| n.tag_name().name() == "manifest") {
            for item_node in manifest_node
                .children()
                .filter(|n| n.is_element() && n.tag_name().name() == "item")
            {
                if let (Some(id), Some(href), Some(media_type)) = (
                    item_node.attribute("id"),
                    item_node.attribute("href"),
                    item_node.attribute("media-type"),
                ) {
                    let properties = item_node.attribute("properties").map(|s| s.to_string());
                    let item = ManifestItem {
                        id: id.to_string(),
                        href: href.to_string(),
                        media_type: media_type.to_string(),
                        properties,
                    };
                    manifest.insert(id.to_string(), item.clone());
                    manifest_items_list.push(item);
                }
            }
        }

        // Spine extraction
        let mut spine_paths = Vec::new();
        if let Some(spine_node) = doc.descendants().find(|n| n.tag_name().name() == "spine") {
            for itemref in spine_node
                .children()
                .filter(|n| n.is_element() && n.tag_name().name() == "itemref")
            {
                if let Some(idref) = itemref.attribute("idref") {
                    if let Some(manifest_item) = manifest.get(idref) {
                        let full_path = resolve_relative_path(opf_dir, &manifest_item.href);
                        spine_paths.push(full_path);
                    }
                }
            }
        }

        // Cover image extraction strategies:
        // Strategy A: EPUB 3 manifest item with property "cover-image"
        let mut cover_href = None;
        for item in &manifest_items_list {
            if let Some(ref props) = item.properties {
                if props.contains("cover-image") {
                    cover_href = Some(resolve_relative_path(opf_dir, &item.href));
                    break;
                }
            }
        }

        // Strategy B: EPUB 2 <meta name="cover" content="item_id" />
        if cover_href.is_none() {
            if let Some(ref cover_id) = epub2_meta_cover_id {
                if let Some(item) = manifest.get(cover_id) {
                    cover_href = Some(resolve_relative_path(opf_dir, &item.href));
                }
            }
        }

        // Strategy C: EPUB 2 <guide><reference type="cover" href="..." /></guide>
        if cover_href.is_none() {
            if let Some(guide_node) = doc.descendants().find(|n| n.tag_name().name() == "guide") {
                for ref_node in guide_node
                    .children()
                    .filter(|n| n.is_element() && n.tag_name().name() == "reference")
                {
                    if let (Some(ref_type), Some(href)) =
                        (ref_node.attribute("type"), ref_node.attribute("href"))
                    {
                        if ref_type.eq_ignore_ascii_case("cover") {
                            let resolved = resolve_relative_path(opf_dir, href);
                            // If the reference points to an image file or manifest item
                            let clean_path = resolved.split('#').next().unwrap_or(&resolved);
                            let mime = get_mime_type(clean_path);
                            if mime.starts_with("image/") {
                                cover_href = Some(clean_path.to_string());
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Strategy D: Fallback scan for any manifest item with "cover" in id or href and image media-type
        if cover_href.is_none() {
            for item in &manifest_items_list {
                let id_lower = item.id.to_lowercase();
                let href_lower = item.href.to_lowercase();
                let is_image = item.media_type.starts_with("image/")
                    || get_mime_type(&item.href).starts_with("image/");

                if is_image && (id_lower.contains("cover") || href_lower.contains("cover")) {
                    cover_href = Some(resolve_relative_path(opf_dir, &item.href));
                    break;
                }
            }
        }

        // Table of Contents (TOC) Detection:
        // Strategy A: EPUB 3 navigation document (<item properties="...nav...">)
        let mut toc_href = None;
        for item in &manifest_items_list {
            if let Some(ref props) = item.properties {
                if props.split_whitespace().any(|p| p == "nav") || props.contains("nav") {
                    toc_href = Some(resolve_relative_path(opf_dir, &item.href));
                    break;
                }
            }
        }

        // Strategy B: EPUB 2 NCX from <spine toc="ncx_id">
        if toc_href.is_none() {
            if let Some(spine_node) = doc.descendants().find(|n| n.tag_name().name() == "spine") {
                if let Some(toc_id) = spine_node.attribute("toc") {
                    if let Some(item) = manifest.get(toc_id) {
                        toc_href = Some(resolve_relative_path(opf_dir, &item.href));
                    }
                }
            }
        }

        // Strategy C: EPUB 2 NCX from manifest media-type
        if toc_href.is_none() {
            for item in &manifest_items_list {
                if item.media_type.eq_ignore_ascii_case("application/x-dtbncx+xml")
                    || item.href.ends_with(".ncx")
                {
                    toc_href = Some(resolve_relative_path(opf_dir, &item.href));
                    break;
                }
            }
        }

        let metadata = EpubMetadata {
            title: title.unwrap_or_else(|| "Unknown Title".to_string()),
            author: author.unwrap_or_else(|| "Unknown Author".to_string()),
            language: language.unwrap_or_else(|| "en".to_string()),
            description,
            identifier,
            publisher,
            date,
            rights,
            cover_href,
            toc_href,
        };

        Ok((metadata, manifest, spine_paths))
    }

    /// Extracts the Table of Contents from either EPUB 3 nav.xhtml or EPUB 2 toc.ncx.
    pub fn extract_toc(&mut self) -> Vec<RawTocItem> {
        let toc_path_opt = self.metadata.toc_href.clone();

        if let Some(ref toc_path) = toc_path_opt {
            if let Ok(toc_content) = self.read_string(toc_path) {
                let toc_dir = get_parent_dir(toc_path);

                if toc_path.ends_with(".ncx") || toc_content.contains("<ncx") {
                    let items = parse_toc_ncx(&toc_content, &toc_dir);
                    if !items.is_empty() {
                        return items;
                    }
                }

                let items = parse_nav_xhtml(&toc_content, &toc_dir);
                if !items.is_empty() {
                    return items;
                }
            }
        }

        // Fallback: scan manifest for any other nav or ncx documents
        let manifest_items: Vec<ManifestItem> = self.manifest.values().cloned().collect();
        for item in manifest_items {
            let full_path = resolve_relative_path(&self.opf_dir, &item.href);
            if full_path.ends_with(".ncx") || item.media_type == "application/x-dtbncx+xml" {
                if let Ok(ncx_content) = self.read_string(&full_path) {
                    let dir = get_parent_dir(&full_path);
                    let items = parse_toc_ncx(&ncx_content, &dir);
                    if !items.is_empty() {
                        return items;
                    }
                }
            }
        }

        Vec::new()
    }

    /// Reads an entry from the zip archive as raw bytes.
    pub fn read_bytes(&mut self, path: &str) -> Result<Vec<u8>> {
        let norm_path = normalize_zip_path(path);

        // Try exact match first
        if let Ok(mut entry) = self.archive.by_name(&norm_path) {
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut buf)?;
            return Ok(buf);
        }

        // Fallback: case-insensitive scan
        let mut matched_name = None;
        for i in 0..self.archive.len() {
            if let Ok(entry) = self.archive.by_index(i) {
                let name = entry.name().to_string();
                if normalize_zip_path(&name).eq_ignore_ascii_case(&norm_path) {
                    matched_name = Some(name);
                    break;
                }
            }
        }

        if let Some(name) = matched_name {
            let mut entry = self.archive.by_name(&name)?;
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut buf)?;
            return Ok(buf);
        }

        Err(anyhow!("File not found in EPUB archive: {}", path))
    }

    /// Reads an entry from the zip archive as a UTF-8 string.
    pub fn read_string(&mut self, path: &str) -> Result<String> {
        let bytes = self.read_bytes(path)?;
        String::from_utf8(bytes).map_err(|e| anyhow!("UTF-8 decode error in {}: {}", path, e))
    }

    /// Helper to read a string from an archive reference directly.
    fn read_archive_string(archive: &mut ZipArchive<File>, path: &str) -> Result<String> {
        let norm_path = normalize_zip_path(path);

        if let Ok(mut entry) = archive.by_name(&norm_path) {
            let mut buf = String::with_capacity(entry.size() as usize);
            entry.read_to_string(&mut buf)?;
            return Ok(buf);
        }

        let mut matched_name = None;
        for i in 0..archive.len() {
            if let Ok(entry) = archive.by_index(i) {
                let name = entry.name().to_string();
                if normalize_zip_path(&name).eq_ignore_ascii_case(&norm_path) {
                    matched_name = Some(name);
                    break;
                }
            }
        }

        if let Some(name) = matched_name {
            let mut entry = archive.by_name(&name)?;
            let mut buf = String::with_capacity(entry.size() as usize);
            entry.read_to_string(&mut buf)?;
            return Ok(buf);
        }

        Err(anyhow!("File not found in EPUB archive: {}", path))
    }
}

/// Parses an EPUB 3 navigation document (`nav.xhtml`) into hierarchical `RawTocItem`s.
pub fn parse_nav_xhtml(nav_xhtml: &str, nav_dir: &str) -> Vec<RawTocItem> {
    let document = scraper::Html::parse_document(nav_xhtml);

    // Look for <nav epub:type="toc">, <nav id="toc">, or any <nav>
    let nav_selector = scraper::Selector::parse("nav[epub\\:type='toc'], nav#toc, nav").unwrap();
    let nav_elem = match document.select(&nav_selector).next() {
        Some(n) => n,
        None => return Vec::new(),
    };

    let list_selector = scraper::Selector::parse("ol, ul").unwrap();
    let root_list = match nav_elem.select(&list_selector).next() {
        Some(l) => l,
        None => return Vec::new(),
    };

    parse_html_toc_list(root_list, nav_dir)
}

fn parse_html_toc_list(list_elem: scraper::ElementRef, nav_dir: &str) -> Vec<RawTocItem> {
    let mut items = Vec::new();
    let a_selector = scraper::Selector::parse("a").unwrap();

    for child in list_elem.children() {
        if let Some(li) = scraper::ElementRef::wrap(child) {
            if !li.value().name().eq_ignore_ascii_case("li") {
                continue;
            }

            let mut href = String::new();

            let title = if let Some(a) = li.select(&a_selector).next() {
                if let Some(h) = a.value().attr("href") {
                    href = resolve_relative_path(nav_dir, h);
                }
                a.text().collect::<Vec<_>>().join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
            } else {
                // If there's no <a>, extract direct text from li
                li.text().collect::<Vec<_>>().join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
            };

            // Only recurse into `ol`/`ul` that are direct children of this `li`.
            // `li.select(...)` matches *any* descendant, not just direct children,
            // so on a TOC nested three-plus levels deep (e.g. Part > Book >
            // Chapter) it would also pick up each grandchild sublist and parse it
            // a second time as if it were a flat, direct child of this `li` too —
            // duplicating every chapter into the TOC tree at each ancestor level.
            let mut children = Vec::new();
            for child in li.children() {
                if let Some(sublist) = scraper::ElementRef::wrap(child) {
                    let name = sublist.value().name();
                    if name.eq_ignore_ascii_case("ol") || name.eq_ignore_ascii_case("ul") {
                        children.extend(parse_html_toc_list(sublist, nav_dir));
                    }
                }
            }

            if !title.is_empty() || !href.is_empty() {
                items.push(RawTocItem {
                    title,
                    href,
                    children,
                });
            }
        }
    }

    items
}

/// Parses an EPUB 2 NCX document (`toc.ncx`) into hierarchical `RawTocItem`s.
pub fn parse_toc_ncx(ncx_xml: &str, ncx_dir: &str) -> Vec<RawTocItem> {
    let doc = match Document::parse(ncx_xml) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };

    let nav_map = match doc.descendants().find(|n| n.tag_name().name() == "navMap") {
        Some(m) => m,
        None => return Vec::new(),
    };

    parse_ncx_nav_points(nav_map, ncx_dir)
}

fn parse_ncx_nav_points(parent_node: roxmltree::Node, ncx_dir: &str) -> Vec<RawTocItem> {
    let mut items = Vec::new();

    for child in parent_node
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "navPoint")
    {
        let mut title = String::new();
        let mut href = String::new();

        if let Some(label_node) = child
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "navLabel")
        {
            if let Some(text_node) = label_node
                .children()
                .find(|n| n.is_element() && n.tag_name().name() == "text")
            {
                if let Some(t) = text_node.text() {
                    title = t.trim().to_string();
                }
            }
        }

        if let Some(content_node) = child
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "content")
        {
            if let Some(src) = content_node.attribute("src") {
                href = resolve_relative_path(ncx_dir, src);
            }
        }

        let children = parse_ncx_nav_points(child, ncx_dir);

        if !title.is_empty() || !href.is_empty() {
            items.push(RawTocItem {
                title,
                href,
                children,
            });
        }
    }

    items
}

