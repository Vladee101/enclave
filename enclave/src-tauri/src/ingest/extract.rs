//! Text extraction for ingestion (ADR-0019, ADR-0020): plain text / Markdown,
//! PDF with a text layer, DOCX, and spreadsheets (XLSX, XLSM, XLSB, XLS, ODS).
//! Pure Rust — no native libraries to ship with the desktop app.
//!
//! The format is decided from the bytes, not from the browser-reported MIME
//! type (often empty or wrong for `.md`), with the file name as a tiebreaker
//! for plain text. Anything else is refused up front by `check_supported` at
//! upload time, and again here, so a job never "succeeds" with nothing in it.

use std::io::{Cursor, Read};

use anyhow::{bail, Context, Result};
use calamine::{Data, Reader as _};
use quick_xml::events::Event;
use quick_xml::Reader;

/// Extensions accepted at upload. Checked by name before anything is stored,
/// so an unsupported file is refused immediately instead of failing later in
/// the worker.
pub const SUPPORTED_EXTENSIONS: &[&str] =
    &["pdf", "docx", "txt", "md", "markdown", "xlsx", "xlsm", "xlsb", "xls", "ods"];

const SUPPORTED_LIST: &str = "PDF, DOCX, XLSX, XLS, ODS, TXT, MD";

/// How the extracted text is laid out, which decides how it is chunked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Running text: fixed-size windows with overlap.
    Prose,
    /// One self-describing record per line (spreadsheet rows): chunks are
    /// packed from whole lines, so a row is never cut in two.
    Rows,
}

#[derive(Debug)]
pub struct Extracted {
    pub text:   String,
    pub layout: Layout,
    /// Spreadsheets only: every sheet as typed rows, for calculations over
    /// the table (ADR-0022). Empty for documents.
    pub sheets: Vec<Sheet>,
}

/// A typed cell value — what the cell holds, not how it is displayed.
#[derive(Debug, Clone, PartialEq)]
pub enum CellValue {
    Number(f64),
    /// ISO: `2021-03-15`, or `2021-03-15 09:30` with a time of day.
    Date(String),
    Text(String),
}

/// One sheet as a table: its records (rows below the header row, or every
/// row when there is none) and a name for each column.
#[derive(Debug, Clone, PartialEq)]
pub struct Sheet {
    pub name:    String,
    /// Column names: header cells, column letters where there is no header.
    /// Unique within the sheet ("Сумма", "Сумма (2)").
    pub columns: Vec<String>,
    pub rows:    Vec<SheetRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SheetRow {
    /// Row number as Excel shows it.
    pub number: usize,
    /// One entry per column; None for empty and error cells.
    pub cells:  Vec<Option<CellValue>>,
}

/// How far down a sheet to look for the header row — title rows above it
/// are common ("Отчёт за март" in A1, headers in row 3).
const HEADER_SCAN_ROWS: usize = 10;

/// DOCX main part larger than this is refused rather than inflated: a
/// deliberately compressed archive could otherwise expand without bound.
const MAX_DOCX_XML_BYTES: u64 = 64 * 1024 * 1024;

pub fn check_supported(filename: &str) -> Result<()> {
    let ext = extension(filename);
    if SUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
        return Ok(());
    }
    bail!(
        "Unsupported file type{}. Supported: {SUPPORTED_LIST}.",
        if ext.is_empty() { String::new() } else { format!(" .{ext}") }
    )
}

/// Text only — see `extract`.
pub fn extract_text(bytes: &[u8], filename: &str) -> Result<String> {
    Ok(extract(bytes, filename)?.text)
}

/// Extract the text of a document and how it is laid out. Errors are
/// permanent (retrying will not help), and an empty result is an error: a
/// document with no text would be "ready" and never found.
pub fn extract(bytes: &[u8], filename: &str) -> Result<Extracted> {
    let ext = extension(filename);
    let mut sheets = Vec::new();
    let (text, layout) = if bytes.starts_with(b"%PDF-") {
        (extract_pdf(bytes)?, Layout::Prose)
    } else if bytes.starts_with(b"PK\x03\x04") {
        // Office Open XML and OpenDocument are all ZIP archives; what is
        // inside decides which one this is, not the name.
        match zip_kind(bytes)? {
            ZipKind::Word => (extract_docx(bytes)?, Layout::Prose),
            ZipKind::Spreadsheet => {
                let (text, parsed) = extract_spreadsheet(bytes)?;
                sheets = parsed;
                (text, Layout::Rows)
            }
            ZipKind::Other => bail!(
                "{filename} is an archive Enclave cannot read (not a Word document or a spreadsheet; .pptx is not supported)"
            ),
        }
    } else if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) {
        // Legacy OLE compound file: .xls, but also .doc / .ppt.
        if ext == "xls" {
            let (text, parsed) = extract_spreadsheet(bytes)?;
            sheets = parsed;
            (text, Layout::Rows)
        } else {
            bail!("{filename} is a legacy Office file (.doc / .ppt) — save it as .docx or PDF");
        }
    } else if matches!(ext.as_str(), "txt" | "md" | "markdown") {
        (decode_text(bytes).with_context(|| format!("{filename} is not a text file"))?, Layout::Prose)
    } else {
        bail!("Unsupported file type for {filename}. Supported: {SUPPORTED_LIST}.");
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
    Ok(Extracted { text, layout, sheets })
}

enum ZipKind {
    Word,
    Spreadsheet,
    Other,
}

fn zip_kind(bytes: &[u8]) -> Result<ZipKind> {
    let archive = zip::ZipArchive::new(Cursor::new(bytes)).context("could not open the file (not a valid ZIP archive)")?;
    let has = |name: &str| archive.index_for_name(name).is_some();
    Ok(if has("word/document.xml") {
        ZipKind::Word
    } else if has("xl/workbook.xml") || has("xl/workbook.bin") || (has("content.xml") && has("mimetype")) {
        ZipKind::Spreadsheet
    } else {
        ZipKind::Other
    })
}

/// A spreadsheet as one self-describing line per row:
///
///   [лист «Зарплаты», строка 5] Имя: Иванов; Отдел: Продажи; Оклад: 120000
///
/// Every row carries its sheet, its row number as Excel shows it, and its
/// column headers, so any chunk makes sense on its own: a CSV-style dump
/// would leave the headers in the first chunk only, and "120000" in the
/// fifth would mean nothing to either search or the model.
///
/// The header row is the first of the top `HEADER_SCAN_ROWS` rows with at
/// least two non-empty cells, all of them text; rows above it (report
/// titles) are kept as rows of their own. A sheet with no such row is
/// labelled by column letters. Values: formulas as their cached result,
/// dates as ISO dates, whole numbers without ".0", empty cells and error
/// values (#N/A) skipped.
///
/// Alongside the text, each sheet as a typed table (`Sheet`): the same
/// records, cell values instead of display strings (ADR-0022).
fn extract_spreadsheet(bytes: &[u8]) -> Result<(String, Vec<Sheet>)> {
    let mut workbook = calamine::open_workbook_auto_from_rs(Cursor::new(bytes))
        .map_err(|e| anyhow::anyhow!("could not read the spreadsheet: {e}"))?;
    let mut out = String::new();
    let mut sheets = Vec::new();

    for sheet in workbook.sheet_names() {
        let range = workbook
            .worksheet_range(&sheet)
            .map_err(|e| anyhow::anyhow!("could not read sheet «{sheet}»: {e}"))?;
        let Some((first_row, first_col)) = range.start() else {
            continue; // empty sheet
        };
        let rows: Vec<&[Data]> = range.rows().collect();

        let header_at = rows.iter().take(HEADER_SCAN_ROWS).position(|row| {
            let filled: Vec<&Data> = row.iter().filter(|c| cell_text(c).is_some()).collect();
            filled.len() >= 2 && filled.iter().all(|c| matches!(c, Data::String(_)))
        });
        let headers: Vec<String> = match header_at {
            Some(i) => rows[i]
                .iter()
                .enumerate()
                .map(|(j, c)| cell_text(c).unwrap_or_else(|| column_letter(first_col as usize + j)))
                .collect(),
            None => Vec::new(),
        };
        let mut table = Sheet {
            name:    sheet.clone(),
            columns: unique_names(
                (0..range.width())
                    .map(|j| headers.get(j).cloned().unwrap_or_else(|| column_letter(first_col as usize + j)))
                    .collect(),
            ),
            rows:    Vec::new(),
        };

        for (i, row) in rows.iter().enumerate() {
            if Some(i) == header_at {
                continue;
            }
            // Title rows above the header row are not records of it.
            let labelled = header_at.is_some_and(|h| i > h);
            let fields: Vec<String> = row
                .iter()
                .enumerate()
                .filter_map(|(j, cell)| {
                    let value = cell_text(cell)?;
                    let label = if labelled { headers[j].clone() } else { column_letter(first_col as usize + j) };
                    Some(format!("{label}: {value}"))
                })
                .collect();
            if fields.is_empty() {
                continue;
            }
            let row_number = first_row as usize + i + 1;
            out.push_str(&format!("[лист «{sheet}», строка {row_number}] {}\n", fields.join("; ")));
            // Title rows above the header are not records of the table.
            if header_at.is_none() || labelled {
                table.rows.push(SheetRow { number: row_number, cells: row.iter().map(cell_value).collect() });
            }
        }
        if !table.rows.is_empty() {
            sheets.push(table);
        }
    }
    Ok((out, sheets))
}

/// Duplicate column names get a suffix — "Сумма", "Сумма (2)" — so a
/// calculation can name a column unambiguously.
fn unique_names(names: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    names
        .into_iter()
        .map(|name| {
            let mut candidate = name.clone();
            let mut n = 2;
            while !seen.insert(candidate.clone()) {
                candidate = format!("{name} ({n})");
                n += 1;
            }
            candidate
        })
        .collect()
}

/// A cell as a typed value, or None for empty / error cells. Numbers and
/// dates keep their type; everything else is its display text.
fn cell_value(cell: &Data) -> Option<CellValue> {
    match cell {
        Data::Int(i) => Some(CellValue::Number(*i as f64)),
        Data::Float(f) if f.is_finite() => Some(CellValue::Number(*f)),
        Data::DateTime(dt) if !dt.is_duration() => match dt.as_datetime() {
            // Time-only cells sit on Excel's day zero: not dates.
            Some(t) if t.date() <= chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap() => {
                cell_text(cell).map(CellValue::Text)
            }
            Some(_) => cell_text(cell).map(CellValue::Date),
            None => cell_text(cell).map(CellValue::Text),
        },
        Data::DateTimeIso(s) if is_iso_date(s) => Some(CellValue::Date(s.replacen('T', " ", 1))),
        _ => cell_text(cell).map(CellValue::Text),
    }
}

/// Starts with YYYY-MM-DD.
pub(crate) fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 10
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
}

/// A cell as text, or None for empty / error cells.
fn cell_text(cell: &Data) -> Option<String> {
    let text = match cell {
        Data::Empty | Data::Error(_) => return None,
        Data::String(s) => s.split_whitespace().collect::<Vec<_>>().join(" "),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => format_number(*f),
        Data::Bool(b) => (if *b { "да" } else { "нет" }).to_string(),
        Data::DateTime(dt) => {
            if dt.is_duration() {
                match dt.as_duration() {
                    Some(d) => {
                        let secs = d.num_seconds();
                        format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
                    }
                    None => format_number(dt.as_f64()),
                }
            } else {
                match dt.as_datetime() {
                    Some(t) if t.time() == chrono::NaiveTime::MIN => t.format("%Y-%m-%d").to_string(),
                    // Time-only cells sit on Excel's day zero.
                    Some(t) if t.date() <= chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap() => {
                        t.format("%H:%M").to_string()
                    }
                    Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
                    None => format_number(dt.as_f64()),
                }
            }
        }
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
    };
    (!text.is_empty()).then_some(text)
}

/// 120000.0 → "120000", 0.1 + 0.2 → "0.3": at most ten decimals, trailing
/// zeros dropped — what a person reads in the cell, not the binary float.
pub(crate) fn format_number(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        return format!("{f:.0}");
    }
    let s = format!("{f:.10}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// 0 → A, 25 → Z, 26 → AA.
fn column_letter(mut index: usize) -> String {
    let mut letters = Vec::new();
    loop {
        letters.push(b'A' + (index % 26) as u8);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    letters.reverse();
    String::from_utf8(letters).unwrap_or_default()
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

    /// A workbook shaped like real ones: a report title above the header
    /// row, typed cells (number, fraction, date, boolean, formula with its
    /// cached result, error, empty), a sheet without headers, an empty sheet.
    fn sample_workbook() -> Vec<u8> {
        use rust_xlsxwriter::{ExcelDateTime, Format, Formula, Workbook};
        let mut wb = Workbook::new();
        let date = Format::new().set_num_format("dd.mm.yyyy");

        let ws = wb.add_worksheet();
        ws.set_name("Зарплаты").unwrap();
        ws.write_string(0, 0, "Отчёт по зарплатам, март").unwrap();
        for (col, header) in ["Имя", "Отдел", "Оклад", "Дата приёма", "Активен", "Годовой"].iter().enumerate() {
            ws.write_string(2, col as u16, *header).unwrap();
        }
        ws.write_string(3, 0, "Иванов  И.И.").unwrap();
        ws.write_string(3, 1, "Продажи").unwrap();
        ws.write_number(3, 2, 120000.0).unwrap();
        ws.write_datetime_with_format(3, 3, &ExcelDateTime::from_ymd(2021, 3, 15).unwrap(), &date).unwrap();
        ws.write_boolean(3, 4, true).unwrap();
        ws.write_formula(3, 5, Formula::new("=C4*12").set_result("1440000")).unwrap();
        ws.write_string(4, 0, "Петрова").unwrap();
        // B5 empty on purpose.
        ws.write_number(4, 2, 95500.5).unwrap();
        ws.write_boolean(4, 4, false).unwrap();
        ws.write_formula(4, 5, Formula::new("=1/0").set_result("#DIV/0!")).unwrap();

        let ws = wb.add_worksheet();
        ws.set_name("Коды").unwrap();
        ws.write_number(0, 0, 101.0).unwrap();
        ws.write_number(0, 1, 0.1 + 0.2).unwrap();

        let ws = wb.add_worksheet();
        ws.set_name("Пусто").unwrap();

        wb.save_to_buffer().unwrap()
    }

    #[test]
    fn spreadsheet_rows_carry_sheet_row_and_headers() {
        let extracted = extract(&sample_workbook(), "salaries.xlsx").unwrap();
        assert_eq!(extracted.layout, Layout::Rows);
        assert_eq!(
            extracted.text,
            [
                "[лист «Зарплаты», строка 1] A: Отчёт по зарплатам, март",
                "[лист «Зарплаты», строка 4] Имя: Иванов И.И.; Отдел: Продажи; Оклад: 120000; Дата приёма: 2021-03-15; Активен: да; Годовой: 1440000",
                "[лист «Зарплаты», строка 5] Имя: Петрова; Оклад: 95500.5; Активен: нет",
                "[лист «Коды», строка 1] A: 101; B: 0.3",
            ]
            .join("\n")
        );
    }

    #[test]
    fn spreadsheet_sheets_are_typed_tables_without_title_rows() {
        use CellValue::*;
        let sheets = extract(&sample_workbook(), "salaries.xlsx").unwrap().sheets;
        // The empty sheet has no table; the title row is not a record.
        assert_eq!(sheets.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Зарплаты", "Коды"]);
        let salaries = &sheets[0];
        assert_eq!(salaries.columns, ["Имя", "Отдел", "Оклад", "Дата приёма", "Активен", "Годовой"]);
        assert_eq!(salaries.rows.iter().map(|r| r.number).collect::<Vec<_>>(), [4, 5]);
        assert_eq!(
            salaries.rows[0].cells,
            [
                Some(Text("Иванов И.И.".into())),
                Some(Text("Продажи".into())),
                Some(Number(120000.0)),
                Some(Date("2021-03-15".into())),
                Some(Text("да".into())),
                Some(Number(1440000.0)),
            ]
        );
        assert_eq!(salaries.rows[1].cells[1], None);
        assert_eq!(salaries.rows[1].cells[5], None, "an error value is an empty cell");
        // No header row: columns are letters.
        assert_eq!(sheets[1].columns, ["A", "B"]);
        assert_eq!(unique_names(vec!["Сумма".into(), "Сумма".into(), "Сумма (2)".into()]), ["Сумма", "Сумма (2)", "Сумма (2) (2)"]);
    }

    #[test]
    fn spreadsheet_is_recognised_by_content_not_name() {
        // An .xlsx renamed to .docx is still read as a spreadsheet; a
        // workbook is never mistaken for a Word document.
        let text = extract_text(&sample_workbook(), "misnamed.docx").unwrap();
        assert!(text.starts_with("[лист «Зарплаты»"), "{text}");
    }

    #[test]
    fn legacy_ole_files_other_than_xls_are_refused_with_a_hint() {
        let err = extract_text(b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1 not really", "old.doc").unwrap_err().to_string();
        assert!(err.contains("legacy Office file"), "{err}");
        assert!(check_supported("book.xlsx").is_ok());
        assert!(check_supported("old.XLS").is_ok());
        assert!(check_supported("slides.pptx").is_err());
    }

    #[test]
    fn column_letters_and_numbers() {
        assert_eq!(
            [0, 1, 25, 26, 27, 51, 52, 701, 702].map(column_letter),
            ["A", "B", "Z", "AA", "AB", "AZ", "BA", "ZZ", "AAA"].map(String::from)
        );
        assert_eq!(format_number(120000.0), "120000");
        assert_eq!(format_number(0.1 + 0.2), "0.3");
        assert_eq!(format_number(-2.50), "-2.5");
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
    fn zips_that_are_neither_documents_nor_workbooks_are_refused() {
        let zip_with = |part: &str| {
            let mut buf = Cursor::new(Vec::new());
            {
                let mut zip = zip::ZipWriter::new(&mut buf);
                zip.start_file(part, zip::write::SimpleFileOptions::default()).unwrap();
                zip.write_all(b"<x/>").unwrap();
                zip.finish().unwrap();
            }
            buf.into_inner()
        };
        // A presentation: recognised as neither, refused with the reason.
        let err = extract_text(&zip_with("ppt/presentation.xml"), "slides.pptx").unwrap_err().to_string();
        assert!(err.contains("cannot read") && err.contains(".pptx"), "{err}");
        // Looks like a workbook, but is broken: the parser's error, not a panic.
        let err = extract_text(&zip_with("xl/workbook.xml"), "broken.xlsx").unwrap_err().to_string();
        assert!(err.contains("could not read the spreadsheet"), "{err}");
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
