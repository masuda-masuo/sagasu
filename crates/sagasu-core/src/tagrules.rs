//! User-defined tag rules — the declarative half of design.md §6.
//!
//! The built-in generators in [`crate::tags`] can only see what is *in* a path:
//! a format, a directory name, a date in a filename. They cannot know that
//! `clients/acme/**` is billable work or that `**/keiri/**` belongs to the
//! accounting department. That knowledge is the user's, and this module is
//! where they write it down.
//!
//! ## Format
//!
//! TOML, because `bench/configs/*.toml` already made TOML this project's config
//! language and having two would be worse than having one.
//!
//! ```toml
//! [[rule]]
//! name = "顧客案件"          # optional label, used in error messages
//! path = "clients/**"        # glob on the root-relative, '/'-separated path
//! tags = ["project:client", "confidential:yes"]
//!
//! [[rule]]
//! file = "*.psd"             # glob on the file name only
//! tags = ["app:photoshop"]
//!
//! [[rule]]
//! ext  = ["docx", "xlsx"]    # extension list (no leading dot)
//! path = "**/keiri/**"
//! tags = ["dept:accounting"]
//! ```
//!
//! ## Matching semantics
//!
//! - A rule's conditions (`path`, `file`, `ext`) are **AND**ed. A rule with no
//!   condition at all is rejected at load time: it would tag every file in the
//!   corpus, which is never what someone meant to write.
//! - Every rule is evaluated; a file collects the union of all matching rules'
//!   tags. Order does not matter, so the result cannot depend on how the file
//!   happens to be written.
//! - Globs match **case-insensitively** against the path *relative to the crawl
//!   root*, always with `/` separators. Both choices exist so the same rule file
//!   produces the same tags on Windows and on Linux — determinism across
//!   machines, not just across runs.
//!
//! ## Failing loudly
//!
//! Unknown keys are an error (`deny_unknown_fields`): a rule file where `tag =`
//! was typed instead of `tags =` must not load as a rule that silently does
//! nothing. Malformed tags, empty tag lists and bad globs are errors too, each
//! reported with the rule's index and name.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use globset::{Glob, GlobMatcher};
use serde::Deserialize;

use crate::tags::Tag;

/// The file rules used to be read from, before the two config files were merged
/// into one (issue #6, docs/cli.md §5).
///
/// Kept as a constant because it is still *looked for*: a `sagasu-tags.toml`
/// left in the working directory is no longer read, and saying so is the whole
/// point — see [`crate::config::check_no_legacy_config`].
pub const LEGACY_RULES_FILE: &str = "sagasu-tags.toml";

// ── On-disk shape ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    #[serde(default)]
    rule: Vec<RawRule>,
}

/// One `[[tags.rule]]` table as written on disk.
///
/// `pub(crate)` so [`crate::config`] can deserialize the same shape out of the
/// unified `sagasu.toml` rather than describing the rule format a second time
/// (two descriptions of one format is how they drift apart).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRule {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) path: Option<String>,
    #[serde(default)]
    pub(crate) file: Option<String>,
    #[serde(default)]
    pub(crate) ext: Vec<String>,
    pub(crate) tags: Vec<String>,
}

// ── Compiled form ───────────────────────────────────────────────────────────

/// One compiled rule: a conjunction of conditions and the tags it contributes.
#[derive(Debug)]
pub struct Rule {
    /// Label from the file, or a generated `rule #N`. Used in diagnostics.
    pub name: String,
    path: Option<GlobMatcher>,
    file: Option<GlobMatcher>,
    /// Lowercased extensions; empty means "any".
    ext: Vec<String>,
    tags: Vec<Tag>,
}

impl Rule {
    /// The tags this rule contributes when it matches.
    pub fn tags(&self) -> &[Tag] {
        &self.tags
    }

    /// Whether this rule matches a file.
    ///
    /// `rel_path` must already be root-relative, `/`-separated and lowercased —
    /// see [`RuleSet::matches`], which is the only caller that gets that right
    /// by construction.
    fn matches(&self, rel_path: &str, file_name: &str, ext: Option<&str>) -> bool {
        if let Some(g) = &self.path {
            if !g.is_match(rel_path) {
                return false;
            }
        }
        if let Some(g) = &self.file {
            if !g.is_match(file_name) {
                return false;
            }
        }
        if !self.ext.is_empty() {
            let Some(ext) = ext else { return false };
            if !self.ext.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
                return false;
            }
        }
        true
    }
}

/// A loaded rule file. An empty set is legal and means "built-in tags only".
#[derive(Debug, Default)]
pub struct RuleSet {
    rules: Vec<Rule>,
    /// Where the rules came from. `None` = no rule file was used.
    source: Option<PathBuf>,
    /// BLAKE3 (hex) of the rule file's bytes. Recorded alongside the tags so a
    /// later run can say *which* rules produced the tags in the database, and
    /// an edited rule file is visible rather than assumed.
    digest: Option<String>,
}

impl RuleSet {
    /// The empty rule set.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Number of rules.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether the set contains no rules.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The rule file this set was loaded from.
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// BLAKE3 (hex) of the rule file's bytes.
    pub fn digest(&self) -> Option<&str> {
        self.digest.as_deref()
    }

    /// Load and compile a rule file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read tag rules {}", path.display()))?;
        let digest = blake3::hash(text.as_bytes()).to_hex().to_string();
        let mut set = Self::parse(&text)
            .with_context(|| format!("invalid tag rules in {}", path.display()))?;
        set.source = Some(path.to_path_buf());
        set.digest = Some(digest);
        Ok(set)
    }

    /// Compile rules from TOML text (the testable half of [`RuleSet::load`]).
    ///
    /// The bare `[[rule]]` shape, which is what [`RuleSet::load`] reads. The
    /// unified `sagasu.toml` nests the same tables under `[[tags.rule]]` and
    /// goes through [`crate::config`] instead.
    pub fn parse(text: &str) -> Result<Self> {
        let file: RuleFile = toml::from_str(text)?;
        Self::from_raw(file.rule)
    }

    /// Record which file this rule set came from, and the digest of its bytes.
    pub(crate) fn with_origin(mut self, source: PathBuf, digest: String) -> Self {
        self.source = Some(source);
        self.digest = Some(digest);
        self
    }

    /// Compile already-deserialized rule tables.
    pub(crate) fn from_raw(raw_rules: Vec<RawRule>) -> Result<Self> {
        let mut rules = Vec::with_capacity(raw_rules.len());

        for (i, raw) in raw_rules.into_iter().enumerate() {
            let name = raw
                .name
                .clone()
                .unwrap_or_else(|| format!("rule #{}", i + 1));

            if raw.path.is_none() && raw.file.is_none() && raw.ext.is_empty() {
                bail!(
                    "{name}: has no condition (`path`, `file` or `ext`). \
                     A rule with no condition would tag every file in the corpus; \
                     if that is really the intent, say so with `path = \"**\"`."
                );
            }
            if raw.tags.is_empty() {
                bail!("{name}: `tags` is empty — the rule would do nothing");
            }

            let tags = raw
                .tags
                .iter()
                .map(|t| Tag::parse(t).with_context(|| format!("{name}: bad tag {t:?}")))
                .collect::<Result<Vec<_>>>()?;

            let path = raw
                .path
                .as_deref()
                .map(compile_glob)
                .transpose()
                .with_context(|| format!("{name}: bad `path` glob"))?;
            let file = raw
                .file
                .as_deref()
                .map(compile_glob)
                .transpose()
                .with_context(|| format!("{name}: bad `file` glob"))?;

            rules.push(Rule {
                name,
                path,
                file,
                ext: raw.ext.iter().map(|e| e.to_lowercase()).collect(),
                tags,
            });
        }

        Ok(Self {
            rules,
            source: None,
            digest: None,
        })
    }

    /// Every rule matching this file, in file order.
    ///
    /// `rel_path` is the root-relative path with `/` separators; matching is
    /// case-insensitive (the globs were compiled that way).
    pub fn matches(&self, rel_path: &str, file_name: &str, ext: Option<&str>) -> Vec<&Rule> {
        self.rules
            .iter()
            .filter(|r| r.matches(rel_path, file_name, ext))
            .collect()
    }
}

/// Compile one glob.
///
/// `literal_separator(true)` makes `*` stop at `/`, so `clients/*` is one level
/// and `clients/**` is the whole subtree — the distinction a user writing path
/// rules is relying on. Matching is case-insensitive so a rule file moved
/// between Windows and Linux keeps producing the same tags.
fn compile_glob(pattern: &str) -> Result<GlobMatcher> {
    Ok(globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(true)
        .build()
        .map(|g: Glob| g.compile_matcher())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(text: &str) -> RuleSet {
        RuleSet::parse(text).unwrap()
    }

    #[test]
    fn path_glob_matches_a_subtree_and_stops_at_a_separator() {
        let s = set(r#"
            [[rule]]
            path = "clients/*"
            tags = ["scope:one-level"]

            [[rule]]
            path = "clients/**"
            tags = ["scope:subtree"]
            "#);

        let one = s.matches("clients/acme", "acme", None);
        assert_eq!(one.len(), 2, "a direct child matches both globs");

        let deep = s.matches("clients/acme/2024/invoice.pdf", "invoice.pdf", Some("pdf"));
        let names: Vec<&str> = deep
            .iter()
            .flat_map(|r| r.tags())
            .map(|t| t.value())
            .collect();
        assert_eq!(names, vec!["subtree"], "`*` must not cross a separator");
    }

    #[test]
    fn conditions_are_anded() {
        let s = set(r#"
            [[rule]]
            path = "**/keiri/**"
            ext  = ["docx", "xlsx"]
            tags = ["dept:accounting"]
            "#);
        assert_eq!(s.matches("a/keiri/b.docx", "b.docx", Some("docx")).len(), 1);
        // Path matches but the extension does not.
        assert_eq!(s.matches("a/keiri/b.txt", "b.txt", Some("txt")).len(), 0);
        // Extension matches but the path does not.
        assert_eq!(s.matches("a/eigyo/b.docx", "b.docx", Some("docx")).len(), 0);
    }

    #[test]
    fn matching_is_case_insensitive_so_rules_port_between_platforms() {
        let s = set(r#"
            [[rule]]
            path = "Clients/**"
            tags = ["project:client"]
            "#);
        assert_eq!(
            s.matches("clients/acme/x.txt", "x.txt", Some("txt")).len(),
            1
        );
    }

    #[test]
    fn a_rule_without_a_condition_is_rejected() {
        let err = RuleSet::parse(
            r#"
            [[rule]]
            tags = ["everything:yes"]
            "#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("no condition"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_typo_in_a_key_is_an_error_not_a_silently_dead_rule() {
        // `tag` instead of `tags`: the rule would otherwise load and never fire.
        let err = RuleSet::parse(
            r#"
            [[rule]]
            path = "a/**"
            tag  = ["x:y"]
            tags = ["a:b"]
            "#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_malformed_tag_names_the_rule() {
        let err = RuleSet::parse(
            r#"
            [[rule]]
            name = "顧客案件"
            path = "a/**"
            tags = ["no-namespace"]
            "#,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("顧客案件"), "unexpected error: {msg}");
    }

    #[test]
    fn an_empty_file_is_a_valid_empty_rule_set() {
        assert!(RuleSet::parse("").unwrap().is_empty());
    }
}
