//! Text-target decision and decoding for the body-extraction stage (design.md §4-2).
//!
//! The rule here is deliberately *not* "extension allowlist == index decision".
//! An extension list is only the fast entrance: it lets us index `.md` without
//! reading a byte, and reject `.pdf` / `.xlsx` / `.png` without opening them.
//! Everything else — `Makefile`, `LICENSE`, `.mts`, a `.foo` config file — falls
//! through to content sniffing, so a plain-text file never disappears just
//! because nobody thought to add its extension.
//!
//! ## Known limitations (M1)
//!
//! - Only UTF-8 (with or without BOM) is treated as text. Shift_JIS / EUC-JP /
//!   UTF-16 files are classified as binary because we do not do charset
//!   detection yet. They are counted and reported, not silently dropped.
//! - PDF / Office are intentionally out of scope for M1 (issue #2); their
//!   extensions live in [`BINARY_EXTS`] so they are reported under a distinct
//!   skip reason rather than looking like a sniffing failure.

/// Extensions accepted as text without opening the file.
///
/// This list exists for speed, not for correctness — anything missing still has
/// the sniffing path (see module docs). It deliberately includes the ESM/TSX
/// family (`mjs`, `cjs`, `jsx`, `tsx`, `mts`, `cts`) that a shorter list dropped
/// silently in an earlier measurement.
pub const TEXT_EXTS: &[&str] = &[
    // プレーンテキスト・文書
    "txt", "text", "md", "markdown", "mdx", "rst", "adoc", "asciidoc", "org", "tex", "bib", "srt",
    "vtt", "po", "pot", "man",
    // 表・ログ・データ
    "csv", "tsv", "log", "ndjson", "jsonl",
    // 設定
    "ini", "cfg", "conf", "config", "properties", "env", "editorconfig", "json", "json5", "jsonc",
    "yaml", "yml", "toml", "xml", "xsd", "xsl", "xslt", "plist", "resx", "svg",
    // Web
    "html", "htm", "xhtml", "css", "scss", "sass", "less", "vue", "svelte", "astro",
    // JS/TS 一族(mjs/cjs/jsx/tsx/mts/cts の欠落が実測で取りこぼしを生んだ)
    "js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx",
    // その他の言語
    "rs", "py", "pyi", "pyx", "go", "java", "kt", "kts", "scala", "groovy", "c", "h", "cc", "cpp",
    "cxx", "hpp", "hh", "hxx", "cs", "fs", "fsx", "vb", "rb", "erb", "rake", "php", "pl", "pm",
    "lua", "r", "jl", "swift", "dart", "ex", "exs", "erl", "hrl", "hs", "ml", "mli", "nim", "zig",
    "clj", "cljs", "elm", "coffee",
    // シェル・ビルド
    "sh", "bash", "zsh", "fish", "ps1", "psm1", "psd1", "bat", "cmd", "mk", "cmake", "gradle",
    "sbt", "bazel", "bzl",
    // クエリ・スキーマ・IaC
    "sql", "graphql", "gql", "proto", "thrift", "avsc", "tf", "tfvars", "hcl",
    // ノート・差分
    "ipynb", "patch", "diff",
];

/// Extensions rejected without opening the file.
///
/// Two groups live here: formats whose body extraction is out of M1 scope
/// (PDF / Office) and formats that have no text body at all (media, archives,
/// binaries). Both are counted under [`crate::fulltext::SkipReason::UnsupportedExt`]
/// so a user can see *why* a file is missing from the index.
pub const BINARY_EXTS: &[&str] = &[
    // M1 スコープ外の文書形式(後続 issue で本文抽出を実装する)
    "pdf", "doc", "docx", "docm", "xls", "xlsx", "xlsm", "ppt", "pptx", "pptm", "odt", "ods",
    "odp", "rtf", "epub", "mobi", "one",
    // 実行形式・オブジェクト
    "exe", "dll", "so", "dylib", "a", "lib", "o", "obj", "bin", "class", "jar", "war", "pyc",
    "pyo", "pdb", "wasm", "msi", "sys", "ko",
    // アーカイブ
    "zip", "gz", "bz2", "xz", "zst", "lz4", "7z", "rar", "tar", "tgz", "cab", "whl", "crate",
    // 画像
    "jpg", "jpeg", "png", "gif", "bmp", "ico", "webp", "tiff", "tif", "heic", "heif", "avif",
    "psd", "xcf", "raw", "cr2", "nef",
    // 音声・映像
    "mp3", "wav", "flac", "ogg", "oga", "m4a", "aac", "wma", "mp4", "m4v", "avi", "mkv", "mov",
    "wmv", "webm", "flv", "mpg", "mpeg",
    // フォント
    "ttf", "otf", "ttc", "woff", "woff2", "eot",
    // データベース・ディスクイメージ
    "db", "db-wal", "db-shm", "sqlite", "sqlite3", "mdb", "accdb", "iso", "dmg", "img", "vhd",
    "vmdk",
];

/// Number of leading bytes examined when sniffing an unknown format.
///
/// Kept at 512 so that the `magic` column of schema v0 (also 512 bytes, filled
/// by `sagasu hash`) can be reused as the sample without re-opening the file.
pub const SNIFF_LEN: usize = crate::store::MAGIC_LEN;

/// UTF-8 byte-order mark.
const BOM_UTF8: &[u8] = &[0xEF, 0xBB, 0xBF];

/// What the extension alone tells us about a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtVerdict {
    /// On the allowlist — index without reading a byte first.
    Text,
    /// On the denylist — skip without opening (out of scope or no text body).
    Binary,
    /// Neither list matched: the content has to decide (see [`sniff_is_text`]).
    Unknown,
}

/// Classify a file by extension alone.
///
/// `ext` is expected lowercased (the crawler stores it that way); the comparison
/// is case-insensitive regardless. `extra` holds user-supplied extensions that
/// extend the allowlist and always win over the denylist.
pub fn classify_ext(ext: Option<&str>, extra: &[String]) -> ExtVerdict {
    let Some(ext) = ext else {
        // No extension at all (`Makefile`, `LICENSE`, `Dockerfile`, ...).
        return ExtVerdict::Unknown;
    };

    if extra.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
        return ExtVerdict::Text;
    }
    if TEXT_EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
        return ExtVerdict::Text;
    }
    if BINARY_EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
        return ExtVerdict::Binary;
    }
    ExtVerdict::Unknown
}

/// Decide whether a leading byte sample looks like UTF-8 text.
///
/// The sample may end mid-character (it is a fixed-size prefix), so a UTF-8
/// error caused by a truncated trailing sequence is not treated as a failure.
pub fn sniff_is_text(sample: &[u8]) -> bool {
    if sample.is_empty() {
        return false;
    }

    // A UTF-8 BOM is a positive identification; UTF-16 BOMs are text we cannot
    // decode yet, and their NUL bytes would poison the index, so reject them.
    if sample.starts_with(BOM_UTF8) {
        return true;
    }
    if sample.starts_with(&[0xFF, 0xFE]) || sample.starts_with(&[0xFE, 0xFF]) {
        return false;
    }

    // A NUL byte in the first block is the classic binary tell (this is also
    // what `grep` uses).
    if sample.contains(&0) {
        return false;
    }

    // Control characters other than tab/LF/CR/FF/ESC should be rare in text.
    let control = sample
        .iter()
        .filter(|&&b| (b < 0x09) || (0x0E..0x20).contains(&b) || b == 0x7F)
        .count();
    if control * 100 > sample.len() {
        return false;
    }

    match std::str::from_utf8(sample) {
        Ok(_) => true,
        // `error_len() == None` means "unexpected end of input", i.e. the sample
        // was cut in the middle of a multi-byte character — not a real error.
        Err(e) => e.error_len().is_none() && e.valid_up_to() + 4 >= sample.len(),
    }
}

/// Decode file bytes into an indexable string.
///
/// Strips a UTF-8 BOM and replaces invalid sequences rather than failing: by the
/// time we get here the content has already been judged as text, and losing a
/// whole document over one bad byte is worse than a replacement character.
pub fn decode(bytes: &[u8]) -> String {
    let body = bytes.strip_prefix(BOM_UTF8).unwrap_or(bytes);
    String::from_utf8_lossy(body).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_allowlist_covers_the_esm_family() {
        for ext in ["mjs", "cjs", "jsx", "tsx", "mts", "cts"] {
            assert_eq!(
                classify_ext(Some(ext), &[]),
                ExtVerdict::Text,
                "{ext} must be on the allowlist"
            );
        }
    }

    #[test]
    fn office_and_pdf_are_denylisted_not_unknown() {
        for ext in ["pdf", "docx", "xlsx", "pptx"] {
            assert_eq!(classify_ext(Some(ext), &[]), ExtVerdict::Binary);
        }
    }

    #[test]
    fn unknown_extension_falls_through_to_sniffing() {
        assert_eq!(classify_ext(Some("wat"), &[]), ExtVerdict::Unknown);
        assert_eq!(classify_ext(None, &[]), ExtVerdict::Unknown);
    }

    #[test]
    fn user_extension_overrides_denylist() {
        let extra = vec!["obj".to_string()];
        assert_eq!(classify_ext(Some("obj"), &[]), ExtVerdict::Binary);
        assert_eq!(classify_ext(Some("obj"), &extra), ExtVerdict::Text);
    }

    #[test]
    fn sniff_accepts_utf8_japanese_and_rejects_nul() {
        assert!(sniff_is_text("日本語のテキスト\n".as_bytes()));
        assert!(sniff_is_text(b"#!/bin/sh\necho hi\n"));
        assert!(!sniff_is_text(b"\x7fELF\x02\x01\x01\x00\x00\x00"));
        assert!(!sniff_is_text(&[]));
    }

    #[test]
    fn sniff_tolerates_a_truncated_trailing_character() {
        let full = "あいうえお".as_bytes();
        // Cut one byte short of a character boundary.
        assert!(sniff_is_text(&full[..full.len() - 1]));
    }

    #[test]
    fn decode_strips_utf8_bom() {
        let mut bytes = BOM_UTF8.to_vec();
        bytes.extend_from_slice("見出し".as_bytes());
        assert_eq!(decode(&bytes), "見出し");
    }
}
