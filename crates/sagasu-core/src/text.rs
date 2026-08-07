//! Text-target decision and decoding for the body-extraction stage (design.md §4-2).
//!
//! The rule here is deliberately *not* "extension allowlist == index decision".
//! An extension list is only the fast entrance: it lets us index `.md` without
//! reading a byte, and reject `.pdf` / `.xlsx` / `.png` without opening them.
//! Everything else — `Makefile`, `LICENSE`, `.mts`, a `.foo` config file — falls
//! through to content sniffing, so a plain-text file never disappears just
//! because nobody thought to add its extension.
//!
//! ## The fourth verdict: extract (issue #40)
//!
//! `docx` / `xlsx` / `pptx` / `pdf` are neither "read the bytes" nor "give up".
//! They get [`ExtVerdict::Extract`], which names the parser in
//! [`crate::docmeta`] that turns the file into text. The decision still happens
//! *here*, in one function, so there is exactly one place that maps an
//! extension to what will be done with the file.
//!
//! Those extensions remain in [`BINARY_EXTS`]. That is not redundancy: with the
//! `office` / `pdf` features off, [`crate::docmeta::BodyFormat::from_ext`]
//! yields nothing and the lookup falls straight through to the denylist, so a
//! build without the parsers behaves exactly as it did before this feature
//! existed — counted under `unsupported format`, never silently missing.
//!
//! ## Known limitations
//!
//! - Only UTF-8 (with or without BOM) is treated as text. Shift_JIS / EUC-JP /
//!   UTF-16 files are classified as binary because we do not do charset
//!   detection yet. They are counted and reported, not silently dropped.
//! - Legacy binary Office (`.doc` / `.xls` / `.ppt`), OpenDocument and `.rtf`
//!   are still denylisted: they are different formats, not different
//!   extensions for the ones above.

/// Extensions accepted as text without opening the file.
///
/// This list exists for speed, not for correctness — anything missing still has
/// the sniffing path (see module docs). It deliberately includes the ESM/TSX
/// family (`mjs`, `cjs`, `jsx`, `tsx`, `mts`, `cts`) that a shorter list dropped
/// silently in an earlier measurement.
///
/// `rustfmt::skip`: the entries are grouped by category behind comments, and
/// rustfmt would put each of the ~180 extensions on its own line, destroying
/// both the grouping and any chance of reviewing an addition at a glance.
#[rustfmt::skip]
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
/// Formats with no text body at all (media, archives, binaries), plus the
/// document formats nothing in this crate can read (legacy binary Office,
/// OpenDocument, `.rtf`, `.epub`). They are counted under
/// [`crate::fulltext::SkipReason::UnsupportedExt`] so a user can see *why* a
/// file is missing from the index.
///
/// The OOXML and PDF extensions are still listed: they are reached first by the
/// [`ExtVerdict::Extract`] lookup when the parsers are compiled in, and this is
/// where they land when they are not.
///
/// Grouped and `rustfmt::skip`ped for the same reason as [`TEXT_EXTS`].
#[rustfmt::skip]
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
    /// A document format with a body behind a parser (issue #40). The variant
    /// carries which parser, so the caller never re-derives it from the
    /// extension.
    Extract(crate::docmeta::BodyFormat),
    /// On the denylist — skip without opening (out of scope or no text body).
    Binary,
    /// Neither list matched: the content has to decide (see [`sniff_is_text`]).
    Unknown,
}

// ── User-extensible policy ──────────────────────────────────────────────────

/// The file this policy used to be read from, before the two config files were
/// merged into one (issue #6, docs/cli.md §5).
///
/// Kept as a constant because it is still *looked for*: a `sagasu-text.toml`
/// left in the working directory is no longer read, and saying so is the whole
/// point — see [`crate::config::check_no_legacy_config`].
pub const LEGACY_TEXT_CONFIG_FILE: &str = "sagasu-text.toml";

/// On-disk shape of the `[text]` section of `sagasu.toml`. Unknown keys are an
/// error for the same reason [`crate::tagrules`] rejects them: a file where
/// `text_exts` was typed instead of `text_ext` must not load as a config that
/// does nothing.
///
/// Lives here rather than in [`crate::config`] so the shape sits next to the
/// type it configures; `config` composes it with the tag-rule section into the
/// one file both halves are read from (docs/cli.md §5).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TextSection {
    #[serde(default)]
    pub(crate) text_ext: Vec<String>,
    #[serde(default)]
    pub(crate) binary_ext: Vec<String>,
}

/// The extension half of the body-extraction decision, plus the user's
/// additions to it.
///
/// The built-in lists are long but they will always be a snapshot of the
/// formats someone thought of. This type is how a user says "`.tmpl` is text
/// here" without waiting for a release — issue #15's requirement that the
/// allowlist be extensible, and the reason the sniffing path is a safety net
/// rather than the only escape hatch.
///
/// Precedence, highest first:
///
/// 1. user text extensions — they override everything, including the built-in
///    denylist, because the user is looking at the files and we are not;
/// 2. user binary extensions;
/// 3. the built-in [`TEXT_EXTS`] allowlist;
/// 4. the built-in [`BINARY_EXTS`] denylist;
/// 5. otherwise [`ExtVerdict::Unknown`] — the content decides.
#[derive(Debug, Clone, Default)]
pub struct TextPolicy {
    text_ext: Vec<String>,
    binary_ext: Vec<String>,
    source: Option<std::path::PathBuf>,
    digest: Option<String>,
}

impl TextPolicy {
    /// The built-in lists with nothing added.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Add extensions to the allowlist (what `--ext` on the command line does).
    /// A leading dot is tolerated so `--ext .mjs` behaves like `--ext mjs`.
    pub fn add_text_exts(&mut self, exts: &[String]) {
        for e in exts {
            let e = normalize_ext(e);
            if !e.is_empty() && !self.text_ext.iter().any(|x| x.eq_ignore_ascii_case(&e)) {
                self.text_ext.push(e);
            }
        }
    }

    /// Add extensions to the denylist.
    pub fn add_binary_exts(&mut self, exts: &[String]) {
        for e in exts {
            let e = normalize_ext(e);
            if !e.is_empty() && !self.binary_ext.iter().any(|x| x.eq_ignore_ascii_case(&e)) {
                self.binary_ext.push(e);
            }
        }
    }

    /// Load a text config file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or is not valid TOML with
    /// only the known keys.
    pub fn load(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        use anyhow::Context;
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read text config {}", path.display()))?;
        let digest = blake3::hash(text.as_bytes()).to_hex().to_string();
        let mut policy = Self::parse(&text)
            .with_context(|| format!("invalid text config in {}", path.display()))?;
        policy.source = Some(path.to_path_buf());
        policy.digest = Some(digest);
        Ok(policy)
    }

    /// Compile a policy from TOML text (the testable half of [`TextPolicy::load`]).
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let section: TextSection = toml::from_str(text)?;
        Ok(Self::from_section(section))
    }

    /// Build a policy from an already-deserialized `[text]` section.
    pub(crate) fn from_section(section: TextSection) -> Self {
        let mut policy = Self::empty();
        policy.add_text_exts(&section.text_ext);
        policy.add_binary_exts(&section.binary_ext);
        policy
    }

    /// Record which file this policy came from, and the digest of its bytes.
    ///
    /// [`TextPolicy::load`] does this for itself; [`crate::config`] needs it
    /// because it reads one file for two policies and the bytes are hashed once.
    pub(crate) fn with_origin(mut self, source: std::path::PathBuf, digest: String) -> Self {
        self.source = Some(source);
        self.digest = Some(digest);
        self
    }

    /// Extensions the user added to the allowlist.
    pub fn text_exts(&self) -> &[String] {
        &self.text_ext
    }

    /// Extensions the user added to the denylist.
    pub fn binary_exts(&self) -> &[String] {
        &self.binary_ext
    }

    /// Whether the user added anything at all.
    pub fn is_empty(&self) -> bool {
        self.text_ext.is_empty() && self.binary_ext.is_empty()
    }

    /// The file this policy was loaded from, if any.
    pub fn source(&self) -> Option<&std::path::Path> {
        self.source.as_deref()
    }

    /// BLAKE3 (hex) of the config file's bytes.
    pub fn digest(&self) -> Option<&str> {
        self.digest.as_deref()
    }

    /// Classify a file by extension alone.
    ///
    /// `ext` is expected lowercased (the crawler stores it that way); the
    /// comparison is case-insensitive regardless.
    pub fn classify_ext(&self, ext: Option<&str>) -> ExtVerdict {
        let Some(ext) = ext else {
            // No extension at all (`Makefile`, `LICENSE`, `Dockerfile`, ...).
            return ExtVerdict::Unknown;
        };

        if self.text_ext.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
            return ExtVerdict::Text;
        }
        if self.binary_ext.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
            return ExtVerdict::Binary;
        }
        if TEXT_EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
            return ExtVerdict::Text;
        }
        // Ahead of the denylist, and only because the parser exists in this
        // build: with the feature off this yields `None` and the extension
        // falls through to `BINARY_EXTS` below, which is where it lived before
        // issue #40.
        if let Some(format) = crate::docmeta::BodyFormat::from_ext(Some(ext)) {
            return ExtVerdict::Extract(format);
        }
        if BINARY_EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
            return ExtVerdict::Binary;
        }
        ExtVerdict::Unknown
    }
}

/// Lowercase an extension and drop a leading dot.
fn normalize_ext(raw: &str) -> String {
    raw.trim().trim_start_matches('.').to_lowercase()
}

/// Name of the `meta` row a [`TextPolicy`] is persisted under.
pub const TEXT_POLICY_KEY: &str = "text_policy";

impl TextPolicy {
    /// Serialize for the `meta` table.
    ///
    /// The full-text index has to carry the rule it was built under, for the
    /// same reason the crawl carries its exclusion policy: a live grep that
    /// judges a changed file differently makes that file **disappear** from the
    /// answer the moment it is edited — the index hit is dropped as changed and
    /// no live hit replaces it. Auto-discovering `./sagasu-text.toml` at query
    /// time cannot do that job, because a search run from another directory
    /// finds no file and silently reverts to the built-in lists.
    ///
    /// Extensions are normalized to `[a-z0-9…]` with no whitespace, so the
    /// line format needs no escaping.
    pub fn encode(&self) -> String {
        let mut out = String::from("v1\n");
        if let Some(digest) = &self.digest {
            out.push_str(&format!("digest={digest}\n"));
        }
        if let Some(source) = &self.source {
            // Informational only — never re-read. It answers "which file was
            // this?" in a report, not "what are the rules?".
            out.push_str(&format!("source={}\n", source.display()));
        }
        for e in &self.text_ext {
            out.push_str(&format!("text={e}\n"));
        }
        for e in &self.binary_ext {
            out.push_str(&format!("binary={e}\n"));
        }
        out
    }

    /// Rebuild a policy written by [`TextPolicy::encode`]. Reads no files.
    ///
    /// Unknown keys and versions are errors, for the reasons spelled out on
    /// [`crate::walk::ExcludeSet::decode`]; the caller degrades rather than
    /// failing the query.
    pub fn decode(text: &str) -> anyhow::Result<Self> {
        use anyhow::bail;
        let mut lines = text.lines();
        match lines.next() {
            Some("v1") => {}
            other => bail!(
                "unsupported text policy format {other:?} — this index was written by a \
                 newer sagasu. Re-run `sagasu fulltext` with this build, or upgrade."
            ),
        }
        let mut policy = Self::empty();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                bail!("malformed text policy line: {line:?}");
            };
            match key {
                "digest" => policy.digest = Some(value.to_string()),
                "source" => policy.source = Some(std::path::PathBuf::from(value)),
                "text" => policy.text_ext.push(value.to_string()),
                "binary" => policy.binary_ext.push(value.to_string()),
                other => bail!(
                    "unknown text policy key {other:?} — this index was written by a \
                     newer sagasu. Re-run `sagasu fulltext` with this build, or upgrade."
                ),
            }
        }
        Ok(policy)
    }

    /// The policy the full-text index at `store` was built with, if it recorded
    /// one.
    ///
    /// # Errors
    ///
    /// Returns an error if the row exists but cannot be parsed.
    pub fn from_index(store: &crate::Store) -> anyhow::Result<Option<Self>> {
        match store.meta_get(TEXT_POLICY_KEY)? {
            Some(encoded) => Ok(Some(Self::decode(&encoded)?)),
            None => Ok(None),
        }
    }

    /// Whether two policies would classify every extension the same way.
    ///
    /// Compared by effect, not by provenance: two config files with different
    /// names and the same lists agree, and that is the only thing a warning
    /// about disagreement should care about.
    pub fn agrees_with(&self, other: &Self) -> bool {
        let norm = |v: &[String]| {
            let mut v = v.to_vec();
            v.sort();
            v.dedup();
            v
        };
        norm(&self.text_ext) == norm(&other.text_ext)
            && norm(&self.binary_ext) == norm(&other.binary_ext)
    }

    /// One-line description for a report.
    pub fn describe(&self) -> String {
        if self.is_empty() {
            return "built-in lists only".to_string();
        }
        let mut parts = Vec::new();
        if !self.text_ext.is_empty() {
            parts.push(format!("+text {}", self.text_ext.join(",")));
        }
        if !self.binary_ext.is_empty() {
            parts.push(format!("+binary {}", self.binary_ext.join(",")));
        }
        match &self.source {
            Some(p) => format!("{} ({})", parts.join(", "), p.display()),
            None => parts.join(", "),
        }
    }
}

/// Classify a file by extension alone under the built-in lists only.
///
/// The shorthand for callers with no user policy to apply; everything that has
/// one goes through [`TextPolicy::classify_ext`].
pub fn classify_ext(ext: Option<&str>) -> ExtVerdict {
    TextPolicy::empty().classify_ext(ext)
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

    // Control characters other than tab / LF / VT / FF / CR / ESC should be
    // rare in text.
    //
    // ESC (0x1B) is deliberately *not* counted. An ANSI-coloured log or a
    // terminal capture is full of it and is exactly the kind of file someone
    // greps their own disk for; the earlier version counted it, which
    // contradicted this comment and pushed the judgement in the direction of
    // dropping text — the failure this project treats as the worst one.
    //
    // The denominator has a floor of [`SNIFF_LEN`]. Without it the ratio is
    // brutal on short files: a 40-byte file with one stray control byte would
    // be called binary, because one is more than 1% of forty. Reading it as
    // "at most one control character per 512 bytes examined" keeps the same
    // rule for a full sample and stops a tiny file from being judged on one
    // byte.
    let control = sample
        .iter()
        .filter(|&&b| {
            (b < 0x09) || (0x0E..0x1B).contains(&b) || (0x1C..0x20).contains(&b) || b == 0x7F
        })
        .count();
    if control * 100 > sample.len().max(SNIFF_LEN) {
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

    fn policy_with_text(exts: &[&str]) -> TextPolicy {
        let mut p = TextPolicy::empty();
        p.add_text_exts(&exts.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        p
    }

    #[test]
    fn ext_allowlist_covers_the_esm_family() {
        for ext in ["mjs", "cjs", "jsx", "tsx", "mts", "cts"] {
            assert_eq!(
                classify_ext(Some(ext)),
                ExtVerdict::Text,
                "{ext} must be on the allowlist"
            );
        }
    }

    #[test]
    fn office_and_pdf_route_to_an_extractor_when_built_with_one() {
        for ext in ["pdf", "docx", "xlsx", "pptx"] {
            let verdict = classify_ext(Some(ext));
            let extracting = matches!(verdict, ExtVerdict::Extract(_));
            // The two builds are both correct; what must never happen is
            // `Unknown`, which would send a binary document to the sniffer and
            // report it as a *content* failure instead of a format decision.
            let expected_without_parsers =
                cfg!(not(feature = "office")) || cfg!(not(feature = "pdf"));
            assert!(
                extracting || (expected_without_parsers && verdict == ExtVerdict::Binary),
                "{ext} classified as {verdict:?}"
            );
        }
    }

    #[test]
    fn legacy_binary_office_stays_on_the_denylist() {
        // `.doc` is not `.docx` with a shorter name; nothing here can read it.
        for ext in ["doc", "xls", "ppt", "odt", "rtf"] {
            assert_eq!(classify_ext(Some(ext)), ExtVerdict::Binary, "{ext}");
        }
    }

    #[test]
    fn a_user_text_extension_still_beats_the_extractor() {
        // Precedence is documented on `TextPolicy`: the user is looking at the
        // files and we are not. Someone whose `.docx` are really plain text
        // must be able to say so.
        let p = policy_with_text(&["docx"]);
        assert_eq!(p.classify_ext(Some("docx")), ExtVerdict::Text);
    }

    #[test]
    fn unknown_extension_falls_through_to_sniffing() {
        assert_eq!(classify_ext(Some("wat")), ExtVerdict::Unknown);
        assert_eq!(classify_ext(None), ExtVerdict::Unknown);
    }

    #[test]
    fn user_extension_overrides_denylist() {
        assert_eq!(classify_ext(Some("obj")), ExtVerdict::Binary);
        assert_eq!(
            policy_with_text(&["obj"]).classify_ext(Some("obj")),
            ExtVerdict::Text
        );
    }

    #[test]
    fn a_leading_dot_and_upper_case_are_accepted_in_user_extensions() {
        let p = policy_with_text(&[".TMPL"]);
        assert_eq!(p.text_exts(), ["tmpl"]);
        assert_eq!(p.classify_ext(Some("tmpl")), ExtVerdict::Text);
        assert_eq!(p.classify_ext(Some("TMPL")), ExtVerdict::Text);
    }

    #[test]
    fn a_config_file_extends_both_lists() {
        let p = TextPolicy::parse(
            r#"
            text_ext   = ["tmpl", "ndjson"]
            binary_ext = ["dat"]
            "#,
        )
        .unwrap();
        assert_eq!(p.classify_ext(Some("tmpl")), ExtVerdict::Text);
        assert_eq!(p.classify_ext(Some("dat")), ExtVerdict::Binary);
        // Something already on the built-in allowlist stays there.
        assert_eq!(p.classify_ext(Some("md")), ExtVerdict::Text);
    }

    #[test]
    fn a_typo_in_a_text_config_key_is_an_error_not_a_dead_config() {
        // `text_exts` instead of `text_ext`: the file would otherwise load and
        // silently add nothing, and the user would blame the sniffer.
        let err = TextPolicy::parse(r#"text_exts = ["tmpl"]"#).unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn an_empty_text_config_is_valid_and_adds_nothing() {
        assert!(TextPolicy::parse("").unwrap().is_empty());
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

#[cfg(test)]
mod sniff_threshold_tests {
    use super::*;

    #[test]
    fn escape_sequences_do_not_make_a_coloured_log_binary() {
        // An ANSI-coloured log is text someone greps their own disk for. The
        // comment above `sniff_is_text` always said ESC was allowed; the code
        // counted it, and the disagreement pushed the judgement toward dropping
        // text — the failure this project treats as the worst one.
        let mut log = Vec::new();
        for i in 0..40 {
            log.extend_from_slice(format!("\x1b[32mINFO\x1b[0m line {i}\n").as_bytes());
        }
        assert!(sniff_is_text(&log));
    }

    #[test]
    fn a_short_file_is_not_condemned_by_a_single_control_byte() {
        // `control * 100 > len` on a 40-byte file means one control character
        // is more than 1% and the file is called binary. The floor of SNIFF_LEN
        // reads the rule as "at most one per 512 bytes examined" instead.
        let short = b"short text\x01 with one stray byte\n";
        assert!(short.len() < SNIFF_LEN);
        assert!(sniff_is_text(short));
    }

    #[test]
    fn a_control_dense_sample_is_still_rejected() {
        // The floor must not turn the check off: a small blob that is mostly
        // control bytes is still binary.
        let dense: Vec<u8> = (0..64)
            .map(|i| if i % 2 == 0 { 0x01 } else { b'a' })
            .collect();
        assert!(!sniff_is_text(&dense));
        // …and so is a full-length sample over the ratio.
        let mut long = vec![b'a'; SNIFF_LEN];
        for slot in long.iter_mut().take(8) {
            *slot = 0x02;
        }
        assert!(!sniff_is_text(&long));
    }
}
