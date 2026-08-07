//! OOXML (`.docx` / `.xlsx` / `.pptx`): a ZIP of XML parts, read with `zip` +
//! `quick-xml`.
//!
//! ## Why hand-wired rather than a per-format crate
//!
//! Three formats and the document properties come out of *one* pair of
//! dependencies (+8 crates, +228 KiB measured on issue #40). The alternatives
//! were a writer-oriented docx library whose default build does not compile and
//! a spreadsheet-only crate, i.e. a different dependency family per format.
//!
//! ## Element matching is by local name
//!
//! `w:t`, `a:t` and `t` are all "a run of text" and producers do not agree on
//! the prefix, so every match below is on the **local** name. It costs nothing
//! and it means a document written by something other than Word still reads.
//! The collision risk is small in practice: the neighbouring elements are
//! `tab`, `tbl`, `tr`, `tc`, none of which is `t`.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{anyhow, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use super::{clean, iso_date, EmbeddedMeta, Sink};

/// Ratio between the text budget and the XML we are willing to inflate to reach
/// it. Markup is most of an OOXML part, so a 1:1 cap would truncate a document
/// long before its text hit the limit.
const XML_INFLATION: u64 = 16;

/// Cap on `docProps/core.xml`. Properties are a handful of short strings; a
/// part this size is already pathological.
const PROPS_LIMIT: u64 = 1 << 20;

type Archive = zip::ZipArchive<BufReader<File>>;

fn open(path: &Path) -> Result<Archive> {
    let file = File::open(path).map_err(|e| anyhow!("failed to open: {e}"))?;
    zip::ZipArchive::new(BufReader::new(file))
        .map_err(|e| anyhow!("not a readable OOXML (ZIP) container: {e}"))
}

/// Read one part, bounded. `Ok(None)` means the part is simply absent.
fn read_part(zip: &mut Archive, name: &str, limit: u64) -> Result<Option<Vec<u8>>> {
    let entry = match zip.by_name(name) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(anyhow!("failed to read {name}: {e}")),
    };
    let mut buf = Vec::new();
    entry
        .take(limit)
        .read_to_end(&mut buf)
        .map_err(|e| anyhow!("failed to decompress {name}: {e}"))?;
    Ok(Some(buf))
}

/// Whether a start/empty element's local name is `name`.
fn is(e: &BytesStart<'_>, name: &[u8]) -> bool {
    e.local_name().as_ref() == name
}

/// The character data of a text event: decoded, then entity-unescaped.
///
/// Both halves are needed. `decode` turns the reader's bytes into `str`;
/// `unescape` turns `&amp;` back into `&`. Skipping the second one puts raw
/// entity references into the index, where nobody will ever search for them.
fn text_of(t: &quick_xml::events::BytesText<'_>) -> Result<String> {
    let decoded = t.decode().map_err(|e| anyhow!("bad XML encoding: {e}"))?;
    let unescaped =
        quick_xml::escape::unescape(&decoded).map_err(|e| anyhow!("bad XML entity: {e}"))?;
    Ok(unescaped.into_owned())
}

/// Attribute value of `name` on a start element, as a `String`.
fn attr(e: &BytesStart<'_>, name: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if a.key.as_ref() == name || a.key.local_name().as_ref() == name {
            String::from_utf8(a.value.into_owned()).ok()
        } else {
            None
        }
    })
}

// ── Body extraction ─────────────────────────────────────────────────────────

/// `word/document.xml`: `<w:t>` runs, joined, with a line break per `<w:p>`.
pub(super) fn docx_body(path: &Path, max_bytes: u64) -> Result<String> {
    let mut zip = open(path)?;
    let part = read_part(
        &mut zip,
        "word/document.xml",
        max_bytes.saturating_mul(XML_INFLATION),
    )?
    .ok_or_else(|| anyhow!("word/document.xml is missing — not a DOCX package"))?;
    let mut sink = Sink::new(max_bytes);
    runs_to_sink(&part, &mut sink)?;
    Ok(sink.finish())
}

/// `ppt/slides/slideN.xml`: `<a:t>` runs, one slide after another in slide
/// order.
///
/// The slides are sorted by their **number**, not by their name: ZIP entry
/// order is whatever the producer felt like, and lexicographic order puts
/// slide10 before slide2. Two runs over the same file must give the same body,
/// or every re-index rewrites every document.
pub(super) fn pptx_body(path: &Path, max_bytes: u64) -> Result<String> {
    let mut zip = open(path)?;
    let mut slides: Vec<(u32, String)> = zip
        .file_names()
        .filter_map(|name| {
            let rest = name.strip_prefix("ppt/slides/slide")?;
            let number = rest.strip_suffix(".xml")?.parse::<u32>().ok()?;
            Some((number, name.to_string()))
        })
        .collect();
    if slides.is_empty() {
        return Err(anyhow!(
            "no ppt/slides/slideN.xml parts — not a PPTX package"
        ));
    }
    slides.sort();

    let mut sink = Sink::new(max_bytes);
    for (_, name) in slides {
        if sink.full {
            break;
        }
        if let Some(part) = read_part(&mut zip, &name, max_bytes.saturating_mul(XML_INFLATION))? {
            runs_to_sink(&part, &mut sink)?;
            sink.newline();
        }
    }
    Ok(sink.finish())
}

/// `xl/worksheets/sheetN.xml` resolved against `xl/sharedStrings.xml`.
///
/// Both cell paths matter. Most strings in a real spreadsheet live in the
/// shared table and the cell holds an index (`<c t="s"><v>7</v>`), but a cell
/// written by a streaming writer carries its text inline
/// (`<c t="inlineStr"><is><t>…`) and never touches the table at all — a file
/// that reads only the shared table comes back empty for those, which looks
/// exactly like a spreadsheet with no text in it.
pub(super) fn xlsx_body(path: &Path, max_bytes: u64) -> Result<String> {
    let mut zip = open(path)?;
    let limit = max_bytes.saturating_mul(XML_INFLATION);
    let shared = match read_part(&mut zip, "xl/sharedStrings.xml", limit)? {
        Some(part) => shared_strings(&part)?,
        None => Vec::new(),
    };

    let mut sheets: Vec<(u32, String)> = zip
        .file_names()
        .filter_map(|name| {
            let rest = name.strip_prefix("xl/worksheets/sheet")?;
            let number = rest.strip_suffix(".xml")?.parse::<u32>().ok()?;
            Some((number, name.to_string()))
        })
        .collect();
    if sheets.is_empty() {
        return Err(anyhow!(
            "no xl/worksheets/sheetN.xml parts — not an XLSX package"
        ));
    }
    sheets.sort();

    let mut sink = Sink::new(max_bytes);
    for (_, name) in sheets {
        if sink.full {
            break;
        }
        if let Some(part) = read_part(&mut zip, &name, limit)? {
            sheet_to_sink(&part, &shared, &mut sink)?;
            sink.newline();
        }
    }
    Ok(sink.finish())
}

/// Collect every `<…:t>` run, breaking a line at the end of each `<…:p>`.
fn runs_to_sink(xml: &[u8], sink: &mut Sink) -> Result<()> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut depth: usize = 0;
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| anyhow!("malformed XML at byte {}: {e}", reader.buffer_position()))?
        {
            Event::Start(e) => {
                if is(&e, b"t") {
                    depth += 1;
                }
            }
            Event::End(e) => {
                let name = e.local_name();
                match name.as_ref() {
                    b"t" => depth = depth.saturating_sub(1),
                    b"p" => sink.newline(),
                    _ => {}
                }
            }
            Event::Empty(e) => {
                let name = e.local_name();
                match name.as_ref() {
                    b"br" | b"cr" => sink.newline(),
                    b"tab" => sink.separator('\t'),
                    _ => {}
                }
            }
            Event::Text(t) if depth > 0 => {
                let text = text_of(&t)?;
                sink.push(&text);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
        if sink.full {
            break;
        }
    }
    Ok(())
}

/// `xl/sharedStrings.xml` → the indexed string table.
///
/// A rich-text entry is several `<r><t>` runs inside one `<si>`; they are one
/// cell value and are concatenated, not listed separately.
fn shared_strings(xml: &[u8]) -> Result<Vec<String>> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_si = false;
    let mut in_t: usize = 0;
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| anyhow!("malformed sharedStrings.xml: {e}"))?
        {
            Event::Start(e) => {
                let name = e.local_name();
                match name.as_ref() {
                    b"si" => {
                        in_si = true;
                        current.clear();
                    }
                    b"t" => in_t += 1,
                    _ => {}
                }
            }
            Event::End(e) => {
                let name = e.local_name();
                match name.as_ref() {
                    b"si" => {
                        in_si = false;
                        out.push(std::mem::take(&mut current));
                    }
                    b"t" => in_t = in_t.saturating_sub(1),
                    _ => {}
                }
            }
            Event::Text(t) if in_si && in_t > 0 => {
                let text = text_of(&t)?;
                current.push_str(&text);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// One worksheet: every cell's value, tab-separated, one line per row.
fn sheet_to_sink(xml: &[u8], shared: &[String], sink: &mut Sink) -> Result<()> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    // The `t` attribute of the cell currently open. "s" means "the `<v>` is an
    // index into the shared table"; everything else means the value is literal.
    let mut cell_type = String::new();
    let mut in_v = false;
    let mut in_t: usize = 0;
    let mut value = String::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| anyhow!("malformed worksheet XML: {e}"))?
        {
            Event::Start(e) => {
                let name = e.local_name();
                let name = name.as_ref().to_vec();
                match name.as_slice() {
                    b"c" => cell_type = attr(&e, b"t").unwrap_or_default(),
                    b"v" => {
                        in_v = true;
                        value.clear();
                    }
                    b"t" => in_t += 1,
                    _ => {}
                }
            }
            Event::End(e) => {
                let name = e.local_name();
                match name.as_ref() {
                    b"v" => {
                        in_v = false;
                        if cell_type == "s" {
                            // A shared index that points nowhere is a corrupt
                            // file, but dropping that one cell is a better
                            // answer than dropping the workbook.
                            if let Some(text) = value
                                .trim()
                                .parse::<usize>()
                                .ok()
                                .and_then(|i| shared.get(i))
                            {
                                sink.push(text);
                                sink.separator('\t');
                            }
                        } else if !value.trim().is_empty() {
                            sink.push(value.trim());
                            sink.separator('\t');
                        }
                    }
                    b"t" => in_t = in_t.saturating_sub(1),
                    b"row" => sink.newline(),
                    _ => {}
                }
            }
            Event::Text(t) => {
                let text = text_of(&t)?;
                if in_v {
                    value.push_str(&text);
                } else if in_t > 0 {
                    // `t="inlineStr"`: the text is right here, not in the table.
                    sink.push(&text);
                    sink.separator('\t');
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
        if sink.full {
            break;
        }
    }
    Ok(())
}

// ── Embedded metadata ───────────────────────────────────────────────────────

/// `docProps/core.xml` — the Dublin Core properties every OOXML package shares.
pub(super) fn core_properties(path: &Path) -> Result<EmbeddedMeta> {
    let mut zip = open(path)?;
    let Some(part) = read_part(&mut zip, "docProps/core.xml", PROPS_LIMIT)? else {
        // A package with no properties part is normal (LibreOffice omits it for
        // some templates); it is not a broken file.
        return Ok(EmbeddedMeta::default());
    };
    parse_core_properties(&part)
}

fn parse_core_properties(xml: &[u8]) -> Result<EmbeddedMeta> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut meta = EmbeddedMeta::default();
    let mut field: Vec<u8> = Vec::new();
    let mut text = String::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| anyhow!("malformed docProps/core.xml: {e}"))?
        {
            Event::Start(e) => {
                field = e.local_name().as_ref().to_vec();
                text.clear();
            }
            Event::Text(t) if !field.is_empty() => {
                let decoded = text_of(&t)?;
                text.push_str(&decoded);
            }
            Event::End(e) => {
                let name = e.local_name();
                if name.as_ref() == field.as_slice() {
                    match field.as_slice() {
                        // Both people-shaped fields feed one namespace. Someone
                        // looking for "a document masuda touched" does not care
                        // which of the two roles it was, and two namespaces
                        // would split that count without saying so.
                        b"creator" | b"lastModifiedBy" => meta.push_author(&text),
                        b"title" => meta.title = meta.title.take().or_else(|| clean(&text)),
                        b"created" => meta.date = meta.date.take().or_else(|| iso_date(&text)),
                        _ => {}
                    }
                }
                field.clear();
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_properties_read_japanese_values() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<cp:coreProperties xmlns:cp="x" xmlns:dc="y" xmlns:dcterms="z">
  <dc:title>四半期レポート</dc:title>
  <dc:creator>増田 太郎</dc:creator>
  <cp:lastModifiedBy>山田 花子</cp:lastModifiedBy>
  <dcterms:created>2024-03-15T01:02:03Z</dcterms:created>
</cp:coreProperties>"#
            .as_bytes();
        let meta = parse_core_properties(xml).unwrap().finish();
        assert_eq!(meta.title.as_deref(), Some("四半期レポート"));
        assert_eq!(meta.authors, ["増田 太郎", "山田 花子"]);
        assert_eq!(meta.date.as_deref(), Some("2024-03-15"));
    }

    #[test]
    fn an_empty_property_does_not_become_an_empty_tag() {
        let xml = r#"<cp:coreProperties xmlns:cp="x" xmlns:dc="y">
  <dc:title>   </dc:title><dc:creator></dc:creator></cp:coreProperties>"#
            .as_bytes();
        let meta = parse_core_properties(xml).unwrap();
        assert!(meta.is_empty(), "{meta:?}");
    }

    #[test]
    fn shared_strings_concatenate_rich_text_runs() {
        let xml =
            r#"<sst><si><t>単純</t></si><si><r><t>リッチ</t></r><r><t>テキスト</t></r></si></sst>"#;
        assert_eq!(
            shared_strings(xml.as_bytes()).unwrap(),
            ["単純", "リッチテキスト"]
        );
    }

    #[test]
    fn a_sheet_reads_both_shared_and_inline_cells() {
        let shared = vec!["共有文字列".to_string()];
        let xml = r#"<worksheet><sheetData>
  <row><c r="A1" t="s"><v>0</v></c><c r="B1" t="inlineStr"><is><t>インライン</t></is></c></row>
  <row><c r="A2"><v>42</v></c></row>
</sheetData></worksheet>"#;
        let mut sink = Sink::new(4096);
        sheet_to_sink(xml.as_bytes(), &shared, &mut sink).unwrap();
        assert_eq!(sink.finish(), "共有文字列\tインライン\n42");
    }

    #[test]
    fn runs_break_lines_on_paragraphs_not_on_runs() {
        let xml = r#"<w:document xmlns:w="x"><w:body>
  <w:p><w:r><w:t>日本語の</w:t></w:r><w:r><w:t>本文</w:t></w:r></w:p>
  <w:p><w:r><w:t>二段落目</w:t></w:r></w:p>
</w:body></w:document>"#;
        let mut sink = Sink::new(4096);
        runs_to_sink(xml.as_bytes(), &mut sink).unwrap();
        assert_eq!(sink.finish(), "日本語の本文\n二段落目");
    }
}
