use std::fs::File;
use std::io::Write;
use std::path::Path;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

pub const COVER_PNG_BYTES: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x01, 0x02, 0x03];
pub const DIAGRAM_PNG_BYTES: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0xAA, 0xBB, 0xCC, 0xDD];

/// Helper to construct a fully valid EPUB 3 archive for testing.
pub fn create_test_epub(file_path: &Path) {
    let file = File::create(file_path).expect("Failed to create test epub file");
    let mut zip = ZipWriter::new(file);

    let stored_opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let deflated_opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    // 1. mimetype
    zip.start_file("mimetype", stored_opts).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();

    // 2. META-INF/container.xml
    zip.start_file("META-INF/container.xml", deflated_opts).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/package.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
    )
    .unwrap();

    // 3. OEBPS/package.opf
    zip.start_file("OEBPS/package.opf", deflated_opts).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="pub-id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="pub-id">urn:uuid:12345678-1234-5678-1234-567812345678</dc:identifier>
    <dc:title>The Principles of Weland</dc:title>
    <dc:creator>Jane Doe</dc:creator>
    <dc:language>en</dc:language>
    <dc:description>A comprehensive guide to next-generation ebook architectures.</dc:description>
    <dc:publisher>Weland Press</dc:publisher>
    <dc:date>2026-08-13</dc:date>
    <meta name="cover" content="cover-image-id"/>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="cover-image-id" href="images/cover.png" media-type="image/png" properties="cover-image"/>
    <item id="chapter1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="chapter2" href="text/ch2.xhtml" media-type="application/xhtml+xml"/>
    <item id="img-diagram" href="images/diagram.png" media-type="image/png"/>
  </manifest>
  <spine>
    <itemref idref="chapter1"/>
    <itemref idref="chapter2"/>
  </spine>
</package>"#,
    )
    .unwrap();

    // 4. Dummy cover image
    zip.start_file("OEBPS/images/cover.png", deflated_opts).unwrap();
    zip.write_all(COVER_PNG_BYTES).unwrap();

    // 5. Dummy diagram image
    zip.start_file("OEBPS/images/diagram.png", deflated_opts).unwrap();
    zip.write_all(DIAGRAM_PNG_BYTES).unwrap();

    // 6. Navigation Document (nav.xhtml)
    zip.start_file("OEBPS/nav.xhtml", deflated_opts).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>TOC</title></head>
<body>
  <nav epub:type="toc" id="toc">
    <h1>Table of Contents</h1>
    <ol>
      <li><a href="text/ch1.xhtml">1. Introduction to Weland</a></li>
      <li><a href="text/ch2.xhtml">2. Deep Dive: Performance</a></li>
    </ol>
  </nav>
</body>
</html>"#,
    )
    .unwrap();

    // 7. Chapter 1 XHTML
    zip.start_file("OEBPS/text/ch1.xhtml", deflated_opts).unwrap();
    zip.write_all(
        br##"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Chapter 1</title></head>
<body>
  <h1>Introduction to Weland</h1>
  <p>The <em>future</em> of <strong>digital publishing</strong> is here with <a href="https://example.com">open standards</a>.</p>
  <hr/>
  <p>Here is a second paragraph referencing a footnote<a href="#fn1" class="noteref">1</a> for details.</p>
  <blockquote>Knowledge is power in the digital age.</blockquote>
  <ul>
    <li>Fast random access</li>
    <li>Full-text search</li>
  </ul>
  <table>
    <tr><th>Feature</th><th>Status</th></tr>
    <tr><td>AST Storage</td><td>Active</td></tr>
    <tr><td>FTS5 Index</td><td>Enabled</td></tr>
  </table>
  <aside id="fn1">
    <p>1. Weland is an SQLite-backed binary standard.</p>
  </aside>
</body>
</html>"##,
    )
    .unwrap();

    // 8. Chapter 2 XHTML
    zip.start_file("OEBPS/text/ch2.xhtml", deflated_opts).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Chapter 2</title></head>
<body>
  <h2>Deep Dive: Performance</h2>
  <p>Look at this architectural diagram:</p>
  <img src="../images/diagram.png" alt="Architecture Diagram" title="Figure 1.1"/>
  <p>Inline code like <code>SELECT * FROM ast_nodes;</code> is fast.</p>
</body>
</html>"#,
    )
    .unwrap();

    zip.finish().unwrap();
}
