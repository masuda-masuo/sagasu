//! `sagasu tag` (generate) and `sagasu tags` (browse) — the CLI surface of the
//! rule-based tag engine (design.md §6, issue #4).
//!
//! Two commands rather than one because they are two different things: `tag` is
//! a pipeline stage next to `hash` and `fulltext`, and `tags` is a read-only
//! query like `find`. The facet browser that grew out of them is
//! [`crate::browse`]; the snapshot/delta block all three of them owe the user
//! lives in [`crate::output`], so it cannot be updated in one and forgotten in
//! the others.

use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use clap::Parser;

use sagasu_core::config::Config;
use sagasu_core::delta;
use sagasu_core::store::Store;
use sagasu_core::tagindex::{self, TagConfig};
use sagasu_core::tags::{self, Tag, TagSource};

use crate::index::{load_config, reject_removed_config_flag};
use crate::json;
use crate::output::{
    print_tag_freshness, tag_freshness, Output, Report, TagFreshness, TagFreshnessReport,
};

// ── sagasu tag ──────────────────────────────────────────────────────────────

/// Arguments of `sagasu tag`.
#[derive(Parser)]
pub struct TagArgs {
    /// Path to the SQLite database file.
    #[arg(long, default_value = "index.db")]
    db: PathBuf,

    /// Config file whose `[[tags.rule]]` tables define the user rules
    /// (default: ./sagasu.toml when it exists). The summary always says which
    /// file was used, or that none was. See docs/cli.md §5.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Removed in issue #6: the two config files were merged into `sagasu.toml`
    /// and this flag became `--config`.
    #[arg(long, hide = true)]
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

    /// Do **not** read embedded metadata (Office document properties, PDF info,
    /// EXIF) from the formats that carry it.
    ///
    /// Reading it is the default: `author:` / `title:` / `camera:` have no
    /// other source, and a person looking for "the deck 山田 wrote" cannot get
    /// there from the file name. Unlike the 512-byte head read this one opens
    /// and parses the container, so it is the flag to reach for when a pass
    /// over a document-heavy tree is too slow.
    #[arg(long)]
    no_read_embedded: bool,

    /// Unless `--no-read-embedded`, skip embedded metadata for files larger
    /// than this (bytes).
    #[arg(long, default_value_t = tagindex::DEFAULT_EMBEDDED_MAX_SIZE)]
    embedded_max_size: u64,
}

/// Run `sagasu tag`.
pub fn cmd_tag(args: TagArgs, mode: Output) -> Result<()> {
    let mut report = Report::new(mode);
    reject_removed_config_flag("--rules", args.rules.as_deref())?;

    // Resolved here rather than inside the build so the report can name the
    // file *before* the pass, not only after: a run that quietly used no rules
    // and a run that used the wrong ones look the same in the numbers.
    // `Config::resolve` is also what refuses to run next to a pre-#6 config
    // file, and that has to happen before anything is written.
    let origin = Config::resolve(args.config.as_deref())?.origin().clone();

    if !report.is_json() {
        println!("config       : {}", origin.describe());
    }

    let config = TagConfig {
        db_path: args.db,
        rules_path: origin.path().map(|p| p.to_path_buf()),
        read_magic: !args.no_read_magic,
        magic_max_size: args.magic_max_size,
        read_embedded: !args.no_read_embedded,
        embedded_max_size: args.embedded_max_size,
    };

    let summary = tagindex::build(&config)?;

    if !report.is_json() {
        print_tag_summary(&summary, &config);
    }

    // An index whose tags are all empty is indistinguishable at query time from
    // one that was never tagged. Say which it is.
    if summary.files == 0 {
        report.warn("the metadata index holds no live files. Run `sagasu index <root>` first.");
    } else if summary.tagged == 0 {
        report.warn("no file received a tag. This is almost certainly a bug — report it.");
    }

    if report.is_json() {
        json::tag(
            &origin,
            &summary,
            config.read_magic,
            config.read_embedded,
            &report,
        );
    }

    if summary.files == 0 || summary.tagged == 0 {
        process::exit(1);
    }

    Ok(())
}

/// The human rendering of a tag build.
fn print_tag_summary(summary: &sagasu_core::TagSummary, config: &TagConfig) {
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

    // Same discipline as the magic line: the `author:` / `title:` / `camera:`
    // axes are only as complete as the documents that were actually opened, so
    // the denominator and the failures are printed next to the successes rather
    // than left for the user to infer from a tag count.
    if summary.embedded_candidates > 0 {
        println!(
            "embedded meta: {} candidates, {} with metadata, {} failed",
            summary.embedded_candidates, summary.embedded_read, summary.embedded_failed
        );
        if !config.read_embedded {
            println!(
                "               (--no-read-embedded was passed: author:/title:/camera: \
                 have no other source)"
            );
        }
        for (path, reason) in &summary.embedded_errors {
            println!("               ! {path}: {reason}");
        }
        if summary.embedded_failed as usize > summary.embedded_errors.len() {
            println!(
                "               … and {} more",
                summary.embedded_failed as usize - summary.embedded_errors.len()
            );
        }
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

    /// Config file used when explaining a file (same defaulting as
    /// `sagasu tag`). See docs/cli.md §5.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Removed in issue #6: the two config files were merged into `sagasu.toml`
    /// and this flag became `--config`.
    #[arg(long, hide = true)]
    rules: Option<PathBuf>,

    /// Maximum number of rows.
    #[arg(long, short = 'n', default_value_t = 20)]
    limit: usize,

    /// Skip the delta probe that reports how much the filesystem has moved
    /// since the tag layer was built.
    #[arg(long)]
    no_fresh: bool,

    /// Give up the delta probe above this many changed files.
    #[arg(long, default_value_t = delta::DEFAULT_DELTA_LIMIT)]
    delta_limit: usize,
}

/// Run `sagasu tags`.
pub fn cmd_tags(args: TagsArgs, mode: Output) -> Result<()> {
    let mut report = Report::new(mode);
    reject_removed_config_flag("--rules", args.rules.as_deref())?;

    let store = Store::open(&args.db)
        .with_context(|| format!("failed to open metadata index {:?}", args.db))?;
    let db = args.db.display().to_string();

    let stats = store.get_stats()?;
    let freshness = tag_freshness(
        &store,
        &stats,
        &TagFreshness {
            db: &args.db,
            no_fresh: args.no_fresh,
            delta_limit: args.delta_limit,
        },
        &mut report,
    );
    if !report.is_json() {
        print_tag_freshness(&freshness);
    }

    if let Some(file) = &args.file {
        return explain_file(
            &store,
            file,
            args.config.as_deref(),
            &db,
            &freshness,
            &mut report,
        );
    }

    // `namespace:` (empty value) means "list this namespace".
    let namespace_only: Option<&str> = match args.query.as_slice() {
        [one] if one.ends_with(':') => Some(one.trim_end_matches(':')),
        _ => None,
    };

    if args.query.is_empty() {
        return list_namespaces(&store, args.limit, &db, &freshness, &mut report);
    }
    if let Some(ns) = namespace_only {
        return list_tags(&store, Some(ns), args.limit, &db, &freshness, &mut report);
    }

    // Otherwise: files carrying all of the given tags.
    let tags: Vec<Tag> = args
        .query
        .iter()
        .map(|t| Tag::parse(t))
        .collect::<Result<Vec<_>>>()?;

    let page = tagindex::files_with_tags(&store, &tags, args.limit as i64)?;
    let total = tagindex::count_files_with_tags(&store, &tags)?;
    let fetched = page.len() as i64;

    // Deletion is invisible to every delta source — a walk only reports what is
    // there — so the freshness design catches it on the other side, by checking
    // the hits themselves (design.md §5). `sagasu find` does; without this,
    // `sagasu tags` listed paths that `find` had already dropped at the same
    // instant. `--no-fresh` skips it, and says so rather than looking checked.
    let (rows, gone) = if args.no_fresh {
        (page, Vec::new())
    } else {
        tagindex::partition_existing(page)
    };

    if report.is_json() {
        json::tags_files(&db, &tags, &freshness, &rows, &gone, total, fetched);
    } else {
        let labels: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
        println!("tags    : {}", labels.join(" AND "));
        // `N of M`, matching `sagasu search`. Printing the page length alone
        // would present a `--limit` truncation as the answer, which is the same
        // silent omission the freshness design exists to prevent — just at the
        // other end.
        println!("hits    : {} of {} files", rows.len(), total);
        for row in &rows {
            println!("{:>8}  {}", row.file_id, row.path);
        }

        if !gone.is_empty() {
            // Named, not just counted: "two rows were stale" and "these two
            // paths no longer exist" are different amounts of help when the next
            // step is deciding whether to re-index.
            println!(
                "dropped : {} of the {fetched} row(s) on this page no longer exist on disk",
                gone.len()
            );
            for row in &gone {
                println!(
                    "{:>8}  {}  (deleted since the index was built)",
                    row.file_id, row.path
                );
            }
        }

        if rows.is_empty() && gone.is_empty() {
            println!("          (no live file carries all of these tags)");
        }
        if total > fetched {
            println!(
                "          ({} more — raise --limit/-n to see them)",
                total - fetched
            );
        }
        // The total comes from the index and was never existence-checked, so it
        // can only ever be an upper bound. Say which number was verified and
        // which was not, rather than letting `of {total}` read as a fact.
        //
        // Only under `--no-fresh` would this be a lie in the other direction:
        // there *are* no rows that were checked against the filesystem, so
        // claiming the listed ones were is worse than saying nothing. That
        // mode's stderr warning already states that nothing was checked.
        if !gone.is_empty() {
            println!(
                "          (the {total} total is the indexed count — an upper bound; \
                 only the rows above were checked against the filesystem)"
            );
        }
    }

    if args.no_fresh {
        report.warn(
            "--no-fresh: the listed paths were not checked against the \
             filesystem, so files deleted since the index was built are listed as \
             if they still existed.",
        );
    } else if !gone.is_empty() {
        report.warn(format!(
            "index is stale: {} of the files carrying these tags have been \
             deleted since it was built — re-run `sagasu index <root>` and \
             `sagasu tag`.",
            gone.len()
        ));
    }

    if report.is_json() {
        json::warnings(&report);
    }
    Ok(())
}

/// The namespace overview: the top level of the facet tree.
fn list_namespaces(
    store: &Store,
    limit: usize,
    db: &str,
    freshness: &TagFreshnessReport,
    report: &mut Report,
) -> Result<()> {
    let namespaces = tagindex::namespace_counts(store)?;
    if namespaces.is_empty() {
        report.warn("no tags in this index. Run `sagasu tag` first.");
        if report.is_json() {
            json::tags_counts(db, "namespaces", None, freshness, &[], &[], 0);
            json::warnings(report);
        }
        process::exit(1);
    }

    let counts = tagindex::tag_counts(store, None, limit as i64)?;
    let total = tagindex::tag_counts_total(store, None)?;

    if report.is_json() {
        json::tags_counts(
            db,
            "namespaces",
            None,
            freshness,
            &namespaces,
            &counts,
            total,
        );
        json::warnings(report);
        return Ok(());
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
    print_tag_counts(&counts, total);
    Ok(())
}

/// Print one facet list, saying how much of it is not on screen.
fn print_tag_counts(counts: &[tagindex::TagCount], total: i64) {
    for tc in counts {
        println!("  {:>7}  {}", tc.files, tc.tag);
    }
    // A facet list cut at `--limit` with nothing said about it reads as the
    // whole axis, and "the tag I wanted is not in this namespace" is exactly
    // the wrong conclusion to lead someone to.
    if total > counts.len() as i64 {
        println!(
            "          ({} of {} tags shown — raise --limit/-n for the rest)",
            counts.len(),
            total
        );
    }
}

/// The values inside one namespace.
fn list_tags(
    store: &Store,
    namespace: Option<&str>,
    limit: usize,
    db: &str,
    freshness: &TagFreshnessReport,
    report: &mut Report,
) -> Result<()> {
    let total = tagindex::tag_counts_total(store, namespace)?;
    let counts = if total == 0 {
        Vec::new()
    } else {
        tagindex::tag_counts(store, namespace, limit as i64)?
    };

    if report.is_json() {
        json::tags_counts(db, "values", namespace, freshness, &[], &counts, total);
        json::warnings(report);
        return Ok(());
    }

    match namespace {
        Some(ns) => println!("namespace: {ns}"),
        None => println!("namespace: (all)"),
    }
    if total == 0 {
        println!("  (no tags — is the namespace spelled correctly? `sagasu tags` lists them)");
        return Ok(());
    }
    print_tag_counts(&counts, total);
    Ok(())
}

/// Explain one file: stored tags beside freshly computed ones.
fn explain_file(
    store: &Store,
    file: &Path,
    explicit_config: Option<&Path>,
    db: &str,
    freshness: &TagFreshnessReport,
    report: &mut Report,
) -> Result<()> {
    let loaded = load_config(explicit_config, &[])?;
    let origin = loaded.origin().clone();
    let rules = loaded.into_rules();

    // Canonicalize when we can: `files.path` holds canonical paths, so a
    // relative argument would otherwise never match a row.
    let path = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let path_str = path.to_string_lossy().into_owned();
    let root = store.meta_get("root_path")?;

    let row = tagindex::file_by_path(store, &path_str)?;
    let stored = match &row {
        Some(row) => tagindex::tags_of_file(store, row.file_id)?,
        None => Vec::new(),
    };

    // Compute from the engine. The stored magic bytes are preferred; failing
    // that the head of the file is read, so the explanation matches what a
    // `--read-magic` build would produce rather than a weaker extension guess.
    let computed = tagindex::explain(&path, root.as_deref(), &rules, true)?;

    if report.is_json() {
        json::tags_explain(
            db,
            &path_str,
            &origin,
            freshness,
            row.as_ref().map(|r| r.file_id),
            &stored,
            &computed.tags,
            &computed.dropped,
            computed.capped,
        );
        json::warnings(report);
        return Ok(());
    }

    println!("file    : {path_str}");
    println!("config  : {}", origin.describe());

    match &row {
        Some(row) => println!("index   : file_id {} (live)", row.file_id),
        None => println!("index   : not in the metadata index"),
    }

    println!("stored  :");
    match &row {
        Some(_) => {
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
