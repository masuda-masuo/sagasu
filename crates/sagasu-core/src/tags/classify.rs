//! Static format / kind classification: the tables that turn a leading byte
//! sample or an extension into a `format:` and a `kind:`.
//!
//! Everything here is a lookup over a literal table, which is why it is its own
//! file: the tables are the part of the tag engine that grows one row at a time
//! as formats are added (issue #15), and they grow without touching any of the
//! logic in [`super`].
//!
//! Only [`crate::text`] is consulted from outside, and only for the two
//! judgements the full-text stage already owns — the text extension allowlist
//! and the text/binary sniff — so `format:text` means exactly "this is what the
//! full-text stage would have indexed".

/// Identify a format from a leading byte sample.
///
/// Returns `None` for an empty sample: a zero-byte file is not evidence of any
/// format, and calling it "binary" would invent an anomaly out of nothing.
pub fn format_from_magic(sample: &[u8]) -> Option<&'static str> {
    if sample.is_empty() {
        return None;
    }
    let at = |off: usize, sig: &[u8]| -> bool {
        sample.len() >= off + sig.len() && &sample[off..off + sig.len()] == sig
    };

    let signatures: &[(&[u8], &str)] = &[
        (b"%PDF-", "pdf"),
        (b"\x89PNG\r\n\x1a\n", "png"),
        (b"\xFF\xD8\xFF", "jpg"),
        (b"GIF87a", "gif"),
        (b"GIF89a", "gif"),
        (b"BM", "bmp"),
        (b"II*\x00", "tiff"),
        (b"MM\x00*", "tiff"),
        (b"PK\x03\x04", "zip"),
        (b"PK\x05\x06", "zip"),
        (b"PK\x07\x08", "zip"),
        (b"\x1F\x8B", "gzip"),
        (b"BZh", "bzip2"),
        (b"\xFD7zXZ\x00", "xz"),
        (b"\x28\xB5\x2F\xFD", "zstd"),
        (b"7z\xBC\xAF\x27\x1C", "sevenzip"),
        (b"Rar!\x1A\x07", "rar"),
        (b"\x7FELF", "elf"),
        (b"MZ", "pe"),
        (b"\xFE\xED\xFA\xCE", "macho"),
        (b"\xFE\xED\xFA\xCF", "macho"),
        // Also a Mach-O universal binary header; `.class` is by far the more
        // common thing to meet in a document tree, and the extension
        // disambiguates in the cases that matter.
        (b"\xCA\xFE\xBA\xBE", "class"),
        (b"SQLite format 3\x00", "sqlite"),
        (b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1", "ole2"),
        (b"{\\rtf", "rtf"),
        (b"OggS", "ogg"),
        (b"fLaC", "flac"),
        (b"ID3", "mp3"),
        (b"\x1A\x45\xDF\xA3", "matroska"),
        (b"%!PS", "postscript"),
        (b"\x00asm", "wasm"),
        (b"wOFF", "woff"),
        (b"wOF2", "woff2"),
        (b"\x00\x01\x00\x00\x00", "ttf"),
        (b"OTTO", "otf"),
        (b"8BPS", "psd"),
        (b"\x25\x21", "postscript"),
    ];
    for (sig, name) in signatures {
        if sample.starts_with(sig) {
            return Some(name);
        }
    }

    // Container formats whose signature is not at offset 0.
    if at(0, b"RIFF") {
        if at(8, b"WAVE") {
            return Some("wav");
        }
        if at(8, b"AVI ") {
            return Some("avi");
        }
        if at(8, b"WEBP") {
            return Some("webp");
        }
        return Some("riff");
    }
    if at(4, b"ftyp") {
        if at(8, b"heic") || at(8, b"heix") || at(8, b"mif1") {
            return Some("heif");
        }
        return Some("mp4");
    }

    // UTF-16 is text we cannot decode yet (`crate::text`), but its byte-order
    // mark is a positive identification, so say what it is instead of "binary".
    if sample.starts_with(&[0xFF, 0xFE]) || sample.starts_with(&[0xFE, 0xFF]) {
        return Some("utf16");
    }
    if sample.starts_with(b"<?xml") || sample.starts_with(b"\xEF\xBB\xBF<?xml") {
        return Some("xml");
    }

    // Nothing matched: fall back to the same text/binary judgement the indexer
    // uses, so `format:text` means exactly "this is what the full-text stage
    // would have indexed".
    Some(if crate::text::sniff_is_text(sample) {
        "text"
    } else {
        "binary"
    })
}

/// Fold extension aliases so one format lands in one bucket.
pub fn fold_ext(ext: &str) -> &str {
    match ext {
        "jpeg" | "jpe" => "jpg",
        "htm" => "html",
        "yml" => "yaml",
        "tif" => "tiff",
        "mpeg" => "mpg",
        "markdown" | "mdown" => "md",
        "text" => "txt",
        "tgz" => "tar.gz",
        other => other,
    }
}

/// The format an extension implies, when it implies one at all.
#[rustfmt::skip]
pub fn format_from_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "pdf" => "pdf",
        "png" => "png", "jpg" => "jpg", "gif" => "gif", "webp" => "webp", "bmp" => "bmp",
        "tiff" => "tiff", "ico" => "ico", "heic" | "heif" => "heif", "psd" => "psd",
        "svg" => "xml",
        "docx" | "xlsx" | "pptx" | "docm" | "xlsm" | "pptm" => "ooxml",
        "doc" | "xls" | "ppt" | "msi" | "msg" => "ole2",
        "odt" | "ods" | "odp" => "opendocument",
        "rtf" => "rtf", "epub" => "zip",
        "zip" | "jar" | "war" | "whl" | "apk" => "zip",
        "gz" | "tar.gz" => "gzip", "bz2" => "bzip2", "xz" => "xz", "zst" => "zstd",
        "7z" => "sevenzip", "rar" => "rar", "tar" => "tar",
        "exe" | "dll" | "sys" => "pe", "so" => "elf", "dylib" => "macho", "class" => "class",
        "wasm" => "wasm",
        "sqlite" | "sqlite3" | "db" => "sqlite",
        "mp3" => "mp3", "flac" => "flac", "ogg" | "oga" => "ogg", "wav" => "wav",
        "mp4" | "m4v" | "m4a" => "mp4", "mkv" | "webm" => "matroska", "avi" => "avi",
        "ttf" => "ttf", "otf" => "otf", "woff" => "woff", "woff2" => "woff2",
        "ps" | "eps" => "postscript",
        "xml" | "xsd" | "xsl" | "xslt" | "plist" | "resx" => "xml",
        _ => return crate::text::TEXT_EXTS.contains(&ext).then_some("text"),
    })
}

/// Formats an extension is *expected* to have, for mismatch detection.
///
/// Only extensions whose content has a checkable binary signature appear here —
/// see the comment at the call site in [`super::tags_for`].
#[rustfmt::skip]
pub(super) fn expected_formats(ext: &str) -> Option<&'static [&'static str]> {
    Some(match ext {
        "pdf" => &["pdf"],
        "png" => &["png"], "jpg" => &["jpg"], "gif" => &["gif"], "bmp" => &["bmp"],
        "tiff" => &["tiff"], "webp" => &["webp", "riff"], "psd" => &["psd"],
        "heic" | "heif" => &["heif", "mp4"],
        // OOXML and ODF are ZIP containers; the ZIP signature is the right
        // answer for them, not a mismatch.
        "docx" | "xlsx" | "pptx" | "docm" | "xlsm" | "pptm" => &["zip", "ooxml"],
        "odt" | "ods" | "odp" => &["zip", "opendocument"],
        "doc" | "xls" | "ppt" | "msi" => &["ole2"],
        "zip" | "jar" | "war" | "whl" | "apk" | "epub" => &["zip"],
        "gz" | "tar.gz" => &["gzip"], "bz2" => &["bzip2"], "xz" => &["xz"], "zst" => &["zstd"],
        "7z" => &["sevenzip"], "rar" => &["rar"],
        "exe" | "dll" => &["pe"], "so" => &["elf"], "class" => &["class"], "wasm" => &["wasm"],
        "sqlite" | "sqlite3" => &["sqlite"],
        "mp3" => &["mp3", "binary"], "flac" => &["flac"], "ogg" | "oga" => &["ogg"],
        "wav" => &["wav", "riff"], "avi" => &["avi", "riff"],
        "mp4" | "m4v" | "m4a" => &["mp4"], "mkv" | "webm" => &["matroska"],
        "ttf" => &["ttf"], "otf" => &["otf"], "woff" => &["woff"], "woff2" => &["woff2"],
        _ => return None,
    })
}

/// Coarse category from the extension.
#[rustfmt::skip]
pub fn kind_from_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        // 文書
        "pdf" | "doc" | "docx" | "docm" | "odt" | "rtf" | "epub" | "mobi" | "pages" | "one"
        | "djvu" | "xps" | "tex" => "document",
        // 表計算
        "xls" | "xlsx" | "xlsm" | "ods" | "numbers" | "csv" | "tsv" => "spreadsheet",
        // プレゼンテーション
        "ppt" | "pptx" | "pptm" | "odp" | "key" => "presentation",
        // 画像
        "jpg" | "png" | "gif" | "bmp" | "ico" | "webp" | "tiff" | "heif" | "avif" | "svg"
        | "psd" | "xcf" | "raw" | "cr2" | "nef" => "image",
        // 音声
        "mp3" | "wav" | "flac" | "ogg" | "oga" | "m4a" | "aac" | "wma" => "audio",
        // 映像
        "mp4" | "m4v" | "avi" | "mkv" | "mov" | "wmv" | "webm" | "flv" | "mpg" => "video",
        // アーカイブ
        "zip" | "gz" | "tar.gz" | "bz2" | "xz" | "zst" | "lz4" | "7z" | "rar" | "tar" | "cab"
        | "whl" | "crate" | "jar" | "war" | "apk" => "archive",
        // 実行形式
        "exe" | "dll" | "so" | "dylib" | "a" | "lib" | "o" | "obj" | "bin" | "class" | "pyc"
        | "pyo" | "wasm" | "msi" | "sys" | "ko" => "executable",
        // フォント
        "ttf" | "otf" | "ttc" | "woff" | "woff2" | "eot" => "font",
        // データベース・ディスクイメージ
        "db" | "sqlite" | "sqlite3" | "mdb" | "accdb" | "iso" | "dmg" | "img" | "vhd"
        | "vmdk" => "database",
        // 設定
        "ini" | "cfg" | "conf" | "config" | "properties" | "env" | "editorconfig" | "json"
        | "json5" | "jsonc" | "yaml" | "toml" | "plist" | "resx" | "tf" | "tfvars"
        | "hcl" => "config",
        // データ・ログ
        "log" | "ndjson" | "jsonl" | "xml" | "xsd" | "xsl" | "xslt" | "avsc" | "parquet" => "data",
        // ノート
        "ipynb" => "notebook",
        // 平文
        "txt" | "md" | "mdx" | "rst" | "adoc" | "asciidoc" | "org" | "bib" | "srt" | "vtt"
        | "po" | "pot" | "man" | "patch" | "diff" => "text",
        other => {
            // Everything else on the text allowlist that is not covered above is
            // source code — the largest group by far, and not worth spelling out
            // extension by extension a second time.
            return crate::text::TEXT_EXTS.contains(&other).then_some("code");
        }
    })
}

/// Coarse category from a detected format, used when the extension is unknown
/// or absent.
#[rustfmt::skip]
pub(super) fn kind_from_format(format: &str) -> Option<&'static str> {
    Some(match format {
        "pdf" | "ole2" | "ooxml" | "opendocument" | "rtf" | "postscript" => "document",
        "png" | "jpg" | "gif" | "bmp" | "tiff" | "webp" | "heif" | "psd" | "ico" => "image",
        "mp3" | "flac" | "ogg" | "wav" => "audio",
        "mp4" | "matroska" | "avi" => "video",
        "zip" | "gzip" | "bzip2" | "xz" | "zstd" | "sevenzip" | "rar" | "tar" => "archive",
        "elf" | "pe" | "macho" | "class" | "wasm" => "executable",
        "ttf" | "otf" | "woff" | "woff2" => "font",
        "sqlite" => "database",
        "xml" => "data",
        "text" => "text",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use crate::tagrules::RuleSet;
    use crate::tags::{tags_for, FileFacts, TagSet, NS_ANOMALY, NS_EXT, NS_FORMAT};

    fn values(set: &TagSet, ns: &str) -> Vec<String> {
        set.tags
            .iter()
            .filter(|(t, _)| t.namespace() == ns)
            .map(|(t, _)| t.value().to_string())
            .collect()
    }

    #[test]
    fn magic_bytes_beat_the_extension_and_the_disagreement_is_tagged() {
        let mut sample = b"%PDF-1.7\n".to_vec();
        sample.extend_from_slice(&[0u8; 32]);
        let set = tags_for(
            &FileFacts {
                path: "/root/a/notes.png",
                root: Some("/root"),
                ext: Some("png"),
                magic: Some(&sample),
            },
            &RuleSet::empty(),
        );
        assert_eq!(values(&set, NS_FORMAT), vec!["pdf"]);
        assert_eq!(values(&set, NS_EXT), vec!["png"]);
        assert_eq!(values(&set, NS_ANOMALY), vec!["format-mismatch"]);
    }

    #[test]
    fn a_zip_signature_on_a_docx_is_not_an_anomaly() {
        let set = tags_for(
            &FileFacts {
                path: "/root/a/plan.docx",
                root: Some("/root"),
                ext: Some("docx"),
                magic: Some(b"PK\x03\x04\x14\x00\x06\x00"),
            },
            &RuleSet::empty(),
        );
        assert_eq!(values(&set, NS_FORMAT), vec!["zip"]);
        assert!(values(&set, NS_ANOMALY).is_empty());
    }
}
