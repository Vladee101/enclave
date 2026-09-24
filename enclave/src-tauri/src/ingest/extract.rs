//! Text extraction for ingestion (ADR-0019): plain text / Markdown, PDF with a
//! text layer, and DOCX. Pure Rust — no native libraries to ship with the
//! desktop app.
//!
//! The format is decided from the bytes, not from the browser-reported MIME
//! type (often empty or wrong for `.md`), with the file name as a tiebreaker
//! for plain text. Anything else is refused up front by `check_supported` at
//! upload time, and again here, so a job never "succeeds" with nothing in it.

use std::io::{Cursor, Read};

use anyhow::{bail, Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

/// Extensions accepted at upload. Checked by name before anything is stored,
/// so an unsupported file is refused immediately instead of failing later in
/// the worker.
pub const SUPPORTED_EXTENSIONS: &[&str] = &["pdf", "docx", "txt", "md", "markdown"];

/// DOCX main part larger than this is refused rather than inflated: a
/// deliberately compressed archive could otherwise expand without bound.
const MAX_DOCX_XML_BYTES: u64 = 64 * 1024 * 1024;

pub fn check_supported(filename: &str) -> Result<()> {
    let ext = extension(filename);
    if SUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
        return Ok(());
    }
    bail!(
        "Unsupported file type{}. Supported: PDF, DOCX, TXT, MD.",
        if ext.is_empty() { String::new() } else { format!(" .{ext}") }
    )
}

/// Extract the text of a document. Errors are permanent (retrying will not
/// help), and an empty result is an error: a document with no text would be
/// "ready" and never found.
pub fn extract_text(bytes: &[u8], filename: &str) -> Result<String> {
    let text = if bytes.starts_with(b"%PDF-") {
        extract_pdf(bytes)?
    } else if bytes.starts_with(b"PK\x03\x04") {
        extract_docx(bytes)?
    } else if matches!(extension(filename).as_str(), "txt" | "md" | "markdown") {
        decode_text(bytes).with_context(|| format!("{filename} is not a text file"))?
    } else {
        bail!("Unsupported file type for {filename}. Supported: PDF, DOCX, TXT, MD.");
    };

    let text = normalize(&text);
    if text.trim().is_empty() {
        if bytes.starts_with(b"%PDF-") {
            bail!(
                "{filename} has no text layer — it looks like a scanned PDF. \
                 Enclave does not do OCR yet; export the document with text or upload a text version."
            );
        }
        bail!("{filename} contains no text");
    }
    Ok(text)
}

fn extension(filename: &str) -> String {
    filename
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Plain text: UTF-8 (BOM or not), or UTF-16 with a BOM — what Windows
/// Notepad writes as "Unicode", where every other byte is NUL. Otherwise a
/// NUL byte means binary data (git's heuristic) and the file is refused.
fn decode_text(bytes: &[u8]) -> Result<String> {
    let utf16 = |rest: &[u8], le: bool| {
        let units = rest.chunks_exact(2).map(|p| if le { u16::from_le_bytes([p[0], p[1]]) } else { u16::from_be_bytes([p[0], p[1]]) });
        char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect::<String>()
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => Ok(utf16(rest, true)),
        [0xFE, 0xFF, rest @ ..] => Ok(utf16(rest, false)),
        [0xEF, 0xBB, 0xBF, rest @ ..] => Ok(String::from_utf8_lossy(rest).into_owned()),
        _ if bytes.contains(&0) => bail!("it contains binary data"),
        _ => Ok(String::from_utf8_lossy(bytes).into_owned()),
    }
}

/// PDF text layer via pdf-extract's parser, with our own text assembly
/// (`TextOutput`). The crate is known to panic on some malformed files; a
/// panic is turned into an ordinary error so one bad document fails its own
/// job instead of taking the worker down.
fn extract_pdf(bytes: &[u8]) -> Result<String> {
    let owned = bytes.to_vec();
    let run = move || -> std::result::Result<String, pdf_extract::OutputError> {
        let doc = pdf_extract::Document::load_mem(&owned)?;
        let mut out = TextOutput::default();
        pdf_extract::output_doc(&doc, &mut out)?;
        Ok(out.finish())
    };
    match std::panic::catch_unwind(run) {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => bail!("could not read PDF: {e}"),
        Err(_) => bail!("could not read PDF: the parser crashed on this file (malformed or unsupported PDF)"),
    }
}

/// A gap between two glyphs wider than this fraction of the font size is a
/// candidate word break. Real spaces in narrow fonts are ~0.22 em (Calibri),
/// kerning is hundredths of an em.
const WORD_GAP_EM: f64 = 0.15;

/// A gap this wide is a column break (table cells, tab stops), kept as a tab
/// whatever the document's spacing.
///
/// Measured on the Chromium fixture: bogus in-word gaps 0.22–1.02 em, column
/// breaks 2.8–3.3 em — and one at 1.06 em, where the widest cell fills its
/// column up to the padding. That one overlaps the bogus gaps, so geometry
/// cannot tell it apart; tuning the threshold to 1.05 would fit this one
/// file. Known limitation: such a cell runs into the next ("более3"). The
/// words themselves stay intact, which is what search depends on.
const COLUMN_GAP_EM: f64 = 2.0;

/// Placeholder for a word break inferred from geometry, resolved once the
/// whole document has been seen (`TextOutput::finish`). Private-use code
/// point: never produced by a real text layer.
const GAP_MARK: char = '\u{E000}';

/// If real space glyphs make up at least this share of a document's
/// characters, word breaks come from them alone. Typical prose is 12–18 %
/// spaces; a document positioning words without space glyphs has ~0 %.
const REAL_SPACE_SHARE: f64 = 0.05;

/// Assembles a PDF's glyph stream into text: pdf-extract's line-break logic,
/// a blank line between pages, and word breaks decided per document.
///
/// Why not just geometry, like pdf-extract's own output: glyph positions it
/// computes are wrong for some fonts. On a Chromium-made PDF the gap measured
/// inside words ("к|алендарных", "t|o", "da|ys") was ~1 em — wider than a real
/// space — so no threshold separates it from a word break, and full-text
/// search never matched the split words. Most generators write real space
/// glyphs, and when a document has them they are the reliable signal; only a
/// document without them (some TeX output) falls back to geometry.
struct TextOutput {
    text:       String,
    last_end:   f64,
    last_y:     f64,
    first_char: bool,
    flip:       pdf_extract::Transform,
}

impl Default for TextOutput {
    fn default() -> Self {
        Self {
            text:       String::new(),
            last_end:   100_000.0,
            last_y:     0.0,
            first_char: false,
            flip:       pdf_extract::Transform::identity(),
        }
    }
}

impl TextOutput {
    /// Resolve the geometric word-break candidates for the whole document:
    /// dropped if the PDF has real spaces, kept as spaces if it has none.
    fn finish(self) -> String {
        let (spaces, visible) = self.text.chars().fold((0usize, 0usize), |(s, v), c| match c {
            ' ' => (s + 1, v + 1),
            GAP_MARK => (s, v),
            c if c.is_whitespace() => (s, v),
            _ => (s, v + 1),
        });
        let has_real_spaces = visible > 0 && spaces as f64 / visible as f64 >= REAL_SPACE_SHARE;
        self.text.replace(GAP_MARK, if has_real_spaces { "" } else { " " })
    }
}

impl pdf_extract::OutputDev for TextOutput {
    fn begin_page(
        &mut self,
        _page_num: u32,
        media_box: &pdf_extract::MediaBox,
        _art_box: Option<(f64, f64, f64, f64)>,
    ) -> std::result::Result<(), pdf_extract::OutputError> {
        self.flip = pdf_extract::Transform::row_major(1., 0., 0., -1., 0., media_box.ury - media_box.lly);
        if !self.text.is_empty() {
            self.text.push_str("\n\n");
        }
        self.last_end = 100_000.0;
        Ok(())
    }

    fn end_page(&mut self) -> std::result::Result<(), pdf_extract::OutputError> {
        Ok(())
    }

    fn output_character(
        &mut self,
        trm: &pdf_extract::Transform,
        width: f64,
        _spacing: f64,
        font_size: f64,
        ch: &str,
    ) -> std::result::Result<(), pdf_extract::OutputError> {
        let position = trm.post_transform(&self.flip);
        // Font size in page space: side of the square with the same area as
        // the transformed (font_size × font_size) box.
        let vx = font_size * trm.m11 + font_size * trm.m21;
        let vy = font_size * trm.m12 + font_size * trm.m22;
        let size = (vx * vy).abs().sqrt();
        let (x, y) = (position.m31, position.m32);

        if self.first_char {
            if (y - self.last_y).abs() > size * 1.5 {
                self.text.push('\n');
            }
            // Moved left and down: a new line.
            if x < self.last_end && (y - self.last_y).abs() > size * 0.5 {
                self.text.push('\n');
            }
            let already_spaced = self.text.ends_with(char::is_whitespace) || ch.starts_with(char::is_whitespace);
            let gap_em = (x - self.last_end) / size;
            let same_line = (y - self.last_y).abs() <= size * 0.5;
            if same_line && gap_em >= COLUMN_GAP_EM && !self.text.ends_with('\t') {
                self.text.push('\t');
            } else if gap_em > WORD_GAP_EM && !already_spaced {
                self.text.push(GAP_MARK);
            }
        }
        self.text.push_str(ch);
        self.first_char = false;
        self.last_y = y;
        self.last_end = x + width * size;
        Ok(())
    }

    fn begin_word(&mut self) -> std::result::Result<(), pdf_extract::OutputError> {
        self.first_char = true;
        Ok(())
    }

    fn end_word(&mut self) -> std::result::Result<(), pdf_extract::OutputError> {
        Ok(())
    }

    fn end_line(&mut self) -> std::result::Result<(), pdf_extract::OutputError> {
        Ok(())
    }
}

/// DOCX = a ZIP archive whose `word/document.xml` holds the body. Text lives
/// in `<w:t>` runs; paragraphs (`</w:p>`), line breaks (`<w:br/>`, `<w:cr/>`)
/// and tabs (`<w:tab/>`) become `\n` / `\n` / `\t`, so the chunker sees the
/// document's structure. Headers, footers, comments and footnotes are
/// separate parts and deliberately not read.
fn extract_docx(bytes: &[u8]) -> Result<String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("could not open DOCX (not a valid ZIP archive)")?;
    let part = archive
        .by_name("word/document.xml")
        .map_err(|_| anyhow::anyhow!("not a Word document (no word/document.xml) — .xlsx / .pptx are not supported"))?;
    if part.size() > MAX_DOCX_XML_BYTES {
        bail!("DOCX body is too large ({} MB uncompressed)", part.size() / (1024 * 1024));
    }
    let mut xml = String::new();
    part.take(MAX_DOCX_XML_BYTES)
        .read_to_string(&mut xml)
        .context("could not read word/document.xml")?;

    let mut reader = Reader::from_str(&xml);
    let mut out = String::new();
    let mut in_text = false;
    loop {
        match reader.read_event().context("malformed word/document.xml")? {
            Event::Start(e) if e.name().as_ref() == b"w:t" => in_text = true,
            Event::End(e) => match e.name().as_ref() {
                b"w:t" => in_text = false,
                b"w:p" => out.push('\n'),
                _ => {}
            },
            Event::Empty(e) => match e.name().as_ref() {
                b"w:tab" => out.push('\t'),
                b"w:br" | b"w:cr" => out.push('\n'),
                b"w:p" => out.push('\n'),
                _ => {}
            },
            Event::Text(t) if in_text => out.push_str(&t.decode().context("bad text in DOCX")?),
            // quick-xml reports entities (&amp;, &#8212;) separately from the
            // text around them.
            Event::GeneralRef(r) if in_text => {
                if let Some(ch) = r.resolve_char_ref().context("bad character reference in DOCX")? {
                    out.push(ch);
                } else {
                    let name = r.decode().context("bad entity in DOCX")?;
                    match quick_xml::escape::resolve_predefined_entity(&name) {
                        Some(s) => out.push_str(s),
                        None => bail!("unknown entity &{name}; in DOCX"),
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// Make extracted text safe and compact for chunking and storage:
/// NUL and other control characters removed (PostgreSQL TEXT rejects NUL
/// outright — one stray byte from a PDF would fail the whole insert),
/// CRLF → LF, trailing spaces trimmed, runs of blank lines collapsed.
fn normalize(text: &str) -> String {
    let cleaned: String = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect();

    let mut out = String::with_capacity(cleaned.len());
    let mut blank_run = 0;
    for line in cleaned.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A minimal, valid single-page PDF with a text layer in Helvetica, the
    /// xref offsets computed rather than hand-counted.
    fn pdf_with_text(text: &str) -> Vec<u8> {
        if text.is_empty() {
            pdf_with_content("")
        } else {
            pdf_with_content(&format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET"))
        }
    }

    /// Same PDF with a raw content stream, for placing glyph runs at exact
    /// positions. Helvetica widths (Adobe AFM, per 1000 em) used below:
    /// V 667, a 556, c 500, t 278, i 222, o 556, n 556, space 278, r 333,
    /// e 556, q 556, u 556, s 500, N 722, m 833.
    fn pdf_with_content(content: &str) -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_string(),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref_at = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for off in offsets {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n", objects.len() + 1).as_bytes(),
        );
        pdf
    }

    fn docx_with_body(body_xml: &str) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("[Content_Types].xml", opts).unwrap();
            zip.write_all(b"<Types/>").unwrap();
            zip.start_file("word/document.xml", opts).unwrap();
            write!(
                zip,
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body_xml}</w:body></w:document>"#
            )
            .unwrap();
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn pdf_text_layer_is_extracted() {
        let text = extract_text(&pdf_with_text("Vacation requests go to HR"), "policy.pdf").unwrap();
        assert!(text.contains("Vacation requests go to HR"), "{text:?}");
    }

    #[test]
    fn pdf_without_text_layer_is_refused_as_scanned() {
        let err = extract_text(&pdf_with_text(""), "scan.pdf").unwrap_err().to_string();
        assert!(err.contains("scanned PDF"), "{err}");
    }

    #[test]
    fn garbage_after_pdf_magic_is_an_error_not_a_panic() {
        let err = extract_text(b"%PDF-1.4\nthis is not a pdf at all", "broken.pdf").unwrap_err().to_string();
        assert!(err.contains("PDF"), "{err}");
    }

    /// Real-world regression fixture: tests/fixtures/chromium_ru_en.pdf,
    /// printed by headless Edge from chromium_ru_en.html (embedded subset
    /// TrueType fonts with ToUnicode, Cyrillic + Latin, a table, two pages).
    /// pdf-extract's own output split words there ("к алендарных",
    /// "t o HR", "adv ance") because it computes ~1 em gaps inside them.
    #[test]
    fn chromium_pdf_keeps_words_whole() {
        let bytes = include_bytes!("../../tests/fixtures/chromium_ru_en.pdf");
        let text = extract_text(bytes, "chromium_ru_en.pdf").unwrap();
        for expected in [
            "Ежегодный отпуск составляет 28 календарных дней.",
            "submit to HR at least 14 days in advance.",
            "Итого: R&D-отделу доступно 31 день — с учётом «северных» надбавок.",
            "Стаж\tДополнительные дни",
            "до 5 лет\t0",
            "Страница 2. Порядок согласования: руководитель → отдел кадров → бухгалтерия.",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
        assert!(!text.contains(GAP_MARK));
    }

    /// A document with real space glyphs: a geometric gap inside a word
    /// (here 0.5 em between "reques" and "ts") is not a word break.
    #[test]
    fn pdf_with_real_spaces_ignores_in_word_gaps() {
        // "Vacation reques" = 7226/1000 em = 86.712 pt at 12 pt; +6 pt = 0.5 em gap.
        let pdf = pdf_with_content("BT /F1 12 Tf 72 720 Td (Vacation reques) Tj 92.712 0 Td (ts go to HR) Tj ET");
        assert_eq!(extract_text(&pdf, "a.pdf").unwrap(), "Vacation requests go to HR");
    }

    /// A document without any space glyphs (words placed by position only):
    /// geometric gaps are the word breaks; a column-sized one is a tab.
    #[test]
    fn pdf_without_space_glyphs_uses_geometry() {
        // "Name" = 2667/1000 em = 32.004 pt; Td 40 → 0.67 em gap → space.
        let words = pdf_with_content("BT /F1 12 Tf 72 720 Td (Name) Tj 40 0 Td (Value) Tj ET");
        assert_eq!(extract_text(&words, "b.pdf").unwrap(), "Name Value");
        // Td 72 → 3.33 em gap → column break.
        let columns = pdf_with_content("BT /F1 12 Tf 72 720 Td (Name) Tj 72 0 Td (Value) Tj ET");
        assert_eq!(extract_text(&columns, "c.pdf").unwrap(), "Name\tValue");
    }

    /// DOCX as Word actually writes it: runs split mid-word by spell-check
    /// marks, bookmarks, a hyperlink, a field (its code must not leak), a
    /// tracked insertion (kept) and deletion (dropped), a table.
    #[test]
    fn docx_as_word_writes_it() {
        let body = concat!(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:bookmarkStart w:id="0" w:name="_Toc1"/><w:r><w:t>Регламент</w:t></w:r><w:r><w:t xml:space="preserve"> отпусков</w:t></w:r><w:bookmarkEnd w:id="0"/></w:p>"#,
            r#"<w:p><w:r><w:t xml:space="preserve">Отпуск </w:t></w:r><w:proofErr w:type="spellStart"/><w:r><w:rPr><w:b/></w:rPr><w:t>календ</w:t></w:r><w:r><w:t>арных</w:t></w:r><w:proofErr w:type="spellEnd"/><w:r><w:t xml:space="preserve"> дней: </w:t></w:r>"#,
            r#"<w:ins w:id="1" w:author="HR"><w:r><w:t>28</w:t></w:r></w:ins><w:del w:id="2" w:author="HR"><w:r><w:delText>24</w:delText></w:r></w:del></w:p>"#,
            r#"<w:p><w:r><w:t xml:space="preserve">См. </w:t></w:r><w:hyperlink r:id="rId5" xmlns:r="r"><w:r><w:t>портал</w:t></w:r></w:hyperlink><w:r><w:t xml:space="preserve">, стр. </w:t></w:r>"#,
            r#"<w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText xml:space="preserve"> PAGEREF _Toc1 \h </w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>2</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>"#,
            r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Стаж</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Дни</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
            r#"<w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr>"#,
        );
        let text = extract_text(&docx_with_body(body), "policy.docx").unwrap();
        assert_eq!(text, "Регламент отпусков\nОтпуск календарных дней: 28\nСм. портал, стр. 2\nСтаж\nДни");
    }

    #[test]
    fn docx_paragraphs_breaks_tabs_and_entities() {
        let body = concat!(
            r#"<w:p><w:r><w:t>Отпуск</w:t></w:r><w:r><w:t xml:space="preserve"> оформляется</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>A</w:t><w:tab/><w:t>B</w:t><w:br/><w:t>R&amp;D &#8212; 14 days</w:t></w:r></w:p>"#,
        );
        let text = extract_text(&docx_with_body(body), "hr.docx").unwrap();
        assert_eq!(text, "Отпуск оформляется\nA\tB\nR&D \u{2014} 14 days");
    }

    #[test]
    fn zip_that_is_not_a_word_document_is_refused() {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            zip.start_file("xl/workbook.xml", zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(b"<workbook/>").unwrap();
            zip.finish().unwrap();
        }
        let err = extract_text(&buf.into_inner(), "sheet.docx").unwrap_err().to_string();
        assert!(err.contains("not a Word document"), "{err}");
    }

    #[test]
    fn plain_text_is_normalized() {
        let text = extract_text(b"line one\r\n\r\n\r\n\r\nline two   \n", "a.md").unwrap();
        assert_eq!(text, "line one\n\nline two");
    }

    #[test]
    fn utf16_notepad_text_and_utf8_bom_are_decoded() {
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "Отпуск: 14 дней".encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(extract_text(&utf16, "notes.txt").unwrap(), "Отпуск: 14 дней");
        assert_eq!(extract_text("\u{feff}# Title".as_bytes(), "a.md").unwrap(), "# Title");
    }

    #[test]
    fn nul_from_an_extractor_never_reaches_the_database() {
        // PostgreSQL TEXT rejects NUL; normalize strips it whatever the source.
        assert_eq!(normalize("a\u{0}b\u{7}c"), "abc");
    }

    #[test]
    fn binary_named_txt_and_unknown_types_are_refused() {
        assert!(extract_text(b"MZ\x90\x00\x03", "virus.txt").is_err());
        assert!(extract_text(b"\xd0\xcf\x11\xe0 legacy word", "old.doc").is_err());
        assert!(check_supported("old.doc").is_err());
        assert!(check_supported("Report.PDF").is_ok());
        assert!(check_supported("notes").is_err());
    }
}
