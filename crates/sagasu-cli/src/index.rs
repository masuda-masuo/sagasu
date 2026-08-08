//! The write side of the pipeline: `index` → (`hash`) → `fulltext`.
//!
//! Three subcommands in one module because they are one sequence. Each reads
//! what the previous one wrote — `hash` and `fulltext` both work from the live
//! rows of the metadata index rather than walking the filesystem again — so they
//! share one exclusion set and one set of stable file IDs, and each ends the same
//! way: a summary of what it did, and a non-zero exit when the answer was "no
//! work was performed", which is the failure that is otherwise invisible until
//! query time.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

use sagasu_core::config::Config;
use sagasu_core::fulltext::{self, FulltextConfig};
use sagasu_core::text::TextPolicy;
use sagasu_core::walk::{ExcludeSet, HiddenPolicy};
use sagasu_core::CrawlConfig;

use crate::json;
use crate::output::{mib, Output, Report};
use crate::{Outcome, DEFAULT_INDEX_DIR};

/// How many entries of the skipped-extension breakdown to print.
const SKIPPED_EXT_ROWS: usize = 8;

// ── index ───────────────────────────────────────────────────────────────────

#[derive(Parser)]
pub struct IndexArgs {
    /// Root directory to crawl.
    root: PathBuf,

    /// Path to the SQLite database file.
    #[arg(long, default_value = "index.db")]
    db: PathBuf,

    /// Additional directory basename to exclude (repeatable).
    #[arg(long = "exclude")]
    exclude: Vec<String>,

    /// Drop the built-in exclusion list (node_modules, target, .git, ...).
    #[arg(long)]
    no_default_excludes: bool,

    /// Skip entries the OS marks hidden. Windows only in effect: a leading dot
    /// is a naming convention, not a hidden attribute, and `.github/` and
    /// `.config/` stay indexed on every platform.
    #[arg(long)]
    skip_hidden: bool,

    /// Also apply the crawl root's .gitignore — directory rules only. Off by
    /// default: "do not commit this" is not "do not find this".
    #[arg(long)]
    use_gitignore: bool,

    /// Number of walker threads (0 = auto).
    #[arg(long, default_value_t = 0)]
    threads: usize,
}

pub fn cmd_index(args: IndexArgs, mode: Output) -> Result<Outcome> {
    let mut report = Report::new(mode);
    let root = args
        .root
        .canonicalize()
        .with_context(|| format!("root not found: {}", args.root.display()))?;

    // Warn when the database would live inside the crawl tree: the walker
    // would otherwise see the DB file (and its WAL/SHM siblings) and re-index
    // a file that changes on every scan. The core crawl skips those files, but
    // placing the database outside the tree is the supported configuration.
    let db_canon = sagasu_core::walk::canonical_db_path(&args.db);
    if db_canon.starts_with(&root) {
        report.warn(format!(
            "database {db_canon:?} is inside the crawl root {root:?}; the database \
             file will be excluded from the index, but placing it outside the \
             crawl tree is recommended."
        ));
    }

    let hidden = if args.skip_hidden {
        HiddenPolicy::SkipOsHidden
    } else {
        HiddenPolicy::Include
    };

    let config = CrawlConfig {
        root: root.clone(),
        db_path: args.db,
        exclude: args.exclude.clone(),
        no_default_excludes: args.no_default_excludes,
        hidden,
        use_gitignore: args.use_gitignore,
        threads: args.threads,
    };

    // What the crawl is about to consider out of scope, before it says how much
    // that came to. A count of exclusions without the rule that produced them
    // cannot be argued with; the rule without the count cannot be believed.
    //
    // Built once and used by both renderings. The human one prints it *before*
    // the walk so a wrong root can be interrupted; the machine one carries it
    // in the single object at the end, where the ordering buys nothing.
    let excludes = scope(&config, &root)?;
    if !report.is_json() {
        print_scope(&root, &excludes);
    }

    let summary = sagasu_core::walk::crawl(config)?;

    if !report.is_json() {
        print_crawl_summary(&summary);
    }

    if summary.errors > 0 {
        report.warn(format!(
            "{} entr(ies) could not be read and are missing from the index \
             along with anything below them. They are not excluded — they were \
             unreachable — so re-running after fixing permissions will change the \
             file count.",
            summary.errors
        ));
    }

    // Zero files indexed = warning + non-zero exit.
    if summary.indexed == 0 {
        report.warn(
            "zero files indexed. Check that the root directory is \
             correct and not entirely excluded.",
        );
    }

    if report.is_json() {
        json::index(&root.display().to_string(), &excludes, &summary, &report);
    }

    // Zero files indexed was exit 1 under the old 2-value contract; it is a
    // failure ("no work was performed"), not a legitimate empty answer, so it
    // moves to 2 with the rest. The warning above already said why.
    if summary.indexed == 0 {
        Ok(Outcome::Unusable)
    } else {
        Ok(Outcome::Success)
    }
}

/// The human rendering of what the crawl did.
fn print_crawl_summary(summary: &sagasu_core::CrawlSummary) {
    println!("scanned      : {}", summary.scanned);
    println!("indexed      : {}", summary.indexed);
    println!("  added      : {}", summary.added);
    println!("  changed    : {}", summary.changed);
    println!("  renamed    : {}", summary.renamed);
    println!("  deleted    : {}", summary.deleted);

    let skipped_total = summary.skipped_total();
    if skipped_total > 0 {
        println!("skipped      : {skipped_total}");
        // Sort by count descending, then by name.
        let mut skips: Vec<_> = summary.skipped.iter().collect();
        skips.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        for (name, count) in skips {
            println!("  {name}: {count}");
        }
        if summary.skipped_hidden > 0 {
            println!("  (os hidden): {}", summary.skipped_hidden);
        }
        if summary.skipped_gitignore > 0 {
            println!("  (gitignore): {}", summary.skipped_gitignore);
        }
    }

    // Not an exclusion: nobody asked for these to be dropped. An unreadable
    // directory takes its whole subtree with it, and without this line the
    // summary still adds up and the exit code is still 0.
    if summary.errors > 0 {
        println!("unreadable   : {}", summary.errors);
        for sample in &summary.error_samples {
            println!("  {sample}");
        }
        if summary.errors as usize > summary.error_samples.len() {
            println!(
                "  ({} more)",
                summary.errors as usize - summary.error_samples.len()
            );
        }
    }

    println!("elapsed      : {:.3}s", summary.elapsed_secs);
}

/// Assemble the exclusion policy the crawl is about to run under.
///
/// Rebuilding the [`ExcludeSet`] here rather than having `crawl` hand one back
/// costs a `.gitignore` read; the alternative is reporting the *arguments* and
/// hoping they describe the same thing the core assembled. It also surfaces a
/// broken `.gitignore` before the walk instead of after it.
fn scope(config: &CrawlConfig, root: &Path) -> Result<ExcludeSet> {
    let excludes = ExcludeSet::new(&config.exclude, config.no_default_excludes)
        .with_hidden(config.hidden)
        .with_gitignore(root, config.use_gitignore)?;
    excludes.validate()?;
    Ok(excludes)
}

/// Print the exclusion policy the crawl is about to run under.
fn print_scope(root: &Path, excludes: &ExcludeSet) {
    println!("root         : {}", root.display());
    if excludes.names().is_empty() {
        println!("excluded dirs: (none)");
    } else {
        println!("excluded dirs: {}", excludes.names().join(", "));
    }
    println!(
        "hidden       : {}",
        match excludes.hidden_policy() {
            HiddenPolicy::Include => "indexed (dot-directories are content)".to_string(),
            // Saying "skipped" on a platform with no hidden attribute would be a
            // claim about a filter that cannot fire.
            HiddenPolicy::SkipOsHidden if cfg!(windows) =>
                "skipped when the OS marks them hidden".to_string(),
            HiddenPolicy::SkipOsHidden => format!(
                "--skip-hidden has no effect on {}: only Windows has a hidden attribute \
                 (a leading dot is not one)",
                std::env::consts::OS
            ),
        }
    );
    println!(
        "gitignore    : {}",
        if excludes.uses_gitignore() {
            format!(
                "{} rule(s) from the root .gitignore, directories only{}",
                excludes.gitignore_rules(),
                match excludes.gitignore_digest() {
                    // The rules are copied into the index, so the file may
                    // change afterwards without changing the answer. The digest
                    // is what lets someone check which version was baked in.
                    Some(d) => format!(" (digest {})", &d[..d.len().min(12)]),
                    None => " (no .gitignore at the root)".to_string(),
                }
            )
        } else {
            "not applied".to_string()
        }
    );
}

// ── hash ────────────────────────────────────────────────────────────────────

#[derive(Parser)]
pub struct HashArgs {
    /// Path to the SQLite database file.
    #[arg(long, default_value = "index.db")]
    db: PathBuf,

    /// Skip files larger than this (bytes). Default 4 MiB.
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    max_size: u64,
}

pub fn cmd_hash(args: HashArgs, mode: Output) -> Result<Outcome> {
    let report = Report::new(mode);
    let summary = sagasu_core::walk::hash_backfill(&args.db, args.max_size)?;

    if report.is_json() {
        json::hash(&summary, &report);
    } else {
        println!("hashed             : {}", summary.hashed);
        println!("skipped (too large): {}", summary.skipped_too_large);
        println!("skipped (unreadable): {}", summary.skipped_unreadable);
    }

    // `hash` has no empty-answer concept: hashing nothing (everything already
    // hashed) is a success, not an empty answer.
    Ok(Outcome::Success)
}

// ── fulltext ────────────────────────────────────────────────────────────────

#[derive(Parser)]
pub struct FulltextArgs {
    /// Path to the SQLite database file (source of the files to index).
    #[arg(long, default_value = "index.db")]
    db: PathBuf,

    /// Directory the tantivy index is (re)built in.
    #[arg(long, default_value = DEFAULT_INDEX_DIR)]
    index_dir: PathBuf,

    /// Skip files larger than this (bytes). Default 2 MiB.
    #[arg(long, default_value_t = fulltext::DEFAULT_MAX_SIZE)]
    max_size: u64,

    /// Additional extension to treat as text (repeatable, no leading dot).
    #[arg(long = "ext")]
    ext: Vec<String>,

    /// Config file whose `[text]` section extends the extension lists
    /// (default: ./sagasu.toml when present). See docs/cli.md §5.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Removed in issue #6: the two config files were merged into `sagasu.toml`
    /// and this flag became `--config`.
    #[arg(long = "text-config", hide = true)]
    text_config: Option<PathBuf>,

    /// Only trust the extension allowlist; do not sniff unknown formats.
    #[arg(long)]
    no_sniff: bool,

    /// Number of file-reading threads (0 = auto).
    #[arg(long, default_value_t = 0)]
    threads: usize,

    /// tantivy writer memory budget in MiB.
    #[arg(long, default_value_t = (fulltext::DEFAULT_HEAP_BYTES / (1024 * 1024)) as u64)]
    heap_mb: u64,
}

/// Refuse a flag that issue #6 removed, by name, with the replacement.
///
/// clap's own "unexpected argument" would be technically correct and useless:
/// the user's next question is "then how do I point at my rules", and the
/// answer is one word. Declared hidden so it does not clutter `--help` with a
/// flag nobody should learn.
pub(crate) fn reject_removed_config_flag(old: &str, value: Option<&Path>) -> Result<()> {
    if value.is_some() {
        anyhow::bail!(
            "{old} was removed in issue #6: the two config files were merged into a \
             single sagasu.toml, so there is one flag for it — `--config <FILE>` \
             (docs/cli.md §5)."
        );
    }
    Ok(())
}

/// Resolve the config file, then apply the `--ext` flags on top.
///
/// The command line is applied last so it wins over the file: an already-built
/// index is the thing `--ext` is an escape hatch from, and a config file cannot
/// be edited from inside a pipeline.
pub(crate) fn load_config(explicit: Option<&Path>, exts: &[String]) -> Result<Config> {
    let mut config = Config::resolve(explicit)?;
    config.add_text_exts(exts);
    Ok(config)
}

pub fn cmd_fulltext(args: FulltextArgs, mode: Output) -> Result<Outcome> {
    let mut report = Report::new(mode);
    reject_removed_config_flag("--text-config", args.text_config.as_deref())?;
    let loaded = load_config(args.config.as_deref(), &args.ext)?;
    let origin = loaded.origin().clone();
    let text_policy = loaded.into_text_policy();

    let config = FulltextConfig {
        db_path: args.db,
        index_dir: args.index_dir,
        max_size: args.max_size,
        text_policy,
        no_sniff: args.no_sniff,
        threads: args.threads,
        heap_bytes: (args.heap_mb as usize) * 1024 * 1024,
    };

    if !report.is_json() {
        println!("config       : {}", origin.describe());
        print_text_policy(&config.text_policy);
    }

    let summary = fulltext::build(&config)?;

    if !report.is_json() {
        print_fulltext_summary(&summary, &config);
    }

    // An empty index is the failure that is hardest to notice from the outside:
    // "indexed but not findable" and "never indexed" look identical at search
    // time. Say so, and exit non-zero.
    if summary.indexed == 0 {
        report.warn(
            "zero documents indexed. Every candidate was skipped (see the \
             reasons above), or the metadata index is empty — run `sagasu index \
             <root>` first, and consider `--ext <EXT>` if your text files use an \
             extension sagasu does not know.",
        );
    }

    if report.is_json() {
        json::fulltext(
            &config.index_dir.display().to_string(),
            &origin,
            &config.text_policy,
            &summary,
            &report,
        );
    }

    // Zero documents indexed was exit 1 under the old 2-value contract; it is
    // a failure ("indexed but not findable"), not an empty answer, so it moves
    // to 2 with the rest. The warning above already said why.
    if summary.indexed == 0 {
        Ok(Outcome::Unusable)
    } else {
        Ok(Outcome::Success)
    }
}

/// The human rendering of a full-text build.
fn print_fulltext_summary(summary: &fulltext::FulltextSummary, config: &FulltextConfig) {
    println!("candidates   : {}", summary.candidates);
    println!("indexed      : {}", summary.indexed);
    println!("  by ext     : {}", summary.accepted_by_ext);
    println!("  by sniff   : {}", summary.accepted_by_sniff);
    println!("  by extract : {}", summary.accepted_by_extract);

    // Nothing is dropped silently: every candidate is either indexed or shows
    // up here with a reason.
    if !summary.skipped.is_empty() {
        println!("skipped      : {}", summary.skipped_total());
        for (reason, count) in &summary.skipped {
            println!("  {}: {count}", reason.as_str());
        }
    }

    // An extraction failure is the one skip reason that names a *file* rather
    // than a class of files, and it is the one where the reason is the whole
    // information: "3 documents failed" is not actionable, "this PDF has no
    // page tree" is.
    if !summary.extract_errors.is_empty() {
        println!("  extraction failures:");
        for (path, reason) in &summary.extract_errors {
            println!("    {path}: {reason}");
        }
        let failed = summary
            .skipped
            .get(&fulltext::SkipReason::ExtractFailed)
            .copied()
            .unwrap_or(0) as usize;
        if failed > summary.extract_errors.len() {
            println!("    (… and {} more)", failed - summary.extract_errors.len());
        }
    }

    // Documents whose body had to be broken up so Lindera's Viterbi lattice
    // stayed bounded (issue #52). Reported rather than silent because it is the
    // one case where the indexed text is not byte-for-byte the file, and
    // because a document in this list is a document that used to lose its tail.
    if summary.lattice_split_docs > 0 {
        println!(
            "long lines   : {} document(s) split into {} segment breaks",
            summary.lattice_split_docs,
            summary.lattice_breaks
        );
        for (path, breaks) in &summary.lattice_split_samples {
            println!("    {path}: {breaks} break(s)");
        }
        let more = summary.lattice_split_docs as usize - summary.lattice_split_samples.len();
        if more > 0 {
            println!("    (… and {more} more)");
        }
    }

    // Should never fire: the split above makes an over-long token unreachable.
    // tantivy drops these with a `warn!` nobody sees, which is what let issue
    // #52 hide; if the count is ever non-zero, say so loudly.
    if summary.dropped_long_tokens > 0 {
        println!(
            "dropped terms: {} token(s) exceeded tantivy's limit and were not \
             indexed (longest {} bytes) — please report this",
            summary.dropped_long_tokens, summary.longest_token_bytes
        );
    }

    // …and the format skips are broken down by extension, because that is the
    // form the user can act on: `--ext mjs`, or a line in the text config.
    if !summary.skipped_exts.is_empty() {
        println!("  by extension:");
        for (ext, count) in summary.skipped_exts.iter().take(SKIPPED_EXT_ROWS) {
            let label = if ext.is_empty() {
                "(no extension)".to_string()
            } else {
                format!(".{ext}")
            };
            println!("    {label}: {count}");
        }
        let shown = SKIPPED_EXT_ROWS.min(summary.skipped_exts.len());
        if summary.skipped_exts.len() > shown {
            println!(
                "    ({} more extension(s))",
                summary.skipped_exts.len() - shown
            );
        }
    }

    println!("text bytes   : {:.1} MiB", mib(summary.text_bytes));
    println!(
        "index size   : {:.1} MiB ({})",
        mib(summary.index_bytes),
        config.index_dir.display()
    );
    if summary.text_bytes > 0 {
        println!(
            "index ratio  : {:.0}% of extracted text",
            100.0 * summary.index_bytes as f64 / summary.text_bytes as f64
        );
    }
    println!("elapsed      : {:.3}s", summary.elapsed_secs);
}

/// Say which extension policy the build ran under.
///
/// Printed even when it is empty. The lists are the reason a file is or is not
/// in the index, and "I added `.tmpl` to a config file the tool never read" is
/// otherwise indistinguishable from "sagasu ignored my `.tmpl` files".
fn print_text_policy(policy: &TextPolicy) {
    if policy.is_empty() {
        // Which file was read is the line above this one; this one says what
        // the file changed, and "nothing" has to be said out loud.
        println!("text         : (built-in lists only)");
        return;
    }
    if !policy.text_exts().is_empty() {
        println!("  +text      : {}", policy.text_exts().join(", "));
    }
    if !policy.binary_exts().is_empty() {
        println!("  +binary    : {}", policy.binary_exts().join(", "));
    }
}
