//! `sagasu tag` (generate) and `sagasu tags` (browse) — the CLI surface of the
//! rule-based tag engine (design.md §6, issue #4).
//!
//! Two commands rather than one because they are two different things: `tag` is
//! a pipeline stage next to `hash` and `fulltext`, and `tags` is a read-only
//! query like `find`. They live in their own module so that adding a facet
//! browser later does not keep growing `main.rs`.

use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use clap::Parser;

use sagasu_core::store::Store;
use sagasu_core::tagindex::{self, TagConfig};
use sagasu_core::tagrules::{RuleSet, DEFAULT_RULES_FILE};
use sagasu_core::tags::{self, Tag, TagSource};

// ── sagasu tag ──────────────────────────────────────────────────────────────

/// Arguments of `sagasu tag`.
#[derive(Parser)]
pub struct TagArgs {
    /// Path to the SQLite database file.
    #[arg(long, default_value = "index.db")]
    db: PathBuf,

    /// User-defined rule file (TOML). Defaults to `./sagasu-tags.toml` when it
    /// exists; the summary always says which file was used, or that none was.
    #[arg(long)]
    rules: Option<PathBuf>,

    /// Do **not** read the leading bytes of files whose `magic` column is NULL.
    ///
    /// Reading them is the default: `format:` is otherwise the extension's own
    /// word for itself, and — worse — a file whose content was edited since the
    /// last crawl has had `magic` nulled, so the default would *lose* a correct
    /// `format:png` + `anomaly:format-mismatch` and replace it with a wrong
    /// `format:jpg`. 512 bytes per file is cheap enough (measured at 1.85s →
    /// 3.65s over 63,901 files) that paying it always beats being quietly wrong.
    #[arg(long)]
    no_read_magic: bool,

    /// Unless `--no-read-magic`, skip reading the head of files larger than
    /// this (bytes).
    #[arg(long, default_value_t = u64::MAX)]
    magic_max_size: u64,
}

/// Resolve the rule file: explicit flag, else the conventional file in the
/// working directory, else none. Returns the path and whether it was discovered
/// rather than named.
fn resolve_rules(explicit: Option<PathBuf>) -> (Option<PathBuf>, bool) {
    match explicit {
        Some(p) => (Some(p), false),
        None => {
            let candidate = PathBuf::from(DEFAULT_RULES_FILE);
            if candidate.is_file() {
                (Some(candidate), true)
            } else {
                (None, false)
            }
        }
    }
}

/// Run `sagasu tag`.
pub fn cmd_tag(args: TagArgs) -> Result<()> {
    let (rules_path, discovered) = resolve_rules(args.rules);

    // Say what rule set is in force *before* the pass, not only after: a run
    // that quietly used no rules and a run that used the wrong ones look the
    // same in the numbers afterwards.
    match (&rules_path, discovered) {
        (Some(p), true) => println!(
            "rules        : {} (found in the working directory)",
            p.display()
        ),
        (Some(p), false) => println!("rules        : {}", p.display()),
        (None, _) => {
            println!("rules        : (none — pass --rules <FILE> or put {DEFAULT_RULES_FILE} here)")
        }
    }

    let config = TagConfig {
        db_path: args.db,
        rules_path,
        read_magic: !args.no_read_magic,
        magic_max_size: args.magic_max_size,
    };

    let summary = tagindex::build(&config)?;

    println!("files        : {}", summary.files);
    println!(
        "tagged       : {} ({:.1}%)",
        summary.tagged,
        summary.coverage()
    );
    // The headline number for issue #4's acceptance criterion. Reported next to
    // the raw coverage because raw coverage is ~100% by construction: every file
    // with an extension gets `ext:`/`kind:`, and every nested file gets `path:`.
    // Quoting only that would be a measurement that cannot fail.
    println!(
        "  semantic   : {} ({:.1}%) — at least one tag outside {}",
        summary.tagged_semantic,
        summary.semantic_coverage(),
        tags::STRUCTURAL_NAMESPACES.join("/"),
    );
    println!("tag rows     : {}", summary.rows);
    println!("distinct tags: {}", summary.distinct);
    println!("rules loaded : {}", summary.rules_count);

    // Format tags are only as good as the bytes behind them. A large `missing`
    // count means `format:` was guessed from extensions, which is exactly the
    // kind of quiet degradation that must be visible.
    println!(
        "magic bytes  : {} present, {} missing, {} read now, {} unreadable",
        summary.magic_present, summary.magic_missing, summary.magic_read, summary.magic_unreadable
    );
    if summary.magic_missing > 0 && !config.read_magic {
        println!(
            "               (--no-read-magic was passed: `format:` is the extension's \
             own word for itself, and a file edited since the last crawl has lost \
             the bytes that contradicted it)"
        );
    }

    // A cap that is only visible as "N files were capped" is a silent omission
    // with a footnote. Name the namespaces it came out of, so a user can see at
    // a glance whether the axis they care about was the one that paid.
    if summary.capped > 0 {
        println!(
            "capped       : {} files hit the {}-tag limit, {} tags dropped",
            summary.capped,
            tags::MAX_TAGS_PER_FILE,
            summary.capped_dropped,
        );
        let mut by_ns: Vec<_> = summary.capped_dropped_namespaces.iter().collect();
        by_ns.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        for (ns, n) in by_ns {
            println!("               {ns}: {n}");
        }
    }

    println!("namespaces   :");
    let mut by_files: Vec<_> = summary.namespaces.iter().collect();
    by_files.sort_by(|a, b| b.1.files.cmp(&a.1.files).then_with(|| a.0.cmp(b.0)));
    for (ns, stat) in by_files {
        println!(
            "  {ns:<9}: {:>7} files ({:>5.1}%), {} values, {} rows",
            stat.files,
            summary.namespace_coverage(ns),
            stat.distinct,
            stat.rows,
        );
    }

    println!("elapsed      : {:.3}s", summary.elapsed_secs);

    // An index whose tags are all empty is indistinguishable at query time from
    // one that was never tagged. Say which it is.
    if summary.files == 0 {
        eprintln!(
            "WARNING: the metadata index holds no live files. Run `sagasu index <root>` first."
        );
        process::exit(1);
    }
    if summary.tagged == 0 {
        eprintln!("WARNING: no file received a tag. This is almost certainly a bug — report it.");
        process::exit(1);
    }

    Ok(())
}

// ── sagasu tags ─────────────────────────────────────────────────────────────

/// Arguments of `sagasu tags`.
#[derive(Parser)]
pub struct TagsArgs {
    /// Tags to filter by, `namespace:value`, ANDed. A bare `namespace:` lists
    /// the values in that namespace. With no query at all, the namespaces
    /// themselves are listed.
    query: Vec<String>,

    /// Path to the SQLite database file.
    #[arg(long, default_value = "index.db")]
    db: PathBuf,

    /// Explain one file: its stored tags next to the tags the engine produces
    /// for it right now. A difference means the stored layer is out of date.
    #[arg(long)]
    file: Option<PathBuf>,

    /// Rule file used when explaining a file (same defaulting as `sagasu tag`).
    #[arg(long)]
    rules: Option<PathBuf>,

    /// Maximum number of rows.
    #[arg(long, short = 'n', default_value_t = 20)]
    limit: usize,
}

/// Run `sagasu tags`.
pub fn cmd_tags(args: TagsArgs) -> Result<()> {
    let store = Store::open(&args.db)
        .with_context(|| format!("failed to open metadata index {:?}", args.db))?;

    if let Some(file) = &args.file {
        return explain_file(&store, file, args.rules);
    }

    let stats = store.get_stats()?;
    report_tag_freshness(&stats);

    // `namespace:` (empty value) means "list this namespace".
    let namespace_only: Option<&str> = match args.query.as_slice() {
        [one] if one.ends_with(':') => Some(one.trim_end_matches(':')),
        _ => None,
    };

    if args.query.is_empty() {
        list_namespaces(&store, args.limit)?;
        return Ok(());
    }
    if let Some(ns) = namespace_only {
        list_tags(&store, Some(ns), args.limit)?;
        return Ok(());
    }

    // Otherwise: files carrying all of the given tags.
    let tags: Vec<Tag> = args
        .query
        .iter()
        .map(|t| Tag::parse(t))
        .collect::<Result<Vec<_>>>()?;

    let rows = tagindex::files_with_tags(&store, &tags, args.limit as i64)?;
    let labels: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
    println!("tags    : {}", labels.join(" AND "));
    println!("hits    : {}", rows.len());
    for row in &rows {
        println!("{:>8}  {}", row.file_id, row.path);
    }
    if rows.is_empty() {
        println!("          (no live file carries all of these tags)");
    }
    Ok(())
}

/// Print the namespace overview: the top level of the facet tree.
fn list_namespaces(store: &Store, limit: usize) -> Result<()> {
    let namespaces = tagindex::namespace_counts(store)?;
    if namespaces.is_empty() {
        eprintln!("WARNING: no tags in this index. Run `sagasu tag` first.");
        process::exit(1);
    }
    println!("namespaces:");
    for ns in &namespaces {
        println!(
            "  {:<9}: {:>7} files, {} values",
            ns.namespace, ns.files, ns.distinct
        );
    }
    println!();
    println!("top tags (all namespaces):");
    for tc in tagindex::tag_counts(store, None, limit as i64)? {
        println!("  {:>7}  {}", tc.files, tc.tag);
    }
    Ok(())
}

/// Print the values inside one namespace.
fn list_tags(store: &Store, namespace: Option<&str>, limit: usize) -> Result<()> {
    let counts = tagindex::tag_counts(store, namespace, limit as i64)?;
    match namespace {
        Some(ns) => println!("namespace: {ns}"),
        None => println!("namespace: (all)"),
    }
    if counts.is_empty() {
        println!("  (no tags — is the namespace spelled correctly? `sagasu tags` lists them)");
        return Ok(());
    }
    for tc in counts {
        println!("  {:>7}  {}", tc.files, tc.tag);
    }
    Ok(())
}

/// Explain one file: stored tags beside freshly computed ones.
fn explain_file(store: &Store, file: &Path, rules: Option<PathBuf>) -> Result<()> {
    let (rules_path, _) = resolve_rules(rules);
    let rules = match &rules_path {
        Some(p) => RuleSet::load(p)?,
        None => RuleSet::empty(),
    };

    // Canonicalize when we can: `files.path` holds canonical paths, so a
    // relative argument would otherwise never match a row.
    let path = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let path_str = path.to_string_lossy().into_owned();
    let root = store.meta_get("root_path")?;

    println!("file    : {path_str}");
    println!(
        "rules   : {}",
        rules_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none)".to_string())
    );

    let row = tagindex::file_by_path(store, &path_str)?;
    match &row {
        Some(row) => println!("index   : file_id {} (live)", row.file_id),
        None => println!("index   : not in the metadata index"),
    }

    println!("stored  :");
    match &row {
        Some(row) => {
            let stored = tagindex::tags_of_file(store, row.file_id)?;
            if stored.is_empty() {
                println!("  (none — has `sagasu tag` run since this file was indexed?)");
            }
            for (tag, sources) in &stored {
                println!(
                    "  {:<32} [{}]",
                    tag.to_string(),
                    TagSource::describe(*sources).join(",")
                );
            }
        }
        None => println!("  (n/a)"),
    }

    // Compute from the engine. The stored magic bytes are preferred; failing
    // that the head of the file is read, so the explanation matches what a
    // `--read-magic` build would produce rather than a weaker extension guess.
    let computed = tagindex::explain(&path, root.as_deref(), &rules, true)?;
    println!("computed:");
    for (tag, sources) in &computed.tags {
        println!(
            "  {:<32} [{}]",
            tag.to_string(),
            TagSource::describe(*sources).join(",")
        );
    }
    // The cap applies here exactly as it does to the stored layer — showing an
    // uncapped `computed` list would make the two columns disagree for a reason
    // that has nothing to do with staleness. Instead the dropped tags are named
    // underneath, so the difference between "the engine never produced this" and
    // "the engine produced it and the cap took it" is visible.
    if computed.capped {
        println!(
            "  ({} tags dropped by the {}-tag cap:)",
            computed.dropped.len(),
            tags::MAX_TAGS_PER_FILE
        );
        for (tag, sources) in &computed.dropped {
            println!(
                "  - {:<30} [{}]",
                tag.to_string(),
                TagSource::describe(*sources).join(",")
            );
        }
    }
    Ok(())
}

/// Warn when the stored tags are older than the metadata index they describe.
///
/// A tag filter answering from a layer built two crawls ago silently omits
/// every file added since. Saying so is the whole difference between a stale
/// answer and a wrong one.
fn report_tag_freshness(stats: &sagasu_core::store::IndexStats) {
    match stats.tag_scan_generation {
        None => {
            println!("tags    : (never built — run `sagasu tag`)");
        }
        Some(gen) => {
            let behind = stats.scan_generation - gen;
            let tagged = stats.tag_files.unwrap_or(0);
            if behind > 0 {
                println!(
                    "tags    : {} rows over {} files, built at scan generation {gen} \
                     ({behind} scan(s) behind — re-run `sagasu tag`)",
                    stats.tag_rows, tagged
                );
            } else {
                println!(
                    "tags    : {} rows over {} files, {} distinct (current)",
                    stats.tag_rows, tagged, stats.distinct_tags
                );
            }
            if let Some(rules) = &stats.tag_rules {
                if !rules.is_empty() {
                    println!("rules   : {rules}");
                }
            }
        }
    }
}
