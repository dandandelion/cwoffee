//! Document output: markdown in, .docx / .html / .pdf out.
//!
//! Why hand-rolled OOXML instead of a docx crate: the obvious crate pulled 88
//! transitive crates (zstd, xz, zopfli, a JPEG decoder) into an app whose whole
//! argument is being light. A .docx is a zip of five XML parts, and writing them
//! directly also means *we* control the escaping. Model output is untrusted - it
//! quotes fetched web pages - and a document writer that passes text through raw
//! is an injection path into a file the user opens in Word.
//!
//! PDF is delegated to LibreOffice when it is installed. Writing a PDF renderer
//! from scratch is a font-metrics project, and pretending otherwise would mean
//! shipping a "PDF" that is really a bad text dump. When LibreOffice is absent
//! the tool says so and still writes the .docx/.html rather than failing the
//! whole request.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use zip::write::FileOptions;
use zip::CompressionMethod;

// ---------------------------------------------------------------------------
// Escaping
// ---------------------------------------------------------------------------

/// Escape for XML/HTML text content. Every string that came from the model goes
/// through this before it reaches a document.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Only these schemes survive into generated links. `[x](javascript:...)`
/// appears in real scraped content, and both Word and a browser will act on it.
fn safe_url(u: &str) -> Option<&str> {
    let t = u.trim();
    if t.is_empty() || t.contains('<') || t.contains('"') || t.contains('\'') {
        return None;
    }
    if t.starts_with('#') || t.starts_with('/') {
        return Some(t);
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
    {
        Some(t)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// HTML
// ---------------------------------------------------------------------------

pub fn markdown_to_html(md: &str) -> String {
    let parser = Parser::new_ext(md, Options::empty());
    let mut body = String::new();
    // Link labels are buffered, then emitted as one element at End(Link). That
    // way an unsafe target drops the anchor but keeps the text, and nothing is
    // half-written into the document.
    let mut link: Option<(String, Option<String>)> = None;

    for ev in parser {
        match ev {
            Event::Start(Tag::Paragraph) => body.push_str("<p>"),
            Event::End(TagEnd::Paragraph) => body.push_str("</p>\n"),
            Event::Start(Tag::Heading { level, .. }) => {
                body.push_str(&format!("<h{}>", level as u8))
            }
            Event::End(TagEnd::Heading(level)) => {
                body.push_str(&format!("</h{}>\n", level as u8))
            }
            Event::Start(Tag::Emphasis) => body.push_str("<em>"),
            Event::End(TagEnd::Emphasis) => body.push_str("</em>"),
            Event::Start(Tag::Strong) => body.push_str("<strong>"),
            Event::End(TagEnd::Strong) => body.push_str("</strong>"),
            Event::Start(Tag::CodeBlock(_)) => body.push_str("<pre><code>"),
            Event::End(TagEnd::CodeBlock) => body.push_str("</code></pre>\n"),
            Event::Start(Tag::BlockQuote(_)) => body.push_str("<blockquote>"),
            Event::End(TagEnd::BlockQuote(_)) => body.push_str("</blockquote>\n"),
            Event::Start(Tag::List(ordered)) => {
                body.push_str(if ordered.is_some() { "<ol>\n" } else { "<ul>\n" })
            }
            Event::End(TagEnd::List(ordered)) => {
                body.push_str(if ordered { "</ol>\n" } else { "</ul>\n" })
            }
            Event::Start(Tag::Item) => body.push_str("<li>"),
            Event::End(TagEnd::Item) => body.push_str("</li>\n"),
            Event::Start(Tag::Link { dest_url, .. }) => {
                link = Some((String::new(), safe_url(&dest_url).map(str::to_string)))
            }
            Event::End(TagEnd::Link) => {
                if let Some((label, href)) = link.take() {
                    match href {
                        // The URL is written out as visible text. LibreOffice's
                        // web-PDF export ignores CSS generated content, so a
                        // print-only ::after rule silently drops every link from
                        // the PDF. This HTML is a document, not an app screen.
                        Some(h) => body.push_str(&format!(
                            "<a href=\"{}\" rel=\"noopener\">{label}</a>\
                             <span class=\"url\">({})</span>",
                            xml_escape(&h),
                            xml_escape(&h)
                        )),
                        // Unsafe target: keep the words, lose the hyperlink.
                        None => body.push_str(&label),
                    }
                }
            }
            Event::Text(t) | Event::Code(t) => {
                let esc = xml_escape(&t);
                match &mut link {
                    Some((label, _)) => label.push_str(&esc),
                    None => body.push_str(&esc),
                }
            }
            Event::Html(t) | Event::InlineHtml(t) => {
                let esc = xml_escape(&t);
                match &mut link {
                    Some((label, _)) => label.push_str(&esc),
                    None => body.push_str(&esc),
                }
            }
            Event::SoftBreak => body.push('\n'),
            Event::HardBreak => body.push_str("<br>\n"),
            Event::Rule => body.push_str("<hr>\n"),
            _ => {}
        }
    }

    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n\
         <style>body{{font:15px/1.6 system-ui,sans-serif;max-width:46em;margin:3em auto;padding:0 1.5em;color:#1c1c1c}}\
         pre{{background:#f5f5f4;padding:12px;border-radius:8px;overflow:auto}}\
         code{{font-family:ui-monospace,monospace}}\
         blockquote{{border-left:3px solid #ddd;margin:0;padding-left:14px;color:#555}}\
         a{{color:#1f4e79;text-decoration:none}}\
         .url{{font-size:0.85em;color:#666}}</style>\n\
         </head>\n<body>\n{body}</body>\n</html>\n"
    )
}

// ---------------------------------------------------------------------------
// DOCX
// ---------------------------------------------------------------------------

const CT: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
    r#"<Types xmlns="http://schemas.openxmlformats.org/officeDocument/2006/content-types">"#,
    r#"<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>"#,
    r#"<Default Extension="xml" ContentType="application/xml"/>"#,
    r#"<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>"#,
    r#"<Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>"#,
    r#"</Types>"#
);

const ROOT_RELS: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>"#,
    r#"</Relationships>"#
);

const DOC_RELS: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#,
    r#"</Relationships>"#
);

fn heading_style(id: &str, name: &str, size: &str, color: &str) -> String {
    format!(
        concat!(
            r#"<w:style w:type="paragraph" w:styleId="{id}"><w:name w:val="{name}"/>"#,
            r#"<w:basedOn w:val="Normal"/><w:next w:val="Normal"/>"#,
            r#"<w:pPr><w:keepNext/><w:spacing w:before="320" w:after="120"/></w:pPr>"#,
            r#"<w:rPr><w:b/><w:color w:val="{color}"/><w:sz w:val="{size}"/></w:rPr></w:style>"#
        ),
        id = id,
        name = name,
        color = color,
        size = size
    )
}

fn styles_xml() -> String {
    let mut s = String::from(concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        r#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
        r#"<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/>"#,
        r#"<w:pPr><w:spacing w:after="160"/></w:pPr><w:rPr><w:sz w:val="22"/></w:rPr></w:style>"#,
    ));
    s.push_str(&heading_style("Heading1", "heading 1", "36", "2E5E8C"));
    s.push_str(&heading_style("Heading2", "heading 2", "30", "2E5E8C"));
    s.push_str(&heading_style("Heading3", "heading 3", "26", "3C3C3C"));
    s.push_str(concat!(
        r#"<w:style w:type="paragraph" w:styleId="Code"><w:name w:val="Code"/>"#,
        r#"<w:pPr><w:spacing w:after="0"/><w:shd w:val="clear" w:fill="F4F4F2"/></w:pPr>"#,
        r#"<w:rPr><w:rFonts w:ascii="Consolas" w:hAnsi="Consolas"/><w:sz w:val="20"/></w:rPr></w:style>"#,
        r#"<w:style w:type="paragraph" w:styleId="Quote"><w:name w:val="Quote"/>"#,
        r#"<w:pPr><w:ind w:left="360"/></w:pPr><w:rPr><w:i/></w:rPr></w:style>"#,
        r#"<w:style w:type="paragraph" w:styleId="ListItem"><w:name w:val="List Item"/>"#,
        r#"<w:pPr><w:ind w:left="360"/></w:pPr></w:style>"#,
        r#"</w:styles>"#,
    ));
    s
}

#[derive(Default, Clone, Copy)]
struct RunStyle {
    bold: bool,
    italic: bool,
    mono: bool,
}

fn run(text: &str, st: RunStyle) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut rpr = String::new();
    if st.bold {
        rpr.push_str("<w:b/>");
    }
    if st.italic {
        rpr.push_str("<w:i/>");
    }
    if st.mono {
        rpr.push_str(r#"<w:rFonts w:ascii="Consolas" w:hAnsi="Consolas"/>"#);
    }
    let rpr = if rpr.is_empty() {
        String::new()
    } else {
        format!("<w:rPr>{rpr}</w:rPr>")
    };
    format!(
        "<w:r>{rpr}<w:t xml:space=\"preserve\">{}</w:t></w:r>",
        xml_escape(text)
    )
}

fn para(style: Option<&str>, runs: &[String]) -> String {
    let ppr = match style {
        Some(s) => format!(r#"<w:pPr><w:pStyle w:val="{s}"/></w:pPr>"#),
        None => String::new(),
    };
    format!("<w:p>{ppr}{}</w:p>", runs.concat())
}

/// Accumulates one paragraph at a time so the borrow checker stays out of the
/// way of the event loop.
struct DocxBuilder {
    body: String,
    runs: Vec<String>,
    style: Option<String>,
    st: RunStyle,
    in_code: bool,
    list_depth: usize,
    /// Target of the link currently being read, if it passed the scheme check.
    pending_link: Option<String>,
}

impl DocxBuilder {
    fn flush(&mut self) {
        if !self.runs.is_empty() {
            let runs = std::mem::take(&mut self.runs);
            let style = self.style.take();
            self.body.push_str(&para(style.as_deref(), &runs));
        } else {
            self.style = None;
        }
    }

    fn finish(mut self) -> String {
        self.flush();
        format!(
            concat!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
                r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
                r#"<w:body>{body}"#,
                r#"<w:sectPr><w:pgSz w:w="11906" w:h="16838"/>"#,
                r#"<w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440"/></w:sectPr>"#,
                r#"</w:body></w:document>"#
            ),
            body = self.body
        )
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                self.style = Some(match level {
                    HeadingLevel::H1 => "Heading1",
                    HeadingLevel::H2 => "Heading2",
                    HeadingLevel::H3 | HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => {
                        "Heading3"
                    }
                }
                .to_string());
            }
            Event::End(TagEnd::Heading(_)) => self.flush(),
            Event::End(TagEnd::Paragraph) => self.flush(),
            Event::Start(Tag::Emphasis) => self.st.italic = true,
            Event::End(TagEnd::Emphasis) => self.st.italic = false,
            Event::Start(Tag::Strong) => self.st.bold = true,
            Event::End(TagEnd::Strong) => self.st.bold = false,
            Event::Start(Tag::CodeBlock(_)) => {
                self.flush();
                self.in_code = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                self.in_code = false;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                self.flush();
                self.style = Some("Quote".into());
            }
            Event::End(TagEnd::BlockQuote(_)) => self.flush(),
            Event::Start(Tag::List(_)) => self.list_depth += 1,
            Event::End(TagEnd::List(_)) => self.list_depth = self.list_depth.saturating_sub(1),
            Event::Start(Tag::Item) => {
                self.style = Some("ListItem".into());
                // A real numbering.xml is a lot of ceremony for what it adds
                // here; a literal bullet renders identically in Word and
                // LibreOffice.
                let bullet = if self.list_depth > 1 { "  • " } else { "• " };
                self.runs.push(run(bullet, RunStyle::default()));
            }
            Event::End(TagEnd::Item) => self.flush(),
            Event::Text(t) => {
                if self.in_code {
                    // One paragraph per line: a code block emitted as a single
                    // run collapses into one wrapped blob.
                    for line in t.split('\n') {
                        if line.is_empty() {
                            continue;
                        }
                        self.body.push_str(&para(
                            Some("Code"),
                            &[run(line, RunStyle { mono: true, ..Default::default() })],
                        ));
                    }
                } else {
                    self.runs.push(run(&t, self.st));
                }
            }
            Event::Code(t) => self
                .runs
                .push(run(&t, RunStyle { mono: true, ..self.st })),
            Event::Html(t) | Event::InlineHtml(t) => {
                // Raw HTML from the model is never interpreted. It is kept as
                // literal text rather than dropped: silently discarding content
                // the model wrote is a worse failure than showing it verbatim.
                self.runs.push(run(&t, self.st));
            }
            Event::SoftBreak | Event::HardBreak => self.runs.push("<w:br/>".into()),
            Event::Rule => self.body.push_str(
                r#"<w:p><w:pPr><w:pBdr><w:bottom w:val="single" w:sz="6" w:space="1" w:color="CCCCCC"/></w:pBdr></w:pPr></w:p>"#,
            ),
            // Links keep their target as visible text. A printed document that
            // silently loses a URL has destroyed information.
            Event::Start(Tag::Link { dest_url, .. }) => {
                if let Some(u) = safe_url(&dest_url) {
                    self.st.italic = true;
                    self.pending_link = Some(u.to_string());
                }
            }
            Event::End(TagEnd::Link) => {
                if let Some(u) = self.pending_link.take() {
                    self.runs.push(run(&format!(" ({u})"), RunStyle { italic: true, ..Default::default() }));
                    self.st.italic = false;
                }
            }
            _ => {}
        }
    }
}

/// Convert markdown to a .docx byte stream.
pub fn markdown_to_docx(md: &str) -> Result<Vec<u8>, String> {
    let parser = Parser::new_ext(md, Options::empty());
    let mut b = DocxBuilder {
        body: String::new(),
        runs: Vec::new(),
        style: None,
        st: RunStyle::default(),
        in_code: false,
        list_depth: 0,
        pending_link: None,
    };
    for ev in parser {
        b.event(ev);
    }
    let document = b.finish();

    let buf = Cursor::new(Vec::new());
    let mut w = zip::ZipWriter::new(buf);
    let opts: FileOptions<'_, ()> = FileOptions::default().compression_method(CompressionMethod::Deflated);

    // `[Content_Types].xml` first, with that exact name: Word reports
    // "unreadable content" if the content-types part is missing or renamed.
    let parts: [(&str, &str); 4] = [
        ("[Content_Types].xml", CT),
        ("_rels/.rels", ROOT_RELS),
        ("word/_rels/document.xml.rels", DOC_RELS),
        ("word/styles.xml", &styles_xml()),
    ];
    for (name, content) in parts {
        w.start_file(name, opts)
            .map_err(|e| format!("docx zip: {e}"))?;
        w.write_all(content.as_bytes())
            .map_err(|e| format!("docx zip: {e}"))?;
    }

    w.start_file("word/document.xml", opts)
        .map_err(|e| format!("docx zip: {e}"))?;
    w.write_all(document.as_bytes())
        .map_err(|e| format!("docx zip: {e}"))?;

    let cur = w.finish().map_err(|e| format!("docx zip: {e}"))?;
    Ok(cur.into_inner())
}

// ---------------------------------------------------------------------------
// PDF via LibreOffice
// ---------------------------------------------------------------------------

/// LibreOffice is optional. Detection is a PATH lookup rather than a hardcoded
/// path so flatpak/Nix installs behave the same.
pub fn converter() -> Option<PathBuf> {
    ["libreoffice", "soffice"]
        .iter()
        .find_map(|n| which(n))
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|c| c.is_file())
}

/// Render markdown straight to a PDF at `out`.
///
/// The path is markdown -> HTML -> LibreOffice -> PDF, not markdown -> docx ->
/// PDF. Measured on this box: LibreOffice ships without the MS Word 2007 XML
/// filter at all (`--convert-to docx` reports "no export filter found"), so a
/// docx intermediate cannot be read back by the same tool that is supposed to
/// render it. The HTML path uses `writer_web_pdf_Export`, which is present.
///
/// The .docx is still written as a deliverable for Word users; it is simply not
/// on the PDF rendering path.
pub async fn markdown_to_pdf(md: &str, out: &Path, timeout_secs: u64) -> Result<PathBuf, String> {
    let stem = out
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("document");
    let dir = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    // LibreOffice names the output after the input, so the scratch HTML has to
    // carry the wanted stem.
    let scratch = dir.join(format!("{stem}.cowry-tmp.html"));
    std::fs::write(&scratch, markdown_to_html(md))
        .map_err(|e| format!("cannot write scratch html: {e}"))?;

    let produced = to_pdf(&scratch, timeout_secs).await;
    let _ = std::fs::remove_file(&scratch);

    let produced = produced?;
    if produced != out {
        std::fs::rename(&produced, out)
            .map_err(|e| format!("cannot move pdf into place: {e}"))?;
    }
    Ok(out.to_path_buf())
}

/// Convert an existing file LibreOffice can read into a PDF next to it.
///
/// `UserInstallation` points at a throwaway profile: without it LibreOffice
/// locks the user's real profile, and the next time they open Writer they get a
/// "recover your documents" dialog because an agent ran a headless conversion.
pub async fn to_pdf(src: &Path, timeout_secs: u64) -> Result<PathBuf, String> {
    let bin = converter().ok_or_else(|| {
        "no PDF converter on PATH (looked for libreoffice, soffice). The .docx/.html was \
         written and can be converted later."
            .to_string()
    })?;

    let dir = src
        .parent()
        .ok_or_else(|| "document has no parent directory".to_string())?;
    let profile = std::env::temp_dir().join(format!("cowry_lo_profile_{}", std::process::id()));

    let mut cmd = tokio::process::Command::new(&bin);
    // `-env:` must come before the action arguments or LibreOffice ignores it.
    cmd.arg(format!("-env:UserInstallation=file://{}", profile.display()))
        .arg("--headless")
        .args(["--convert-to", "pdf", "--outdir"])
        .arg(dir)
        .arg(src)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let out = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), cmd.output())
        .await
        .map_err(|_| format!("PDF conversion timed out after {timeout_secs}s"))?
        .map_err(|e| format!("cannot run {}: {e}", bin.display()))?;

    let _ = std::fs::remove_dir_all(&profile);

    if !out.status.success() {
        let err: String = String::from_utf8_lossy(&out.stderr)
            .trim()
            .chars()
            .take(300)
            .collect();
        return Err(format!("converter failed: {err}"));
    }

    let pdf = src.with_extension("pdf");
    if !pdf.exists() {
        return Err("converter reported success but produced no .pdf".into());
    }
    Ok(pdf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_xml(md: &str) -> String {
        let bytes = markdown_to_docx(md).expect("docx built");
        assert_eq!(&bytes[0..2], b"PK", "not a zip");
        let cur = Cursor::new(bytes);
        let mut zip = zip::ZipArchive::new(cur).expect("zip opens");
        let mut doc = zip.by_name("word/document.xml").expect("document part");
        let mut xml = String::new();
        std::io::Read::read_to_string(&mut doc, &mut xml).unwrap();
        xml
    }

    #[test]
    fn docx_contains_the_required_parts() {
        let bytes = markdown_to_docx("# T\n\nx").unwrap();
        let cur = Cursor::new(bytes);
        let zip = zip::ZipArchive::new(cur).unwrap();
        let names: Vec<String> = zip.file_names().map(|s| s.to_string()).collect();
        for need in [
            "[Content_Types].xml",
            "_rels/.rels",
            "word/_rels/document.xml.rels",
            "word/styles.xml",
            "word/document.xml",
        ] {
            assert!(names.iter().any(|n| n == need), "missing {need}: {names:?}");
        }
    }

    #[test]
    fn formatting_and_lists_render() {
        let xml = doc_xml("# Title\n\nSome **bold** and *italic* text.\n\n- one\n- two\n");
        assert!(xml.contains(">Title</w:t>"), "heading missing: {xml}");
        assert!(xml.contains(r#"w:val="Heading1""#), "heading style missing");
        assert!(xml.contains("<w:b/>"), "bold missing");
        assert!(xml.contains("<w:i/>"), "italic missing");
        assert!(xml.contains(r#"w:val="ListItem""#), "list style missing: {xml}");
        assert!(xml.contains(">• <"), "bullet run missing: {xml}");
        assert!(xml.contains(">one<"), "item text missing: {xml}");
        assert_balanced(&xml);
    }

    #[test]
    fn model_text_cannot_inject_xml_into_the_document() {
        let evil = "</w:t></w:r><w:r><w:t>injected</w:t>\n\n<script>alert(1)</script>\n\n<b>x</b>";
        let xml = doc_xml(evil);
        // pulldown emits `<` and `script>` as separate text events, so the
        // escaped form is split across runs. What matters is that no literal
        // markup survives and every angle bracket is an entity.
        assert!(!xml.contains("<script"), "raw script reached the document");
        assert!(!xml.contains("alert(1)<"), "unescaped payload");
        assert!(xml.contains("&lt;"), "angle bracket not escaped: {xml}");
        assert!(xml.contains("script&gt;"), "should be escaped: {xml}");
        assert_balanced(&xml);
    }

    #[test]
    fn ampersands_and_quotes_survive_without_breaking_the_part() {
        let xml = doc_xml("Tom & Jerry's \"quote\" <tag>");
        assert!(xml.contains("Tom &amp; Jerry&apos;s"), "got {xml}");
        assert_balanced(&xml);
    }

    #[test]
    fn code_blocks_keep_their_line_structure() {
        let xml = doc_xml("```\nfn a() {}\nfn b() {}\n```");
        let code_paras = xml.matches(r#"w:val="Code""#).count();
        assert!(code_paras >= 2, "one paragraph per line: {xml}");
        assert_balanced(&xml);
    }

    #[test]
    fn static_parts_are_balanced() {
        assert_balanced(&styles_xml());
        assert_balanced(CT);
        assert_balanced(ROOT_RELS);
        assert_balanced(DOC_RELS);
    }

    #[test]
    fn html_escapes_and_drops_unsafe_links() {
        let h = markdown_to_html(
            "<script>alert(1)</script>\n\n[click](javascript:alert(1))\n\n[safe](https://x.dev/a)",
        );
        assert!(!h.contains("<script>"), "script passed through: {h}");
        assert!(!h.contains("javascript:"), "unsafe scheme kept: {h}");
        assert!(h.contains("click"), "link text should survive");
        assert!(h.contains("href=\"https://x.dev/a\""), "safe link dropped: {h}");
    }

    #[test]
    fn docx_link_keeps_the_url_as_text() {
        let xml = doc_xml("see [the docs](https://example.com/d)");
        assert!(xml.contains("example.com"), "url lost: {xml}");
        let bad = doc_xml("see [x](javascript:alert(1))");
        assert!(!bad.contains("javascript:"), "unsafe url kept: {bad}");
    }

    #[test]
    fn degenerate_documents_still_produce_a_valid_package() {
        for md in ["", "# only a heading", "\n\n\n", "- a", "```", "**unclosed", "[]()", "```rust"] {
            let xml = doc_xml(md);
            assert_balanced(&xml);
        }
    }

    /// Balanced-tag sanity check. Word's "unreadable content" error is what a
    /// missing escape produces, and that is far easier to see here than in a
    /// screenshot of Word.
    fn assert_balanced(xml: &str) {
        let mut depth = 0i32;
        let mut i = 0;
        let b = xml.as_bytes();
        while i < b.len() {
            if b[i] == b'<' {
                // Declarations and comments are not elements.
                if xml[i..].starts_with("<?") || xml[i..].starts_with("<!") {
                    let end = xml[i..].find('>').unwrap_or(0);
                    i += end + 1;
                    continue;
                }
                let end = match xml[i..].find('>') {
                    Some(e) => e,
                    None => panic!("unterminated tag at {i} in {xml}"),
                };
                let tag = &xml[i..=i + end];
                if tag.starts_with("</") {
                    depth -= 1;
                } else if !tag.ends_with("/>") {
                    depth += 1;
                }
                assert!(depth >= 0, "unbalanced close near {i}: {xml}");
                i += end + 1;
            } else {
                i += 1;
            }
        }
        assert_eq!(depth, 0, "unbalanced document xml: {xml}");
    }
}
