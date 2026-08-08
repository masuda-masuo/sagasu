//! PDF body text and info dictionary, via `lopdf` with default features off.
//!
//! ## Why `lopdf` and not `pdf-extract`
//!
//! Measured on issue #40: `pdf-extract` 0.12 **panics** on a Japanese PDF whose
//! Type0 font uses any predefined CMap other than `Identity-H` / `Identity-V`
//! (`UniJIS-UCS2-H`, `90ms-RKSJ-H`, …), which is what real Japanese tooling
//! emits. For a program that walks an entire disk, "panics on some inputs" is
//! not a quality trade-off, it is a crash. `lopdf` returns `Result` instead. It
//! is still called under [`super::isolate`], because "returns `Result` today"
//! is not a guarantee.
//!
//! The known limitation is inherited rather than chosen: a PDF with no
//! `ToUnicode` map cannot be decoded correctly by any Rust crate today. Those
//! files come back as mojibake or empty, and an empty body is counted as a skip
//! by the caller rather than passed off as an indexed document.
//!
//! ## Default features are off on purpose
//!
//! `lopdf`'s defaults are `chrono` + `jiff` + `time` + `rayon` — three date
//! libraries and a thread pool for a crate we only ask to decode text. The
//! workspace manifest pins `default-features = false`, and the PR's smoke run
//! greps `cargo tree` to prove none of them came back.

use std::path::Path;

use anyhow::{anyhow, Result};
use lopdf::{Document, Object};

use super::{clean, iso_date, EmbeddedMeta, Sink};

/// Ceiling on decompressed content streams, derived from the caller's text
/// budget. `lopdf` takes this directly, which is the whole defence against a
/// PDF whose content stream inflates to gigabytes.
const STREAM_INFLATION: u64 = 16;

fn load(path: &Path) -> Result<Document> {
    Document::load(path).map_err(|e| anyhow!("failed to parse PDF: {e}"))
}

/// Text of every page, in page order.
pub(super) fn body(path: &Path, max_bytes: u64) -> Result<String> {
    let doc = load(path)?;
    // `get_pages` is a BTreeMap, so this is page order and not object order —
    // two runs over the same file must produce the same body.
    let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
    if pages.is_empty() {
        return Err(anyhow!("PDF has no pages"));
    }
    let limit = usize::try_from(max_bytes.saturating_mul(STREAM_INFLATION)).unwrap_or(usize::MAX);
    let text = doc
        .extract_text_with_limit(&pages, limit)
        .map_err(|e| anyhow!("failed to extract PDF text: {e}"))?;
    let text = remove_kerning_spaces(&text);

    let mut sink = Sink::new(max_bytes);
    sink.push(&text);
    Ok(sink.finish())
}

/// Whether `c` is a CJK character in the exact set this rule applies to.
///
/// The six ranges are the CJK script blocks a Japanese document actually
/// mixes: Hiragana, Katakana, CJK Symbols and Punctuation, the two CJK
/// Unified Ideograph blocks, and Halfwidth/Fullwidth Forms. Deliberately
/// nothing wider — the CJK Compatibility blocks are excluded, because widening
/// the set widens the chance of eating a genuine boundary.
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3000}'..='\u{303F}'   // CJK Symbols and Punctuation
        | '\u{3040}'..='\u{309F}' // Hiragana
        | '\u{30A0}'..='\u{30FF}' // Katakana
        | '\u{3400}'..='\u{4DBF}' // CJK Unified Ideographs Extension A
        | '\u{4E00}'..='\u{9FFF}' // CJK Unified Ideographs
        | '\u{FF00}'..='\u{FFEF}' // Halfwidth and Fullwidth Forms
    )
}

/// Whether the space between `left` and `right` is a kerning artifact.
///
/// `lopdf` emits an ASCII space for a `TJ` kerning adjustment past its
/// threshold, assuming a large glyph gap means a word boundary. That holds for
/// Latin but not for Japanese, which is kerned between individual characters —
/// so the space lands *inside* a word, and the word becomes unreachable (a
/// split word is found by neither half, which is silence rather than a worse
/// ranking). Both neighbours must be CJK: next to Latin, digits or punctuation
/// the space may well be genuine.
///
/// Joining is the safe direction, and that is the justification for a
/// heuristic that is knowingly not always right. If we wrongly join two
/// genuinely separate Japanese words (`設定 変更` → `設定変更`), Lindera still
/// segments the compound and both halves remain searchable. If we leave a
/// wrongly split word, neither half of it retrieves the document.
fn is_kerning_artifact_space(left: char, right: char) -> bool {
    is_cjk(left) && is_cjk(right)
}

/// Drop ASCII spaces (`U+0020`) whose neighbours on both sides are CJK, in the
/// PDF extraction path only.
///
/// Only `U+0020` is touched, and a run of them collapses away entirely.
/// Newlines and tabs are structure and stay untouched — page and line
/// boundaries are real. A space adjacent to Latin, digits or punctuation is
/// left alone, because there it may well be genuine.
fn remove_kerning_spaces(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ' ' {
            let mut end = i;
            while end < chars.len() && chars[end] == ' ' {
                end += 1;
            }
            let left = i.checked_sub(1).map(|p| chars[p]).unwrap_or('\0');
            let right = chars.get(end).copied().unwrap_or('\0');
            if !is_kerning_artifact_space(left, right) {
                out.extend(std::iter::repeat_n(' ', end - i));
            }
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// The trailer's info dictionary: `/Author`, `/Title`, `/CreationDate`.
pub(super) fn meta(path: &Path) -> Result<EmbeddedMeta> {
    let doc = load(path)?;
    let mut meta = EmbeddedMeta::default();

    // A PDF with no `/Info` is normal (a linearized web PDF often has none) and
    // is not a parse failure.
    let Ok(info_ref) = doc.trailer.get(b"Info") else {
        return Ok(meta);
    };
    let Ok(info) = doc.dereference(info_ref) else {
        return Ok(meta);
    };
    let Ok(dict) = info.1.as_dict() else {
        return Ok(meta);
    };

    if let Some(author) = dict.get(b"Author").ok().and_then(decode_string) {
        meta.push_author(&author);
    }
    if let Some(title) = dict.get(b"Title").ok().and_then(decode_string) {
        meta.title = clean(&title);
    }
    if let Some(created) = dict.get(b"CreationDate").ok().and_then(decode_string) {
        meta.date = iso_date(&created);
    }
    Ok(meta)
}

/// Decode a PDF text string.
///
/// Two encodings are legal and both appear in the wild: UTF-16BE behind a byte
/// order mark, and PDFDocEncoding otherwise. PDFDocEncoding agrees with
/// Latin-1 over the range document metadata actually uses, so the fallback maps
/// bytes to code points — which is also what keeps an ASCII author name from
/// being thrown away because a stray high byte broke a UTF-8 decode.
fn decode_string(object: &Object) -> Option<String> {
    let bytes = match object {
        Object::String(bytes, _) => bytes,
        _ => return None,
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16(&units).ok();
    }
    // Producers that write UTF-8 without a BOM are out of spec but common.
    if let Ok(s) = std::str::from_utf8(bytes) {
        return Some(s.to_string());
    }
    Some(bytes.iter().map(|&b| b as char).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16be_with_a_bom_round_trips_japanese() {
        let mut bytes = vec![0xFE, 0xFF];
        for unit in "増田 太郎".encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        let obj = Object::String(bytes, lopdf::StringFormat::Literal);
        assert_eq!(decode_string(&obj).as_deref(), Some("増田 太郎"));
    }

    #[test]
    fn a_latin1_string_without_a_bom_is_not_discarded() {
        let obj = Object::String(
            vec![0x4D, 0x61, 0x73, 0x75, 0x64, 0x61],
            lopdf::StringFormat::Literal,
        );
        assert_eq!(decode_string(&obj).as_deref(), Some("Masuda"));
        // A byte that is not valid UTF-8 falls back rather than returning None.
        let obj = Object::String(vec![0xC9, 0x74, 0xE9], lopdf::StringFormat::Literal);
        assert_eq!(decode_string(&obj).as_deref(), Some("Été"));
    }

    #[test]
    fn a_space_between_cjk_characters_is_a_kerning_artifact() {
        assert!(is_kerning_artifact_space('ッ', 'プ')); // Katakana
        assert!(is_kerning_artifact_space('設', '定')); // CJK ideographs
        assert!(is_kerning_artifact_space('の', '設')); // Hiragana + ideograph
        assert!(is_kerning_artifact_space('。', '設')); // CJK punctuation
    }

    #[test]
    fn a_space_next_to_latin_or_a_digit_is_genuine() {
        assert!(!is_kerning_artifact_space('l', 'w')); // Latin both sides
        assert!(!is_kerning_artifact_space('設', 'A'));
        assert!(!is_kerning_artifact_space('A', '設'));
        assert!(!is_kerning_artifact_space('1', '設'));
        assert!(!is_kerning_artifact_space('設', '2'));
        assert!(!is_kerning_artifact_space('2', '0'));
    }

    #[test]
    fn remove_kerning_spaces_joins_cjk_and_keeps_real_spaces() {
        // The issue-#66 shape: a kerned `TJ` array split the word here.
        assert_eq!(remove_kerning_spaces("バックアッ プの設定"), "バックアップの設定");
        // Two genuinely separate words join, and stay searchable via Lindera's
        // compound segmentation — the asymmetry that makes joining the safe
        // direction.
        assert_eq!(remove_kerning_spaces("設定 変更"), "設定変更");
        assert_eq!(remove_kerning_spaces("hello world"), "hello world");
        assert_eq!(remove_kerning_spaces("設定 A"), "設定 A");
        assert_eq!(remove_kerning_spaces("A 設定"), "A 設定");
        assert_eq!(remove_kerning_spaces("2024 設定"), "2024 設定");
        assert_eq!(remove_kerning_spaces("設定 2"), "設定 2");
        assert_eq!(remove_kerning_spaces("a 設定 変更 b"), "a 設定変更 b");
    }

    #[test]
    fn a_run_of_spaces_between_cjk_collapses_entirely() {
        assert_eq!(remove_kerning_spaces("バックアッ  プ"), "バックアップ");
        assert_eq!(remove_kerning_spaces("バックアッ   プ"), "バックアップ");
        // A run next to Latin is not an artifact: nothing is dropped.
        assert_eq!(remove_kerning_spaces("a   b"), "a   b");
    }

    #[test]
    fn a_newline_between_cjk_is_structure_not_kerning() {
        assert_eq!(remove_kerning_spaces("設定\n変更"), "設定\n変更");
        assert_eq!(remove_kerning_spaces("設定\t変更"), "設定\t変更");
    }
}
