//! Embedded document formats: body extraction and embedded metadata (issue #40).
//!
//! Two things that look unrelated live here because they need the same three
//! parsers:
//!
//! - **Body extraction** for `docx` / `xlsx` / `pptx` / `pdf`, feeding the
//!   full-text stage of design.md §4-2. Before this module those extensions sat
//!   on the denylist in [`crate::text`] and were reported as
//!   `unsupported format`.
//! - **Embedded metadata** — OOXML `docProps/core.xml`, the PDF info
//!   dictionary, EXIF — feeding the [`crate::tags::FileFacts`] boundary of
//!   design.md §6-1.
//!
//! ## One decision point
//!
//! Nothing outside this module maps an extension to a parser.
//! [`BodyFormat::from_ext`] is the only place that knows `.docx` is a ZIP of
//! XML, and it is called from exactly one place — the extension verdict in
//! [`crate::text::TextPolicy::classify_ext`]. The equivalent for the tag side is
//! [`MetaFormat::from_ext`]. A format is added by editing those two functions,
//! not by growing a table in a caller.
//!
//! ## Panics are per-file errors
//!
//! Every function here is handed an arbitrary user file. `lopdf` returns
//! `Result` rather than panicking on the malformed input that made `pdf-extract`
//! unusable for this project (see the dependency comparison on issue #40), but
//! "returns Result" is a property of the code today, not a guarantee, and a
//! single bad file must never take down a scan of a whole disk. So every entry
//! point wraps its parser in [`std::panic::catch_unwind`] and turns a panic into
//! an ordinary `Err` **carrying the panic message**. It is recorded and
//! reported, not swallowed: the caller counts it as that file's error and keeps
//! going.
//!
//! The panic hook still prints its usual line to stderr — this module
//! deliberately does not install a global hook, because suppressing panics
//! process-wide from a library is a much larger promise than "this one call was
//! isolated".
//!
//! ## Features
//!
//! `office` / `pdf` / `exif`, all on by default. With one off, the format's
//! variants do not exist, `from_ext` never yields them, and the extensions fall
//! back through to the built-in denylist — the pre-#40 behaviour, counted rather
//! than silently missing.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

use anyhow::{anyhow, Result};

#[cfg(feature = "exif")]
mod exifmeta;
#[cfg(feature = "office")]
mod office;
#[cfg(feature = "pdf")]
mod pdf;

// ── Body extraction ─────────────────────────────────────────────────────────

/// A format this build can extract a text body from.
///
/// The variants are feature-gated rather than the functions behind them: with
/// `office` off there is no `Docx` to construct, so the "what happens when the
/// feature is off" question is answered by the type system instead of by a
/// runtime branch that could drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BodyFormat {
    /// WordprocessingML (`.docx`, `.docm`).
    #[cfg(feature = "office")]
    Docx,
    /// SpreadsheetML (`.xlsx`, `.xlsm`).
    #[cfg(feature = "office")]
    Xlsx,
    /// PresentationML (`.pptx`, `.pptm`).
    #[cfg(feature = "office")]
    Pptx,
    /// Portable Document Format.
    #[cfg(feature = "pdf")]
    Pdf,
}

impl BodyFormat {
    /// The one place an extension becomes an extractor.
    ///
    /// `ext` is expected lowercased (the crawler stores it that way); the
    /// comparison is case-insensitive regardless.
    pub fn from_ext(ext: Option<&str>) -> Option<Self> {
        let ext = ext?;
        #[cfg(feature = "office")]
        {
            // The macro-enabled variants are the same container with a
            // different content type, so they extract identically.
            if eq(ext, "docx") || eq(ext, "docm") {
                return Some(BodyFormat::Docx);
            }
            if eq(ext, "xlsx") || eq(ext, "xlsm") {
                return Some(BodyFormat::Xlsx);
            }
            if eq(ext, "pptx") || eq(ext, "pptm") {
                return Some(BodyFormat::Pptx);
            }
        }
        #[cfg(feature = "pdf")]
        {
            if eq(ext, "pdf") {
                return Some(BodyFormat::Pdf);
            }
        }
        let _ = ext;
        None
    }

    /// Stable label used in reports and error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            #[cfg(feature = "office")]
            BodyFormat::Docx => "docx",
            #[cfg(feature = "office")]
            BodyFormat::Xlsx => "xlsx",
            #[cfg(feature = "office")]
            BodyFormat::Pptx => "pptx",
            #[cfg(feature = "pdf")]
            BodyFormat::Pdf => "pdf",
        }
    }
}

impl std::fmt::Display for BodyFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Extract the indexable text body of `path`.
///
/// `max_bytes` is the caller's existing body-extraction budget
/// ([`crate::fulltext::FulltextConfig::max_size`]); the result is truncated to
/// it and the decompressed input is bounded by it too, so an OOXML or PDF
/// stream that expands a thousandfold cannot turn one file into an
/// out-of-memory kill.
///
/// # Errors
///
/// Returns an error when the container is unreadable, the expected part is
/// missing, or the parser fails — **including when it panics**, which is caught
/// and converted here (see the module docs).
pub fn extract_body(path: &Path, format: BodyFormat, max_bytes: u64) -> Result<String> {
    isolate(format.as_str(), || {
        extract_body_inner(path, format, max_bytes)
    })
}

fn extract_body_inner(path: &Path, format: BodyFormat, max_bytes: u64) -> Result<String> {
    // With every parser feature off, `BodyFormat` has no variants: the match
    // below is empty, and these two arguments are unused rather than the code
    // being wrong. Naming them here says so once instead of underscoring the
    // parameters and losing the documentation on them.
    let _ = (path, max_bytes);
    match format {
        #[cfg(feature = "office")]
        BodyFormat::Docx => office::docx_body(path, max_bytes),
        #[cfg(feature = "office")]
        BodyFormat::Xlsx => office::xlsx_body(path, max_bytes),
        #[cfg(feature = "office")]
        BodyFormat::Pptx => office::pptx_body(path, max_bytes),
        #[cfg(feature = "pdf")]
        BodyFormat::Pdf => pdf::body(path, max_bytes),
    }
}

// ── Embedded metadata ───────────────────────────────────────────────────────

/// A format this build can read embedded metadata from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MetaFormat {
    /// Any OOXML package — the properties live in `docProps/core.xml`
    /// regardless of which of docx / xlsx / pptx it is.
    #[cfg(feature = "office")]
    Ooxml,
    /// The PDF trailer's info dictionary.
    #[cfg(feature = "pdf")]
    Pdf,
    /// EXIF in a JPEG / TIFF / HEIF / PNG / WebP container.
    #[cfg(feature = "exif")]
    Exif,
}

impl MetaFormat {
    /// The one place an extension becomes a metadata reader.
    pub fn from_ext(ext: Option<&str>) -> Option<Self> {
        let ext = ext?;
        #[cfg(feature = "office")]
        {
            for known in ["docx", "docm", "xlsx", "xlsm", "pptx", "pptm"] {
                if eq(ext, known) {
                    return Some(MetaFormat::Ooxml);
                }
            }
        }
        #[cfg(feature = "pdf")]
        {
            if eq(ext, "pdf") {
                return Some(MetaFormat::Pdf);
            }
        }
        #[cfg(feature = "exif")]
        {
            // The containers `kamadak-exif` can find an APP1/Exif block in.
            for known in [
                "jpg", "jpeg", "tif", "tiff", "heic", "heif", "avif", "png", "webp",
            ] {
                if eq(ext, known) {
                    return Some(MetaFormat::Exif);
                }
            }
        }
        let _ = ext;
        None
    }

    /// Stable label used in reports and error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            #[cfg(feature = "office")]
            MetaFormat::Ooxml => "ooxml",
            #[cfg(feature = "pdf")]
            MetaFormat::Pdf => "pdf",
            #[cfg(feature = "exif")]
            MetaFormat::Exif => "exif",
        }
    }
}

/// What a file says about itself from the inside.
///
/// Deliberately a small, closed set rather than a property bag: every field
/// here becomes a tag namespace, and a namespace nobody can name is a facet
/// axis nobody can use. Values are cleaned (whitespace collapsed, empties
/// dropped) and `authors` is sorted and deduplicated **here**, so that
/// [`crate::tags::tags_for`] stays a pure function of its inputs and two runs
/// over the same file cannot produce two different tag orders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbeddedMeta {
    /// People the document names: `dc:creator`, `cp:lastModifiedBy`, PDF
    /// `/Author`. Sorted and deduplicated.
    pub authors: Vec<String>,
    /// `dc:title` / PDF `/Title`.
    pub title: Option<String>,
    /// EXIF `Make` + `Model`, joined with a space.
    pub camera: Option<String>,
    /// The date the document claims for itself, as `YYYY-MM-DD`: EXIF
    /// `DateTimeOriginal`, `dcterms:created`, PDF `/CreationDate`.
    ///
    /// Not `mtime` — that moves when a file is merely touched, which is exactly
    /// the non-determinism design.md §6-1 keeps out of the tag engine.
    pub date: Option<String>,
}

impl EmbeddedMeta {
    /// Whether the file carried nothing usable.
    pub fn is_empty(&self) -> bool {
        self.authors.is_empty()
            && self.title.is_none()
            && self.camera.is_none()
            && self.date.is_none()
    }

    /// Add an author, ignoring blanks. Order is fixed by [`Self::finish`].
    #[allow(dead_code)] // No caller when every parser feature is off.
    fn push_author(&mut self, raw: &str) {
        if let Some(v) = clean(raw) {
            self.authors.push(v);
        }
    }

    /// Canonicalize: collapse duplicate authors and fix their order.
    fn finish(mut self) -> Self {
        self.authors.sort();
        self.authors.dedup();
        self
    }
}

/// Read the embedded metadata of `path`.
///
/// A file of the right type that simply carries no metadata is **not** an
/// error: it returns an empty [`EmbeddedMeta`]. Most JPEGs have no EXIF and
/// most `.docx` files have an empty `dc:title`; counting those as failures
/// would bury the handful of genuinely broken files under them.
///
/// # Errors
///
/// Returns an error when the container itself is unreadable or malformed,
/// including a parser panic (see the module docs).
pub fn extract_meta(path: &Path, format: MetaFormat) -> Result<EmbeddedMeta> {
    isolate(format.as_str(), || extract_meta_inner(path, format)).map(EmbeddedMeta::finish)
}

fn extract_meta_inner(path: &Path, format: MetaFormat) -> Result<EmbeddedMeta> {
    // Same as `extract_body_inner`: an empty `MetaFormat` makes this unused.
    let _ = path;
    match format {
        #[cfg(feature = "office")]
        MetaFormat::Ooxml => office::core_properties(path),
        #[cfg(feature = "pdf")]
        MetaFormat::Pdf => pdf::meta(path),
        #[cfg(feature = "exif")]
        MetaFormat::Exif => exifmeta::meta(path),
    }
}

// ── Shared helpers ──────────────────────────────────────────────────────────

/// Run `f`, converting a panic into an `Err` that names the format and repeats
/// the panic message.
fn isolate<T>(what: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => Err(anyhow!(
            "the {what} parser panicked: {}",
            panic_message(payload.as_ref())
        )),
    }
}

/// Best-effort text of a panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Case-insensitive extension comparison.
#[allow(dead_code)]
fn eq(ext: &str, known: &str) -> bool {
    ext.eq_ignore_ascii_case(known)
}

/// Collapse whitespace and drop the value if nothing is left.
///
/// Metadata strings come from other people's software and routinely arrive with
/// newlines, NBSPs and trailing spaces in them. A tag value carrying a newline
/// is rejected by [`crate::tags::Tag::new`], so cleaning here is what keeps a
/// perfectly good `author:` from disappearing on a technicality.
#[allow(dead_code)]
fn clean(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut space = false;
    for c in raw.chars() {
        if c.is_whitespace() || c.is_control() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
            }
            space = false;
            out.push(c);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Take the leading `YYYY-MM-DD` of a timestamp written in any of the three
/// shapes this module meets: ISO 8601 (`dcterms:created`), EXIF
/// (`2024:03:15 10:11:12`) and PDF (`D:20240315101112+09'00'`).
///
/// Returns `None` unless a plausible calendar date can be read out, so a
/// placeholder like `0000:00:00 00:00:00` — which cameras really do write —
/// does not become `date:0000`.
#[allow(dead_code)]
fn iso_date(raw: &str) -> Option<String> {
    let digits: Vec<char> = raw
        .trim()
        .trim_start_matches("D:")
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == ':' || *c == ' ')
        .filter(|c| c.is_ascii_digit())
        .take(8)
        .collect();
    if digits.len() < 8 {
        return None;
    }
    let s: String = digits.into_iter().collect();
    let year: u32 = s[0..4].parse().ok()?;
    let month: u32 = s[4..6].parse().ok()?;
    let day: u32 = s[6..8].parse().ok()?;
    if !(1970..=2099).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

/// A bounded text accumulator.
///
/// The cap is the caller's body-extraction budget. Hitting it stops the parse
/// rather than failing it: a 300-page PDF whose first two megabytes are indexed
/// is a better answer than no document at all, and it is the same trade-off the
/// plain-text path already makes by refusing files over the limit outright.
#[allow(dead_code)]
#[derive(Debug)]
struct Sink {
    out: String,
    cap: usize,
    /// True once the cap stopped an append.
    full: bool,
}

#[allow(dead_code)]
impl Sink {
    fn new(cap: u64) -> Self {
        Self {
            out: String::new(),
            cap: usize::try_from(cap).unwrap_or(usize::MAX),
            full: false,
        }
    }

    /// Append text, truncating at a character boundary when the cap is reached.
    fn push(&mut self, s: &str) {
        if self.full {
            return;
        }
        let room = self.cap.saturating_sub(self.out.len());
        if s.len() <= room {
            self.out.push_str(s);
            return;
        }
        let mut end = room.min(s.len());
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        self.out.push_str(&s[..end]);
        self.full = true;
    }

    /// Separate two cells / runs without stacking separators.
    fn separator(&mut self, c: char) {
        if self.out.is_empty() || self.out.ends_with(['\n', '\t', ' ']) {
            return;
        }
        let mut buf = [0u8; 4];
        self.push(c.encode_utf8(&mut buf));
    }

    /// End the current line, dropping the cell separator that would otherwise
    /// be left dangling at the end of a spreadsheet row.
    fn newline(&mut self) {
        while self.out.ends_with([' ', '\t']) {
            self.out.pop();
        }
        if self.out.is_empty() || self.out.ends_with('\n') {
            return;
        }
        self.push("\n");
    }

    /// The accumulated text with trailing whitespace removed.
    ///
    /// Documents end with structure, not content — a closing paragraph, an
    /// empty last row — and the separators that structure produced are not part
    /// of the body. Trimming here rather than at each call site is what keeps
    /// the same file from producing two different `text_bytes` figures
    /// depending on which extractor read it.
    fn finish(mut self) -> String {
        let end = self.out.trim_end().len();
        self.out.truncate(end);
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panicking_parser_becomes_an_error_not_a_dead_process() {
        let err = isolate("test", || -> Result<()> { panic!("boom") }).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("test"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }

    #[test]
    fn clean_collapses_whitespace_and_rejects_blanks() {
        assert_eq!(clean("  増田 \n 太郎 "), Some("増田 太郎".to_string()));
        assert_eq!(clean("   \n\t "), None);
    }

    #[test]
    fn iso_date_reads_all_three_timestamp_shapes() {
        assert_eq!(
            iso_date("2024-03-15T01:02:03Z").as_deref(),
            Some("2024-03-15")
        );
        assert_eq!(
            iso_date("2024:03:15 10:11:12").as_deref(),
            Some("2024-03-15")
        );
        assert_eq!(
            iso_date("D:20240315101112+09'00'").as_deref(),
            Some("2024-03-15")
        );
    }

    #[test]
    fn iso_date_rejects_the_zero_placeholder_cameras_write() {
        assert_eq!(iso_date("0000:00:00 00:00:00"), None);
        assert_eq!(iso_date(""), None);
        assert_eq!(iso_date("not a date"), None);
    }

    #[test]
    fn sink_truncates_at_a_character_boundary() {
        let mut sink = Sink::new(7);
        sink.push("日本語です");
        assert!(sink.full);
        // 6 bytes = two characters; the third would cross the cap.
        assert_eq!(sink.finish(), "日本");
    }

    #[test]
    fn sink_does_not_stack_separators() {
        let mut sink = Sink::new(64);
        sink.separator('\t');
        sink.push("a");
        sink.separator('\t');
        sink.separator('\t');
        sink.newline();
        sink.newline();
        sink.push("b");
        // The dangling cell separator is dropped by the line break: a row that
        // ends with a tab would otherwise put one in the indexed body of every
        // spreadsheet row ever written.
        assert_eq!(sink.finish(), "a\nb");
    }

    #[test]
    fn an_unknown_extension_maps_to_no_extractor() {
        assert_eq!(BodyFormat::from_ext(Some("md")), None);
        assert_eq!(BodyFormat::from_ext(None), None);
        assert_eq!(MetaFormat::from_ext(Some("md")), None);
    }

    #[cfg(feature = "office")]
    #[test]
    fn the_macro_enabled_office_variants_map_to_the_same_extractor() {
        assert_eq!(BodyFormat::from_ext(Some("docm")), Some(BodyFormat::Docx));
        assert_eq!(BodyFormat::from_ext(Some("XLSX")), Some(BodyFormat::Xlsx));
        assert_eq!(MetaFormat::from_ext(Some("pptm")), Some(MetaFormat::Ooxml));
    }
}
