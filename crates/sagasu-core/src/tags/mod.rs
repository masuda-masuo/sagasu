//! Rule-based tag engine — the semantic layer of design.md §6, without an LLM.
//!
//! ## What this is for
//!
//! Files are remembered by meaning, not by location, but a filesystem can only
//! be queried by location. This module manufactures the missing axis: it reads
//! what is already knowable about a file — its format, the directories it sits
//! in, the shape of its name — and turns that into `namespace:value` tags that
//! can be counted, faceted and drilled into (issue #5).
//!
//! ## The one hard property: determinism
//!
//! [`tags_for`] is a **pure function** of four inputs: the file's path, the
//! crawl root it is relative to, its extension, and its leading bytes. No clock,
//! no filesystem access, no iteration over a hash map, no dependence on the
//! order files were crawled in. Re-indexing therefore cannot make a file's tags
//! wobble, which is what lets a user build spatial memory on top of a facet
//! tree instead of re-learning it after every scan.
//!
//! Everything that could break that property is deliberately kept out:
//! `mtime` never produces a tag (it changes when a file is touched), and the
//! user rule set is order-independent (a file collects the *union* of matching
//! rules, see [`crate::tagrules`]).
//!
//! ## Generators
//!
//! | Namespace | Source | Example |
//! |---|---|---|
//! | `format:` | magic bytes, else extension | `format:pdf` |
//! | `ext:` | extension, alias-folded | `ext:jpg` |
//! | `kind:` | coarse category of the format | `kind:image` |
//! | `path:` | directory components and their sub-tokens | `path:invoices` |
//! | `date:` | date patterns in names | `date:2024`, `date:2024-03` |
//! | `version:` | version/revision markers in the file name | `version:v2` |
//! | `pattern:` | naming conventions | `pattern:screenshot` |
//! | `anomaly:` | contradictions between the sources | `anomaly:format-mismatch` |
//! | *(any)* | user rules | `author:masuda` |
//!
//! The file *name*'s word tokens deliberately do **not** become `path:` tags:
//! the name is already reachable through `sagasu find` and the full-text index,
//! whereas directory names are a query axis nothing else offers. Date, version
//! and naming patterns are still extracted from the name, because those are
//! *interpretations* of it rather than a repeat of its text.
//!
//! ## What is not here (deferred)
//!
//! Embedded metadata — Office document properties, PDF info dictionaries, EXIF
//! — is listed in design.md §6 but is not implemented in this version. It needs
//! a ZIP+XML reader, a PDF parser and an EXIF parser, i.e. three new dependency
//! families, and two of those same parsers are what the deferred PDF/Office
//! *body extraction* (`crate::text`, still out of scope after M1) will need. The
//! two belong in one issue, opened together, rather than pulling half the
//! dependency set in twice. Until then `author:`-style tags come from user rules
//! (see `docs/tag_rules.md`), which is where a person's own naming conventions
//! live anyway.
//!
//! ## Where the state lives
//!
//! Nowhere in here. Building the layer into SQLite, measuring its coverage and
//! querying it back are [`crate::tagindex`]. This module has no database handle
//! and makes no filesystem call, which is what turns the determinism claim above
//! from a promise into a property of the code.
//!
//! ## File layout
//!
//! One directory, three concerns, because the two that grow do so for reasons
//! that have nothing to do with each other:
//!
//! - **here** — the vocabulary ([`Tag`], [`TagSource`]), the input and output
//!   types ([`FileFacts`], [`TagSet`]), the cap, and [`tags_for`] itself, which
//!   is the only place the generators are wired together.
//! - `classify` — the static format/kind tables. Pure data plus the lookups
//!   over it; every new format is one more row there.
//! - `patterns` — what a *file name* can be read to mean: dates, version
//!   markers, naming conventions.
//!
//! The split is internal. Every name below keeps the path it had before
//! (`sagasu_core::tags::…`), by re-export where the definition moved.

mod classify;
mod patterns;

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};

use self::classify::{expected_formats, kind_from_format};
use crate::tagrules::RuleSet;

pub use self::classify::{fold_ext, format_from_ext, format_from_magic, kind_from_ext};
pub use self::patterns::{date_values, pattern_values, version_values};

// ── Namespaces ──────────────────────────────────────────────────────────────

/// Concrete file format (`format:pdf`), from magic bytes where available.
pub const NS_FORMAT: &str = "format";
/// Alias-folded extension (`ext:jpg` for both `.jpg` and `.jpeg`).
pub const NS_EXT: &str = "ext";
/// Coarse category (`kind:image`).
pub const NS_KIND: &str = "kind";
/// Directory component or sub-token (`path:invoices`).
pub const NS_PATH: &str = "path";
/// Date found in a name (`date:2024`, `date:2024-03`).
pub const NS_DATE: &str = "date";
/// Version / revision marker (`version:v2`, `version:final`).
pub const NS_VERSION: &str = "version";
/// Naming convention (`pattern:screenshot`).
pub const NS_PATTERN: &str = "pattern";
/// A contradiction between two sources (`anomaly:format-mismatch`).
pub const NS_ANOMALY: &str = "anomaly";

/// Namespaces that fall out of *any* path with an extension, so they say
/// nothing about a file beyond what `ls` already showed.
///
/// The acceptance criterion for this work is "the share of files that get at
/// least one semantic tag". Counting `ext:`/`kind:`/`format:`/`path:` would make
/// that number ~100% by construction and therefore meaningless, so the summary
/// reports coverage *both* ways and this list is what separates them.
pub const STRUCTURAL_NAMESPACES: &[&str] = &[NS_FORMAT, NS_EXT, NS_KIND, NS_PATH];

/// Upper bound on tags per file.
///
/// A pathologically deep or long path would otherwise put unbounded rows in
/// `file_tags`. Which tags survive the cut is decided by [`cap_priority`] and
/// then by tag order, so it is deterministic; what was dropped is carried in
/// [`TagSet::dropped`] rather than vanishing.
pub const MAX_TAGS_PER_FILE: usize = 64;

/// How many directory components (nearest the file first) contribute `path:`
/// tags. Deeper ancestors are progressively less about the file itself.
pub const MAX_PATH_COMPONENTS: usize = 12;

/// Longest accepted tag value, in characters.
pub const MAX_VALUE_CHARS: usize = 128;

// ── Tag ─────────────────────────────────────────────────────────────────────

/// One `namespace:value` tag.
///
/// Both halves are lowercased on construction. Case folding costs the original
/// spelling but buys a single facet bucket: `author:Masuda` and `author:masuda`
/// being two different tags would split every count in a facet tree, and there
/// is no way for a user to notice that from the outside.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tag {
    namespace: String,
    value: String,
}

impl Tag {
    /// Build a tag from its two halves, validating and lowercasing both.
    pub fn new(namespace: &str, value: &str) -> Result<Self> {
        let namespace = namespace.trim().to_lowercase();
        let value = value.trim().to_lowercase();

        if namespace.is_empty() {
            bail!("tag namespace is empty");
        }
        if !namespace
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            bail!("tag namespace {namespace:?} must be ASCII [a-z0-9_-]");
        }
        if value.is_empty() {
            bail!("tag value for namespace {namespace:?} is empty");
        }
        if value.chars().any(|c| c.is_control()) {
            bail!("tag value {value:?} contains a control character");
        }
        if value.chars().count() > MAX_VALUE_CHARS {
            bail!("tag value {value:?} is longer than {MAX_VALUE_CHARS} characters");
        }

        Ok(Self { namespace, value })
    }

    /// Parse `namespace:value`. The value may itself contain `:`.
    pub fn parse(s: &str) -> Result<Self> {
        let (ns, value) = s
            .split_once(':')
            .with_context(|| format!("tag {s:?} is not in `namespace:value` form"))?;
        Self::new(ns, value)
    }

    /// The namespace half.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// The value half.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl std::fmt::Display for Tag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.namespace, self.value)
    }
}

// ── Sources ─────────────────────────────────────────────────────────────────

/// Which generator produced a tag. Stored as a bitmask because the same tag can
/// come from several at once (`format:pdf` from both the extension and the magic
/// bytes), and the union of two sets is order-independent — one more thing that
/// cannot make the stored result depend on evaluation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum TagSource {
    /// The file extension.
    Ext = 1,
    /// The leading bytes of the file.
    Magic = 2,
    /// A directory component of the path.
    Path = 4,
    /// The file name (date / version / naming pattern).
    Name = 8,
    /// A user rule (`crate::tagrules`).
    Rule = 16,
}

impl TagSource {
    /// All sources, in bit order.
    pub const ALL: [TagSource; 5] = [
        TagSource::Ext,
        TagSource::Magic,
        TagSource::Path,
        TagSource::Name,
        TagSource::Rule,
    ];

    /// Stable label used in CLI output.
    pub fn as_str(self) -> &'static str {
        match self {
            TagSource::Ext => "ext",
            TagSource::Magic => "magic",
            TagSource::Path => "path",
            TagSource::Name => "name",
            TagSource::Rule => "rule",
        }
    }

    /// Decode a stored bitmask into labels, in bit order.
    pub fn describe(mask: u32) -> Vec<&'static str> {
        Self::ALL
            .iter()
            .filter(|s| mask & (**s as u32) != 0)
            .map(|s| s.as_str())
            .collect()
    }
}

// ── Engine input / output ───────────────────────────────────────────────────

/// Everything the engine is allowed to look at. Constructing one of these is the
/// only way to call [`tags_for`], which is how the purity claim in the module
/// docs is kept honest: there is no path from here to a clock or a syscall.
#[derive(Debug, Clone, Copy)]
pub struct FileFacts<'a> {
    /// The file's path as stored in `files.path` (absolute, native separators).
    pub path: &'a str,
    /// The crawl root, so path tags can be relative to it. `None` falls back to
    /// treating every component of the absolute path as a candidate.
    pub root: Option<&'a str>,
    /// The lowercased extension, as stored in `files.ext`.
    pub ext: Option<&'a str>,
    /// The first [`MAGIC_LEN`] bytes, as stored in `files.magic`. `None` = never
    /// read; the engine then works from the extension alone and the caller is
    /// told how many files were in that position.
    pub magic: Option<&'a [u8]>,
}

/// The tags of one file: sorted, deduplicated, each with its source mask.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagSet {
    /// `(tag, source mask)` in `Tag` order.
    pub tags: Vec<(Tag, u32)>,
    /// True when [`MAX_TAGS_PER_FILE`] cut the list short.
    pub capped: bool,
    /// The tags that lost the cut, in `Tag` order. Kept rather than counted so
    /// `sagasu tags --file` can name them: "this file has 64 tags" and "these
    /// eleven tags were dropped, all of them `path:`" are different answers, and
    /// only the second one lets a user decide whether it mattered.
    pub dropped: Vec<(Tag, u32)>,
}

impl TagSet {
    /// Number of tags.
    pub fn len(&self) -> usize {
        self.tags.len()
    }

    /// Whether the file got no tags at all.
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }

    /// Whether any tag lives outside [`STRUCTURAL_NAMESPACES`].
    pub fn has_semantic_tag(&self) -> bool {
        self.tags
            .iter()
            .any(|(t, _)| !STRUCTURAL_NAMESPACES.contains(&t.namespace()))
    }

    /// How many tags the cap dropped, per namespace, in namespace order.
    pub fn dropped_by_namespace(&self) -> BTreeMap<String, u64> {
        let mut out: BTreeMap<String, u64> = BTreeMap::new();
        for (tag, _) in &self.dropped {
            *out.entry(tag.namespace().to_string()).or_insert(0) += 1;
        }
        out
    }
}

/// Rank for the [`MAX_TAGS_PER_FILE`] cut — lower survives.
///
/// The cut used to be plain `truncate` over the alphabetically ordered set,
/// which is the worst possible order for it: `path:` is the one axis that grows
/// multiplicatively with depth *and* the one that says least (a directory name
/// is already visible in the path), so it ate the budget and starved everything
/// else. Measured on a real tree, `sagasu tags project:client-work` answered
/// `hits: 0` for files the rule plainly matched.
///
/// So the budget is spent in the order of how much a tag could only have come
/// from here:
///
/// 1. **User-rule tags.** Someone wrote this knowledge down by hand; nothing
///    else in the system can reconstruct it.
/// 2. **Other non-structural tags** (`date:` / `version:` / `pattern:` /
///    `anomaly:`) — interpretations of the name, not a repeat of it.
/// 3. **Structural, non-`path:`** (`format:` / `ext:` / `kind:`) — one tag each,
///    so they cannot crowd anything out.
/// 4. **`path:`** — the unbounded one, and the one `sagasu find` can stand in
///    for.
///
/// Ties are broken by tag order, so the whole comparison is a total order and
/// the surviving set stays deterministic.
fn cap_priority(tag: &Tag, sources: u32) -> u8 {
    if sources & (TagSource::Rule as u32) != 0 {
        0
    } else if !STRUCTURAL_NAMESPACES.contains(&tag.namespace()) {
        1
    } else if tag.namespace() != NS_PATH {
        2
    } else {
        3
    }
}

/// Accumulator that keeps the output canonical no matter what order the
/// generators run in.
#[derive(Default)]
struct Collector {
    map: BTreeMap<Tag, u32>,
}

impl Collector {
    fn add(&mut self, namespace: &str, value: &str, source: TagSource) {
        // A generator producing an invalid tag is a bug in *this* file, not user
        // input; the value is dropped rather than aborting a whole index pass
        // over one odd filename. User-supplied tags are validated at rule-load
        // time instead, where the error can name the offending rule.
        if let Ok(tag) = Tag::new(namespace, value) {
            *self.map.entry(tag).or_insert(0) |= source as u32;
        }
    }

    fn add_tag(&mut self, tag: Tag, source: TagSource) {
        *self.map.entry(tag).or_insert(0) |= source as u32;
    }

    fn finish(self) -> TagSet {
        // `BTreeMap` hands these over in `Tag` order already.
        let mut tags: Vec<(Tag, u32)> = self.map.into_iter().collect();
        if tags.len() <= MAX_TAGS_PER_FILE {
            return TagSet {
                tags,
                capped: false,
                dropped: Vec::new(),
            };
        }
        // A *stable* sort by priority alone: within a tier the incoming `Tag`
        // order is preserved, so priority-then-tag is the effective total order
        // without having to clone a key for it.
        tags.sort_by_key(|(tag, sources)| cap_priority(tag, *sources));
        let mut dropped = tags.split_off(MAX_TAGS_PER_FILE);
        // Both halves go back to canonical order: everything downstream (the
        // stored rows, the explain output, the determinism tests) reads them as
        // sorted lists, and the priority order is an implementation detail of
        // the cut itself.
        tags.sort();
        dropped.sort();
        TagSet {
            tags,
            capped: true,
            dropped,
        }
    }
}

/// Generate every tag for one file.
///
/// Pure: same inputs, same output, on every platform and every run.
pub fn tags_for(facts: &FileFacts<'_>, rules: &RuleSet) -> TagSet {
    let mut c = Collector::default();

    let rel = relative_path(facts.path, facts.root);
    let file_name = rel.rsplit('/').next().unwrap_or("").to_string();
    let stem = file_stem(&file_name);

    // ── format / ext / kind ─────────────────────────────────────────────────
    let ext = facts.ext.map(|e| e.to_lowercase());
    let ext = ext.as_deref().filter(|e| !e.is_empty());
    let folded = ext.map(fold_ext);
    if let Some(folded) = folded {
        c.add(NS_EXT, folded, TagSource::Ext);
    }

    let magic_format = facts.magic.and_then(format_from_magic);
    let ext_format = folded.and_then(format_from_ext);

    match (magic_format, ext_format) {
        (Some(m), _) => c.add(NS_FORMAT, m, TagSource::Magic),
        (None, Some(e)) => c.add(NS_FORMAT, e, TagSource::Ext),
        (None, None) => {}
    }

    // Only claim a mismatch when the extension carries a *binary* expectation:
    // "this .png does not start with a PNG header" is a real finding, while
    // "this .txt is Shift_JIS so it sniffed as binary" is an encoding gap in
    // `crate::text`, not a mislabelled file, and flagging it would train the
    // user to ignore the namespace.
    if let (Some(m), Some(folded)) = (magic_format, folded) {
        if let Some(expected) = expected_formats(folded) {
            if !expected.contains(&m) {
                c.add(NS_ANOMALY, "format-mismatch", TagSource::Magic);
            }
        }
    }

    let kind = folded
        .and_then(kind_from_ext)
        .or_else(|| magic_format.or(ext_format).and_then(kind_from_format));
    if let Some(kind) = kind {
        c.add(NS_KIND, kind, TagSource::Ext);
    }

    // ── path components ─────────────────────────────────────────────────────
    let components: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    let dirs = components.split_last().map(|(_, d)| d).unwrap_or(&[]);
    let start = dirs.len().saturating_sub(MAX_PATH_COMPONENTS);
    for component in &dirs[start..] {
        for token in component_tokens(component) {
            c.add(NS_PATH, &token, TagSource::Path);
        }
        for date in date_values(component) {
            c.add(NS_DATE, &date, TagSource::Path);
        }
    }

    // ── file name patterns ──────────────────────────────────────────────────
    for date in date_values(stem) {
        c.add(NS_DATE, &date, TagSource::Name);
    }
    for version in version_values(stem) {
        c.add(NS_VERSION, &version, TagSource::Name);
    }
    for pattern in pattern_values(&file_name, stem, ext) {
        c.add(NS_PATTERN, &pattern, TagSource::Name);
    }

    // ── user rules ──────────────────────────────────────────────────────────
    if !rules.is_empty() {
        let rel_lower = rel.to_lowercase();
        let name_lower = file_name.to_lowercase();
        for rule in rules.matches(&rel_lower, &name_lower, ext) {
            for tag in rule.tags() {
                c.add_tag(tag.clone(), TagSource::Rule);
            }
        }
    }

    c.finish()
}

// ── Path handling ───────────────────────────────────────────────────────────

/// Path relative to the crawl root, always `/`-separated.
///
/// Producing the same string from `C:\work\a\b.txt` and `/work/a/b.txt` is what
/// makes one rule file usable on both platforms, and what stops a `path:` tag
/// from encoding which machine ran the crawl.
pub fn relative_path(path: &str, root: Option<&str>) -> String {
    let norm = normalize_separators(path);
    if let Some(root) = root {
        let root = normalize_separators(root);
        let root = root.trim_end_matches('/');
        // Compared as bytes on purpose. `str::split_at` panics when the index is
        // not a character boundary, and `root.len()` lands mid-character
        // whenever the path diverges from the root inside a multi-byte name
        // (`/ab` against `/写真/...`). A prefix test must never be able to abort
        // an index pass over a Japanese directory name.
        let (rb, nb) = (root.as_bytes(), norm.as_bytes());
        if !rb.is_empty()
            && nb.len() > rb.len()
            && nb[..rb.len()].eq_ignore_ascii_case(rb)
            // NTFS compares case-insensitively; folding ASCII case is enough
            // here because the root was produced by the same canonicalization
            // that produced the path.
            && nb[rb.len()] == b'/'
        {
            return norm[rb.len() + 1..].trim_start_matches('/').to_string();
        }
        if norm.eq_ignore_ascii_case(root) {
            return String::new();
        }
    }
    // No root (or the path is not under it): drop a leading `/` and any drive
    // letter so the result is still a relative-looking path.
    let stripped = norm.trim_start_matches('/');
    match stripped.split_once('/') {
        Some((first, rest)) if first.len() == 2 && first.ends_with(':') => rest.to_string(),
        _ => stripped.to_string(),
    }
}

/// `\` → `/`, with any Windows verbatim prefix removed.
///
/// `std::fs::canonicalize` returns `\\?\C:\…` on Windows (and
/// `\\?\UNC\server\share\…` for network paths), and that is what `sagasu index`
/// stores, so the prefix is the *ordinary* case there rather than an exotic one.
/// Left in place it survives the `\`→`/` swap as `//?/C:/…`, and the fallback
/// below then reads `?` as the first component and `c:` as the second — turning
/// a drive letter into a facet bucket (`path:c:`) and making every file look
/// like it diverged from the crawl root.
///
/// Stripping it also lets a root recorded one way match a path recorded the
/// other, which is the same normalisation `delta::path_under` needs and for the
/// same reason (design.md §5-2).
fn normalize_separators(path: &str) -> String {
    let norm = path.replace('\\', "/");
    let Some(rest) = norm.strip_prefix("//?/") else {
        return norm;
    };
    // `\\?\UNC\server\share` denotes `\\server\share`; keep it a UNC path
    // rather than letting `UNC` become a directory token.
    match rest.get(..4) {
        Some(p) if p.eq_ignore_ascii_case("unc/") => format!("//{}", &rest[4..]),
        _ => rest.to_string(),
    }
}

/// The file name minus its final extension.
fn file_stem(name: &str) -> &str {
    match name.rfind('.') {
        // A leading dot is part of the name (`.gitignore`), not a separator.
        Some(0) | None => name,
        Some(i) => &name[..i],
    }
}

/// Characters that separate words inside a path component or file name.
const TOKEN_SEPARATORS: &[char] = &[
    '-', '_', '.', ' ', '+', '&', '(', ')', '[', ']', '{', '}', ',', ';', '\'', '@', '!', '#', '=',
    '~', '\t', '　',
];

/// `path:` tags for one directory component: the whole component, plus its
/// sub-tokens when splitting yields more than one.
fn component_tokens(component: &str) -> Vec<String> {
    let mut out = Vec::new();
    let whole = component.to_lowercase();
    if is_useful_token(&whole) {
        out.push(whole);
    }
    let parts: Vec<&str> = component
        .split(TOKEN_SEPARATORS)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.len() > 1 {
        for part in parts {
            let t = part.to_lowercase();
            if is_useful_token(&t) && !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

/// Whether a token is worth a facet bucket.
///
/// Single characters carry no meaning, and an all-digit token is either a date
/// (which the `date:` generator already handles properly) or an opaque id.
fn is_useful_token(token: &str) -> bool {
    let chars = token.chars().count();
    if !(2..=MAX_VALUE_CHARS).contains(&chars) {
        return false;
    }
    !token.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(path: &'a str, root: &'a str, ext: Option<&'a str>) -> FileFacts<'a> {
        FileFacts {
            path,
            root: Some(root),
            ext,
            magic: None,
        }
    }

    fn values(set: &TagSet, ns: &str) -> Vec<String> {
        set.tags
            .iter()
            .filter(|(t, _)| t.namespace() == ns)
            .map(|(t, _)| t.value().to_string())
            .collect()
    }

    #[test]
    fn tag_parsing_lowercases_both_halves() {
        let t = Tag::parse("Author:Masuda Masuo").unwrap();
        assert_eq!(t.namespace(), "author");
        assert_eq!(t.value(), "masuda masuo");
        assert_eq!(t.to_string(), "author:masuda masuo");
    }

    #[test]
    fn tag_parsing_rejects_the_shapes_that_would_corrupt_a_facet_axis() {
        assert!(Tag::parse("nocolon").is_err());
        assert!(Tag::parse(":value").is_err());
        assert!(Tag::parse("ns:").is_err());
        assert!(Tag::parse("bad ns:value").is_err());
        // A value may contain a colon; only the first one splits.
        assert_eq!(Tag::parse("url:http://x").unwrap().value(), "http://x");
    }

    #[test]
    fn relative_path_is_platform_neutral() {
        assert_eq!(
            relative_path(r"C:\work\docs\a.txt", Some(r"C:\work")),
            "docs/a.txt"
        );
        assert_eq!(
            relative_path("/home/u/work/docs/a.txt", Some("/home/u/work")),
            "docs/a.txt"
        );
        // Root given with a trailing separator, and a case difference.
        assert_eq!(
            relative_path(r"C:\Work\docs\a.txt", Some(r"c:\work\")),
            "docs/a.txt"
        );
        // Not under the root: fall back to a relative-looking path.
        assert_eq!(relative_path("/other/a.txt", Some("/work")), "other/a.txt");
        // A path that diverges from the root *inside* a multi-byte character.
        // Comparing by byte index here used to be a panic, not a mismatch.
        assert_eq!(relative_path("/写真/a.txt", Some("/ab")), "写真/a.txt");
        assert_eq!(relative_path("/a/写真", Some("/a/写")), "a/写真");
    }

    #[test]
    fn a_windows_verbatim_prefix_does_not_become_a_path_tag() {
        // What `std::fs::canonicalize` actually returns on Windows, which is
        // what `sagasu index` stores — so this is the common case there.
        assert_eq!(
            relative_path(r"\\?\C:\work\docs\a.txt", Some(r"\\?\C:\work")),
            "docs/a.txt"
        );
        // A root recorded one way and a path the other must still line up.
        assert_eq!(
            relative_path(r"\\?\C:\work\docs\a.txt", Some(r"C:\work")),
            "docs/a.txt"
        );
        assert_eq!(
            relative_path(r"C:\work\docs\a.txt", Some(r"\\?\C:\work")),
            "docs/a.txt"
        );
        // Outside the root: the fallback must not leave `?` and `c:` behind as
        // components — `path:c:` is a bucket that means nothing.
        let outside = relative_path(r"\\?\C:\other\a.txt", Some(r"\\?\C:\work"));
        assert_eq!(outside, "other/a.txt");
        // UNC keeps its share, and `UNC` itself is not a directory.
        assert_eq!(
            relative_path(r"\\?\UNC\server\share\docs\a.txt", None),
            "server/share/docs/a.txt"
        );
        assert_eq!(
            relative_path(
                r"\\?\UNC\server\share\docs\a.txt",
                Some(r"\\?\UNC\server\share")
            ),
            "docs/a.txt"
        );
    }

    #[test]
    fn a_verbatim_path_yields_no_meaningless_tags() {
        let set = tags_for(
            &facts(r"\\?\C:\other\invoices\a.txt", r"\\?\C:\work", Some("txt")),
            &RuleSet::empty(),
        );
        let path_tags = values(&set, NS_PATH);
        assert!(path_tags.contains(&"invoices".to_string()), "{path_tags:?}");
        for junk in ["c:", "?", "unc"] {
            assert!(
                !path_tags.contains(&junk.to_string()),
                "{junk:?} is not a directory: {path_tags:?}"
            );
        }
    }

    #[test]
    fn path_tags_come_from_directories_not_from_the_file_name() {
        let set = tags_for(
            &facts(
                "/root/clients/acme-corp/invoice-april.pdf",
                "/root",
                Some("pdf"),
            ),
            &RuleSet::empty(),
        );
        let path_tags = values(&set, NS_PATH);
        assert!(path_tags.contains(&"clients".to_string()));
        assert!(path_tags.contains(&"acme-corp".to_string()));
        assert!(path_tags.contains(&"acme".to_string()));
        assert!(path_tags.contains(&"corp".to_string()));
        assert!(
            !path_tags.contains(&"invoice".to_string()),
            "the file name must not become a path token: {path_tags:?}"
        );
    }

    #[test]
    fn output_is_sorted_and_deduplicated() {
        let set = tags_for(
            &facts("/root/2024/2024/report-2024.txt", "/root", Some("txt")),
            &RuleSet::empty(),
        );
        let list: Vec<String> = set.tags.iter().map(|(t, _)| t.to_string()).collect();
        let mut sorted = list.clone();
        sorted.sort();
        assert_eq!(list, sorted, "tags must come back in canonical order");
        assert_eq!(values(&set, NS_DATE), vec!["2024"], "duplicates collapse");
    }

    /// A path deep and wordy enough to blow past [`MAX_TAGS_PER_FILE`] on
    /// `path:` tags alone, with a dated, versioned file name at the end of it.
    fn overflowing_path() -> String {
        let mut out = String::from("/root/");
        for d in 0..MAX_PATH_COMPONENTS {
            let letter = (b'a' + d as u8) as char;
            let component: Vec<String> = (0..4)
                .map(|i| format!("{letter}{}", (b'a' + i as u8) as char))
                .collect();
            out.push_str(&component.join("-"));
            out.push('/');
        }
        out.push_str("invoice-2024-03-15_v2.txt");
        out
    }

    #[test]
    fn the_cap_spends_its_budget_on_the_tags_nothing_else_could_produce() {
        let rules = RuleSet::parse(
            r#"
            [[rule]]
            ext  = ["txt"]
            tags = [
                "project:client-work", "client:acme", "billing:billable",
                "dept:accounting", "retention:7y", "stage:final",
                "author:masuda", "doc-type:invoice",
            ]
            "#,
        )
        .unwrap();
        let path = overflowing_path();
        let set = tags_for(
            &FileFacts {
                path: &path,
                root: Some("/root"),
                ext: Some("txt"),
                magic: None,
            },
            &rules,
        );

        assert!(set.capped, "this path must overflow the cap");
        assert_eq!(set.len(), MAX_TAGS_PER_FILE);

        // The whole point: a user rule's tags are the *last* thing to go, not
        // the first. Alphabetical truncation dropped every one of these.
        for want in [
            "project:client-work",
            "client:acme",
            "billing:billable",
            "dept:accounting",
            "retention:7y",
            "stage:final",
            "author:masuda",
            "doc-type:invoice",
        ] {
            assert!(
                set.tags.iter().any(|(t, _)| t.to_string() == want),
                "{want} was dropped by the cap: {:?}",
                set.tags
                    .iter()
                    .map(|(t, _)| t.to_string())
                    .collect::<Vec<_>>()
            );
        }
        // …and so are the interpretations of the name, and the one-per-file
        // structural tags.
        for want in ["date:2024", "date:2024-03", "version:v2", "ext:txt"] {
            assert!(
                set.tags.iter().any(|(t, _)| t.to_string() == want),
                "{want} was dropped by the cap"
            );
        }

        // Everything sacrificed is `path:`, and it is reported rather than
        // silently missing.
        assert!(!set.dropped.is_empty());
        assert_eq!(
            set.dropped_by_namespace().keys().collect::<Vec<_>>(),
            vec![NS_PATH],
            "only path tags should have been given up: {:?}",
            set.dropped_by_namespace()
        );
    }

    #[test]
    fn the_capped_result_is_still_sorted_and_still_deterministic() {
        let path = overflowing_path();
        let facts = FileFacts {
            path: &path,
            root: Some("/root"),
            ext: Some("txt"),
            magic: None,
        };
        let first = tags_for(&facts, &RuleSet::empty());
        let second = tags_for(&facts, &RuleSet::empty());
        assert_eq!(first, second, "the cut must not wobble between runs");

        let list: Vec<String> = first.tags.iter().map(|(t, _)| t.to_string()).collect();
        let mut sorted = list.clone();
        sorted.sort();
        assert_eq!(list, sorted, "survivors must come back in canonical order");

        let dropped: Vec<String> = first.dropped.iter().map(|(t, _)| t.to_string()).collect();
        let mut sorted_dropped = dropped.clone();
        sorted_dropped.sort();
        assert_eq!(dropped, sorted_dropped);
        // No tag is both kept and dropped.
        assert!(!dropped.iter().any(|d| list.contains(d)));
    }

    #[test]
    fn a_set_under_the_cap_drops_nothing() {
        let set = tags_for(
            &facts("/root/a/notes.txt", "/root", Some("txt")),
            &RuleSet::empty(),
        );
        assert!(!set.capped);
        assert!(set.dropped.is_empty());
        assert!(set.dropped_by_namespace().is_empty());
    }

    #[test]
    fn the_same_tag_from_two_sources_keeps_both_bits() {
        // `2024` appears both as a directory and in the file name.
        let set = tags_for(
            &facts("/root/2024/report-2024.txt", "/root", Some("txt")),
            &RuleSet::empty(),
        );
        let (_, sources) = set
            .tags
            .iter()
            .find(|(t, _)| t.namespace() == NS_DATE && t.value() == "2024")
            .expect("date:2024");
        assert_eq!(
            TagSource::describe(*sources),
            vec!["path", "name"],
            "a tag reached from two generators must record both"
        );
    }
}
