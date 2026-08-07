//! Real document fixtures, built byte by byte.
//!
//! Included with `#[path]` by the tests that need it rather than checked into
//! the tree as binary blobs, for two reasons that both cost debugging time
//! later: a committed `.docx` cannot be reviewed in a diff, and a fixture whose
//! construction is invisible is a fixture that can quietly stop containing what
//! the test claims it contains (a `.docx` with an empty `document.xml` still
//! opens, and every assertion about "no text found" keeps passing).
//!
//! Everything below carries Japanese text or values, because that is the case
//! this project exists for and the one where an encoding mistake actually shows
//! up. An ASCII fixture would pass over a broken UTF-16 path.

#![allow(dead_code)]

use std::io::Write;
use std::path::Path;

// ── OOXML ───────────────────────────────────────────────────────────────────

/// Write a ZIP with the given parts. Deflated, like a real Office file — the
/// stored-only path would not exercise the decompressor the extractor relies on.
pub fn write_zip(path: &Path, parts: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, content) in parts {
        zip.start_file(*name, options).unwrap();
        zip.write_all(content).unwrap();
    }
    zip.finish().unwrap();
}

/// `docProps/core.xml` with the three properties the tag engine reads.
pub fn core_props(title: &str, creator: &str, last_modified_by: &str, created: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties
  xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
  xmlns:dc="http://purl.org/dc/elements/1.1/"
  xmlns:dcterms="http://purl.org/dc/terms/">
  <dc:title>{title}</dc:title>
  <dc:creator>{creator}</dc:creator>
  <cp:lastModifiedBy>{last_modified_by}</cp:lastModifiedBy>
  <dcterms:created xsi:type="dcterms:W3CDTF"
    xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">{created}</dcterms:created>
</cp:coreProperties>"#
    )
    .into_bytes()
}

/// A `.docx` whose body is the given paragraphs.
pub fn write_docx(path: &Path, paragraphs: &[&str], props: Option<Vec<u8>>) {
    let body: String = paragraphs
        .iter()
        .map(|p| format!(r#"<w:p><w:r><w:t xml:space="preserve">{p}</w:t></w:r></w:p>"#))
        .collect();
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>{body}</w:body></w:document>"#
    )
    .into_bytes();

    let mut parts: Vec<(&str, Vec<u8>)> = vec![
        ("[Content_Types].xml", content_types()),
        ("word/document.xml", document),
    ];
    if let Some(props) = props {
        parts.push(("docProps/core.xml", props));
    }
    let refs: Vec<(&str, &[u8])> = parts.iter().map(|(n, c)| (*n, c.as_slice())).collect();
    write_zip(path, &refs);
}

/// An `.xlsx` with one sheet.
///
/// `shared` are the entries of `xl/sharedStrings.xml`; `rows` describe cells as
/// either a shared index or an inline string, so one fixture covers both cell
/// encodings a real spreadsheet mixes.
pub enum Cell {
    Shared(usize),
    Inline(&'static str),
    Number(&'static str),
}

pub fn write_xlsx(path: &Path, shared: &[&str], rows: &[Vec<Cell>], props: Option<Vec<u8>>) {
    let sst_items: String = shared
        .iter()
        .map(|s| format!("<si><t>{s}</t></si>"))
        .collect();
    let sst = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="{}" uniqueCount="{}">{sst_items}</sst>"#,
        shared.len(),
        shared.len()
    )
    .into_bytes();

    let mut sheet_rows = String::new();
    for (r, row) in rows.iter().enumerate() {
        sheet_rows.push_str(&format!(r#"<row r="{}">"#, r + 1));
        for (c, cell) in row.iter().enumerate() {
            let reference = format!("{}{}", (b'A' + c as u8) as char, r + 1);
            match cell {
                Cell::Shared(i) => {
                    sheet_rows.push_str(&format!(r#"<c r="{reference}" t="s"><v>{i}</v></c>"#))
                }
                Cell::Inline(text) => sheet_rows.push_str(&format!(
                    r#"<c r="{reference}" t="inlineStr"><is><t>{text}</t></is></c>"#
                )),
                Cell::Number(value) => {
                    sheet_rows.push_str(&format!(r#"<c r="{reference}"><v>{value}</v></c>"#))
                }
            }
        }
        sheet_rows.push_str("</row>");
    }
    let sheet = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>{sheet_rows}</sheetData></worksheet>"#
    )
    .into_bytes();

    let mut parts: Vec<(&str, Vec<u8>)> = vec![
        ("[Content_Types].xml", content_types()),
        ("xl/sharedStrings.xml", sst),
        ("xl/worksheets/sheet1.xml", sheet),
    ];
    if let Some(props) = props {
        parts.push(("docProps/core.xml", props));
    }
    let refs: Vec<(&str, &[u8])> = parts.iter().map(|(n, c)| (*n, c.as_slice())).collect();
    write_zip(path, &refs);
}

/// A `.pptx`, one paragraph per slide.
///
/// The slides are written to the archive in *reverse* order on purpose: slide
/// order must come from the part name, not from where the entry happens to sit
/// in the ZIP.
pub fn write_pptx(path: &Path, slides: &[&str], props: Option<Vec<u8>>) {
    let mut parts: Vec<(String, Vec<u8>)> =
        vec![("[Content_Types].xml".to_string(), content_types())];
    for (i, text) in slides.iter().enumerate().rev() {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree><p:sp><p:txBody>
<a:p><a:r><a:t>{text}</a:t></a:r></a:p>
</p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
        );
        parts.push((format!("ppt/slides/slide{}.xml", i + 1), xml.into_bytes()));
    }
    if let Some(props) = props {
        parts.push(("docProps/core.xml".to_string(), props));
    }
    let refs: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|(n, c)| (n.as_str(), c.as_slice()))
        .collect();
    write_zip(path, &refs);
}

fn content_types() -> Vec<u8> {
    br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="xml" ContentType="application/xml"/>
</Types>"#
        .to_vec()
}

// ── PDF ─────────────────────────────────────────────────────────────────────

/// A minimal single-page PDF whose text is `text`, encoded as Identity-H with a
/// `ToUnicode` CMap.
///
/// This is the shape the dependency comparison on issue #40 identified as the
/// one that matters: Word, LibreOffice and most recent Japanese tooling emit
/// Identity-H + ToUnicode, and it is the case `lopdf` can actually decode. The
/// glyph codes are arbitrary (1, 2, 3, …) and the CMap is what maps them back
/// to characters — so a test that gets the text back has proven the CMap was
/// read, not that the bytes happened to be legible.
///
/// `info` is `(author, title, creation_date)` for the info dictionary.
pub fn minimal_pdf(text: &str, info: Option<(&str, &str, &str)>) -> Vec<u8> {
    let chars: Vec<char> = text.chars().collect();

    // The content stream addresses each character by its 1-based index.
    let hex: String = (1..=chars.len()).map(|i| format!("{i:04X}")).collect();
    let content = format!("BT /F1 24 Tf 72 700 Td <{hex}> Tj ET\n");

    let bfchars: String = chars
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut units = String::new();
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                units.push_str(&format!("{unit:04X}"));
            }
            format!("<{:04X}> <{units}>\n", i + 1)
        })
        .collect();
    let cmap = format!(
        "/CIDInit /ProcSet findresource begin\n\
         12 dict begin\nbegincmap\n\
         /CMapName /Test-H def\n/CMapType 2 def\n\
         1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n\
         {} beginbfchar\n{bfchars}endbfchar\n\
         endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n",
        chars.len()
    );

    let mut objects: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
         /Resources << /Font << /F1 4 0 R >> >> /Contents 7 0 R >>"
            .to_string(),
        "<< /Type /Font /Subtype /Type0 /BaseFont /Test /Encoding /Identity-H \
         /DescendantFonts [5 0 R] /ToUnicode 6 0 R >>"
            .to_string(),
        "<< /Type /Font /Subtype /CIDFontType2 /BaseFont /Test \
         /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
         /FontDescriptor 8 0 R /DW 1000 >>"
            .to_string(),
        format!("<< /Length {} >>\nstream\n{cmap}endstream", cmap.len()),
        format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        ),
        "<< /Type /FontDescriptor /FontName /Test /Flags 4 /FontBBox [0 0 1000 1000] \
         /ItalicAngle 0 /Ascent 800 /Descent -200 /CapHeight 700 /StemV 80 >>"
            .to_string(),
    ];

    let trailer_info = match info {
        Some((author, title, created)) => {
            objects.push(format!(
                "<< /Author {} /Title {} /CreationDate ({created}) >>",
                pdf_text_string(author),
                pdf_text_string(title)
            ));
            format!(" /Info {} 0 R", objects.len())
        }
        None => String::new(),
    };

    assemble_pdf(&objects, &trailer_info)
}

/// A PDF with a Type0 font and **no** `ToUnicode`, i.e. the case no Rust crate
/// can decode correctly today.
///
/// It exists to prove the *failure* shape: the scan must keep going and the
/// file must be accounted for, not that the text comes back.
pub fn pdf_without_tounicode() -> Vec<u8> {
    let content = "BT /F1 24 Tf 72 700 Td <00010002> Tj ET\n";
    let objects: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
         /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>"
            .to_string(),
        "<< /Type /Font /Subtype /Type0 /BaseFont /Test /Encoding /UniJIS-UCS2-H \
         /DescendantFonts [6 0 R] >>"
            .to_string(),
        format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        ),
        "<< /Type /Font /Subtype /CIDFontType0 /BaseFont /Test \
         /CIDSystemInfo << /Registry (Adobe) /Ordering (Japan1) /Supplement 6 >> /DW 1000 >>"
            .to_string(),
    ];
    assemble_pdf(&objects, "")
}

/// Serialize numbered objects with a correct cross-reference table.
///
/// The offsets are computed while writing rather than guessed: a PDF with a
/// wrong xref is a *different* test (one about error handling) than the one
/// these fixtures are for.
fn assemble_pdf(objects: &[String], trailer_extra: &str) -> Vec<u8> {
    let mut out: Vec<u8> = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets: Vec<usize> = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let startxref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R{trailer_extra} >>\nstartxref\n{startxref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// A PDF text string: UTF-16BE behind a BOM, hex-encoded, which is the only
/// encoding that can carry Japanese.
fn pdf_text_string(s: &str) -> String {
    let mut hex = String::from("<FEFF");
    for unit in s.encode_utf16() {
        hex.push_str(&format!("{unit:04X}"));
    }
    hex.push('>');
    hex
}

// ── EXIF ────────────────────────────────────────────────────────────────────

/// A minimal JPEG carrying an EXIF APP1 segment with `Make`, `Model` and
/// `DateTimeOriginal`.
///
/// Built rather than embedded for the reason at the top of this file, and
/// little-endian because that is what the cameras this matters for write.
pub fn minimal_jpeg_with_exif(make: &str, model: &str, date_time_original: &str) -> Vec<u8> {
    // ── TIFF block ─────────────────────────────────────────────────────────
    // Layout: header (8) | IFD0 | Exif IFD | value area.
    let make_bytes = c_string(make);
    let model_bytes = c_string(model);
    let dto_bytes = c_string(date_time_original);

    const HEADER: usize = 8;
    const IFD0_ENTRIES: usize = 3;
    const EXIF_ENTRIES: usize = 1;
    let ifd0_len = 2 + IFD0_ENTRIES * 12 + 4;
    let exif_len = 2 + EXIF_ENTRIES * 12 + 4;
    let exif_ifd_offset = HEADER + ifd0_len;
    let values_offset = exif_ifd_offset + exif_len;

    let make_offset = values_offset;
    let model_offset = make_offset + make_bytes.len();
    let dto_offset = model_offset + model_bytes.len();

    let mut tiff: Vec<u8> = Vec::new();
    tiff.extend_from_slice(b"II"); // little-endian
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&(HEADER as u32).to_le_bytes());

    // IFD0: Make, Model, ExifIFDPointer. Tag numbers must ascend.
    tiff.extend_from_slice(&(IFD0_ENTRIES as u16).to_le_bytes());
    push_ascii_entry(&mut tiff, 0x010F, &make_bytes, make_offset);
    push_ascii_entry(&mut tiff, 0x0110, &model_bytes, model_offset);
    push_entry(&mut tiff, 0x8769, 4, 1, exif_ifd_offset as u32); // LONG pointer
    tiff.extend_from_slice(&0u32.to_le_bytes()); // no IFD1

    // Exif IFD: DateTimeOriginal. It has to live here — `Tag::DateTimeOriginal`
    // is defined in the Exif context, so a copy parked in IFD0 would not be
    // found and the test would be asserting on the wrong thing.
    tiff.extend_from_slice(&(EXIF_ENTRIES as u16).to_le_bytes());
    push_ascii_entry(&mut tiff, 0x9003, &dto_bytes, dto_offset);
    tiff.extend_from_slice(&0u32.to_le_bytes());

    tiff.extend_from_slice(&make_bytes);
    tiff.extend_from_slice(&model_bytes);
    tiff.extend_from_slice(&dto_bytes);

    // ── JPEG wrapper ───────────────────────────────────────────────────────
    let mut app1: Vec<u8> = b"Exif\0\0".to_vec();
    app1.extend_from_slice(&tiff);

    let mut jpeg: Vec<u8> = vec![0xFF, 0xD8]; // SOI
    jpeg.extend_from_slice(&[0xFF, 0xE1]);
    jpeg.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    jpeg.extend_from_slice(&app1);
    jpeg.extend_from_slice(&[0xFF, 0xD9]); // EOI
    jpeg
}

fn c_string(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

/// An ASCII IFD entry. Values of four bytes or fewer live inline; longer ones
/// are an offset into the value area.
fn push_ascii_entry(out: &mut Vec<u8>, tag: u16, value: &[u8], offset: usize) {
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // ASCII
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    if value.len() <= 4 {
        let mut inline = value.to_vec();
        inline.resize(4, 0);
        out.extend_from_slice(&inline);
    } else {
        out.extend_from_slice(&(offset as u32).to_le_bytes());
    }
}

fn push_entry(out: &mut Vec<u8>, tag: u16, format: u16, count: u32, value: u32) {
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&format.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
}
