//! The one config file: `sagasu.toml` (issue #6, docs/cli.md §5).
//!
//! sagasu used to have two — `sagasu-text.toml` for the body-extraction
//! extension lists and `sagasu-tags.toml` for the user tag rules. The reason
//! they were split (design.md §4-2) was that `sagasu tag` reads one and
//! `sagasu fulltext` reads the other, and "a command called `fulltext` reading
//! a file called `tags`" has no explanation. **Section names answer that just
//! as well**: `[text]` and `[[tags.rule]]` say which half is which without
//! needing two files, two discovery paths and two flags.
//!
//! ```toml
//! [text]
//! text_ext   = ["tmpl", "hbs"]
//! binary_ext = ["dat"]
//!
//! [[tags.rule]]
//! path = "clients/**"
//! tags = ["project:client-work"]
//! ```
//!
//! Both sections are optional. Unknown keys are an error at every level, for
//! the reason spelled out on [`crate::tagrules`]: a file where `text_exts` was
//! typed instead of `text_ext` must not load as a config that does nothing.
//!
//! ## The old files are not read, and not ignored either
//!
//! PoC-stage, so there is no back-compat path: `sagasu-tags.toml` and
//! `sagasu-text.toml` are not read. But **being detected is not the same as
//! being ignored** — [`check_no_legacy_config`] turns their presence into an
//! error that explains the move. Silently not reading a file the user wrote is
//! the same class of failure as a silently truncated result set: what the user
//! sees is "my rules stopped working", with nothing pointing at why.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::tagrules::{RawRule, RuleSet, LEGACY_RULES_FILE};
use crate::text::{TextPolicy, TextSection, LEGACY_TEXT_CONFIG_FILE};

/// Filename looked for when no `--config` is given.
pub const DEFAULT_CONFIG_FILE: &str = "sagasu.toml";

// ── On-disk shape ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    text: Option<TextSection>,
    #[serde(default)]
    tags: Option<TagsSection>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TagsSection {
    #[serde(default)]
    rule: Vec<RawRule>,
}

// ── Loaded form ─────────────────────────────────────────────────────────────

/// How the config file in force was arrived at.
///
/// Reported rather than inferred: "no rules were applied because you have no
/// config file" and "no rules were applied because the one you named is empty"
/// lead to different next moves, and the numbers afterwards look identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigOrigin {
    /// `--config <PATH>` named it.
    Explicit(PathBuf),
    /// `./sagasu.toml` was found without being asked for.
    Discovered(PathBuf),
    /// No file: the built-in behaviour.
    None,
}

impl ConfigOrigin {
    /// The file, if there was one.
    pub fn path(&self) -> Option<&Path> {
        match self {
            ConfigOrigin::Explicit(p) | ConfigOrigin::Discovered(p) => Some(p),
            ConfigOrigin::None => None,
        }
    }

    /// One line naming the file and how it was found, for the report every
    /// command prints before doing any work.
    pub fn describe(&self) -> String {
        match self {
            ConfigOrigin::Explicit(p) => p.display().to_string(),
            ConfigOrigin::Discovered(p) => {
                format!("{} (found in the working directory)", p.display())
            }
            ConfigOrigin::None => {
                format!("(none — pass --config <FILE> or put {DEFAULT_CONFIG_FILE} here)")
            }
        }
    }
}

/// A loaded `sagasu.toml`: both halves, plus where they came from.
#[derive(Debug)]
pub struct Config {
    text: TextPolicy,
    rules: RuleSet,
    origin: ConfigOrigin,
}

impl Config {
    /// The built-in behaviour: no user extensions, no user rules.
    pub fn empty() -> Self {
        Self {
            text: TextPolicy::empty(),
            rules: RuleSet::empty(),
            origin: ConfigOrigin::None,
        }
    }

    /// The extension policy from `[text]`.
    pub fn text_policy(&self) -> &TextPolicy {
        &self.text
    }

    /// Take the extension policy (callers hand it to `FulltextConfig`).
    pub fn into_text_policy(self) -> TextPolicy {
        self.text
    }

    /// Apply `--ext` additions on top of whatever the file said.
    ///
    /// The command line wins because it is the escape hatch: a config file
    /// cannot be edited from inside a pipeline, and an index that was built
    /// with the wrong extension list is exactly when someone needs one.
    pub fn add_text_exts(&mut self, exts: &[String]) {
        self.text.add_text_exts(exts);
    }

    /// The tag rules from `[[tags.rule]]`.
    pub fn rules(&self) -> &RuleSet {
        &self.rules
    }

    /// Take the tag rules.
    pub fn into_rules(self) -> RuleSet {
        self.rules
    }

    /// Where this config came from.
    pub fn origin(&self) -> &ConfigOrigin {
        &self.origin
    }

    /// Load and compile a config file.
    ///
    /// The bytes are read and hashed **once**, and the digest is attached to
    /// both halves — the full-text index and the tag layer each record which
    /// version of the file they were built from, and with one file those two
    /// digests must agree by construction rather than by luck.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let digest = blake3::hash(text.as_bytes()).to_hex().to_string();
        let file: ConfigFile = toml::from_str(&text)
            .with_context(|| format!("invalid config in {}", path.display()))?;

        let text_policy = TextPolicy::from_section(file.text.unwrap_or_default())
            .with_origin(path.to_path_buf(), digest.clone());

        let rules = RuleSet::from_raw(file.tags.unwrap_or_default().rule)
            .with_context(|| format!("invalid tag rules in {}", path.display()))?
            .with_origin(path.to_path_buf(), digest);

        Ok(Self {
            text: text_policy,
            rules,
            origin: ConfigOrigin::Explicit(path.to_path_buf()),
        })
    }

    /// Resolve the config the way every subcommand does it.
    ///
    /// 1. `--config <PATH>` — **an error when missing.** The user named it.
    /// 2. `./sagasu.toml` — read when present, "no config" when not. Having no
    ///    config file is the normal case, not a mistake.
    ///
    /// Before either, the working directory is checked for the two files this
    /// one replaced ([`check_no_legacy_config`]).
    pub fn resolve(explicit: Option<&Path>) -> Result<Self> {
        check_no_legacy_config(Path::new("."))?;

        match explicit {
            Some(path) => {
                if !path.is_file() {
                    bail!(
                        "config file not found: {} (--config named it, so this is an \
                         error rather than a fallback to the built-in behaviour)",
                        path.display()
                    );
                }
                Config::load(path)
            }
            None => {
                let candidate = Path::new(DEFAULT_CONFIG_FILE);
                if candidate.is_file() {
                    let mut config = Config::load(candidate)?;
                    config.origin = ConfigOrigin::Discovered(candidate.to_path_buf());
                    Ok(config)
                } else {
                    Ok(Config::empty())
                }
            }
        }
    }
}

// ── The files this one replaced ─────────────────────────────────────────────

/// Refuse to run while a pre-#6 config file is sitting in `dir`.
///
/// Checked in the working directory — the only place the old files were ever
/// discovered from. An error rather than a warning: a warning scrolls past on a
/// build that then produces a tag layer with none of the user's rules in it,
/// and the tag counts afterwards look perfectly healthy.
pub fn check_no_legacy_config(dir: &Path) -> Result<()> {
    for (legacy, moved_to) in [
        (LEGACY_RULES_FILE, "[[tags.rule]] (was [[rule]])"),
        (
            LEGACY_TEXT_CONFIG_FILE,
            "[text] (was the top-level text_ext / binary_ext)",
        ),
    ] {
        let path = dir.join(legacy);
        if path.is_file() {
            bail!(
                "{legacy} is no longer read: the two config files were merged into a \
                 single {DEFAULT_CONFIG_FILE} (docs/cli.md §5).\n  \
                 Its contents move under {moved_to}.\n  \
                 Move them into {DEFAULT_CONFIG_FILE}, then remove or rename \
                 {legacy}.\n  \
                 (This is an error rather than a silent skip: a config file that is \
                 present but unread produces a build that looks perfectly healthy \
                 and applies none of your settings.)"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::ExtVerdict;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sagasu-config-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn one_file_carries_both_halves() {
        let dir = tmpdir("both");
        let path = write(
            &dir,
            "sagasu.toml",
            r#"
            [text]
            text_ext = ["tmpl"]
            binary_ext = ["pak"]

            [[tags.rule]]
            name = "client work"
            path = "clients/**"
            tags = ["project:client-work"]
            "#,
        );

        let config = Config::load(&path).unwrap();
        assert_eq!(
            config.text_policy().classify_ext(Some("tmpl")),
            ExtVerdict::Text
        );
        assert_eq!(
            config.text_policy().classify_ext(Some("pak")),
            ExtVerdict::Binary
        );
        assert_eq!(config.rules().len(), 1);

        // Same bytes, so the two halves cannot disagree about which version of
        // the file they came from.
        assert_eq!(config.text_policy().digest(), config.rules().digest());
        assert_eq!(config.text_policy().source(), Some(path.as_path()));
    }

    #[test]
    fn either_section_may_be_missing() {
        let dir = tmpdir("partial");
        let text_only = write(&dir, "text-only.toml", "[text]\ntext_ext = [\"tmpl\"]\n");
        let config = Config::load(&text_only).unwrap();
        assert!(config.rules().is_empty());
        assert!(!config.text_policy().is_empty());

        let tags_only = write(
            &dir,
            "tags-only.toml",
            "[[tags.rule]]\nfile = \"*.psd\"\ntags = [\"app:photoshop\"]\n",
        );
        let config = Config::load(&tags_only).unwrap();
        assert_eq!(config.rules().len(), 1);
        assert!(config.text_policy().is_empty());

        // An empty file is legal and means "the built-in behaviour", which is
        // not the same as having no file at all — the origin still names it.
        let empty = write(&dir, "empty.toml", "");
        let config = Config::load(&empty).unwrap();
        assert!(config.rules().is_empty());
        assert!(config.text_policy().is_empty());
        assert_eq!(config.origin().path(), Some(empty.as_path()));
    }

    #[test]
    fn a_typo_is_an_error_rather_than_a_config_that_does_nothing() {
        let dir = tmpdir("typo");

        // The exact failure design.md §4-2 names: `text_exts` for `text_ext`.
        let typo = write(&dir, "typo.toml", "[text]\ntext_exts = [\"tmpl\"]\n");
        assert!(Config::load(&typo).is_err());

        // …and the same at the outer level, and inside a rule.
        let stray = write(&dir, "stray.toml", "[texts]\ntext_ext = [\"tmpl\"]\n");
        assert!(Config::load(&stray).is_err());

        let bad_rule = write(
            &dir,
            "rule.toml",
            "[[tags.rule]]\npath = \"a/**\"\ntag = [\"x:y\"]\n",
        );
        assert!(Config::load(&bad_rule).is_err());
    }

    #[test]
    fn a_rule_with_no_condition_is_still_rejected_through_the_unified_file() {
        let dir = tmpdir("nocond");
        let path = write(&dir, "sagasu.toml", "[[tags.rule]]\ntags = [\"a:b\"]\n");
        let err = Config::load(&path).unwrap_err();
        assert!(
            format!("{err:#}").contains("no condition"),
            "the rule-level diagnostics survive the move into the unified file: {err:#}"
        );
    }

    #[test]
    fn an_old_config_file_is_an_error_not_a_silent_skip() {
        let dir = tmpdir("legacy");

        write(
            &dir,
            "sagasu-tags.toml",
            "[[rule]]\npath = \"a/**\"\ntags = [\"a:b\"]\n",
        );
        let err = check_no_legacy_config(&dir).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("sagasu-tags.toml"), "{msg}");
        assert!(msg.contains("sagasu.toml"), "names where to move it: {msg}");
        assert!(
            msg.contains("[[tags.rule]]"),
            "names the new section: {msg}"
        );

        // …and it stays an error even once the new file exists: a half-migrated
        // tree that looks like it works is worse than one that refuses to run.
        write(&dir, "sagasu.toml", "[text]\ntext_ext = [\"tmpl\"]\n");
        assert!(check_no_legacy_config(&dir).is_err());

        std::fs::remove_file(dir.join("sagasu-tags.toml")).unwrap();
        write(&dir, "sagasu-text.toml", "text_ext = [\"tmpl\"]\n");
        let msg = format!("{:#}", check_no_legacy_config(&dir).unwrap_err());
        assert!(msg.contains("sagasu-text.toml"), "{msg}");
        assert!(msg.contains("[text]"), "{msg}");

        std::fs::remove_file(dir.join("sagasu-text.toml")).unwrap();
        assert!(check_no_legacy_config(&dir).is_ok());
    }

    #[test]
    fn a_named_config_that_does_not_exist_is_an_error() {
        let dir = tmpdir("missing");
        let err = Config::resolve(Some(&dir.join("nope.toml"))).unwrap_err();
        assert!(format!("{err:#}").contains("not found"));
    }
}
