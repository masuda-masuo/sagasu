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

use sagasu_core::delta;
use sagasu_core::store::Store;
use sagasu_core::tagindex::{self, TagConfig};
use sagasu_core::tagrules::{RuleSet, DEFAULT_RULES_FILE};
use sagasu_core::tags::{self, Tag, TagSource};

use crate::output::{print_tag_freshness, TagFreshness};

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

    /// Skip the delta probe that reports how much the filesystem has moved
    /// since the tag layer was built.
    #[arg(long)]
    no_fresh: bool,

    /// Give up the delta probe above this many changed files.
    #[arg(long, default_value_t = delta::DEFAULT_DELTA_LIMIT)]
    delta_limit: usize,
}

/// Run `sagasu tags`.
pub fn cmd_tags(args: TagsArgs) -> Result<()> {
    let store = Store::open(&args.db)
        .with_context(|| format!("failed to open metadata index {:?}", args.db))?;

    let stats = store.get_stats()?;
    print_tag_freshness(
        &store,
        &stats,
        &TagFreshness {
            db: &args.db,
            no_fresh: args.no_fresh,
            delta_limit: args.delta_limit,
        },
    );

    if let Some(file) = &args.file {
        return explain_file(&store, file, args.rules.clone());
    }

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

    let labels: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
    println!("tags    : {}", labels.join(" AND "));
    // `N of M`, matching `sagasu search`. Printing the page length alone would
    // present a `--limit` truncation as the answer, which is the same silent
    // omission the freshness design exists to prevent — just at the other end.
    println!("hits    : {} of {} files", rows.len(), total);
    for row in &rows {
        println!("{:>8}  {}", row.file_id, row.path);
    }

    if !gone.is_empty() {
        // Named, not just counted: "two rows were stale" and "these two paths no
        // longer exist" are different amounts of help when the next step is
        // deciding whether to re-index.
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
    // The total comes from the index and was never existence-checked, so it can
    // only ever be an upper bound. Say which number was verified and which was
    // not, rather than letting `of {total}` read as a fact.
    //
    // Only under `--no-fresh` would this be a lie in the other direction: there
    // *are* no rows that were checked against the filesystem, so claiming the
    // listed ones were is worse than saying nothing. That mode's stderr warning
    // already states that nothing was checked.
    if !gone.is_empty() {
        println!(
            "          (the {total} total is the indexed count — an upper bound; \
             only the rows above were checked against the filesystem)"
        );
    }

    if args.no_fresh {
        eprintln!(
            "WARNING: --no-fresh: the listed paths were not checked against the \
             filesystem, so files deleted since the index was built are listed as \
             if they still existed."
        );
    } else if !gone.is_empty() {
        eprintln!(
            "WARNING: index is stale: {} of the files carrying these tags have been \
             deleted since it was built — re-run `sagasu index <root>` and \
             `sagasu tag`.",
            gone.len()
        );
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
    print_tag_counts(store, None, limit)
}

/// Print one facet list, saying how much of it is not on screen.
fn print_tag_counts(store: &Store, namespace: Option<&str>, limit: usize) -> Result<()> {
    let counts = tagindex::tag_counts(store, namespace, limit as i64)?;
    let total = tagindex::tag_counts_total(store, namespace)?;
    for tc in &counts {
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
    Ok(())
}

/// Print the values inside one namespace.
fn list_tags(store: &Store, namespace: Option<&str>, limit: usize) -> Result<()> {
    match namespace {
        Some(ns) => println!("namespace: {ns}"),
        None => println!("namespace: (all)"),
    }
    if tagindex::tag_counts_total(store, namespace)? == 0 {
        println!("  (no tags — is the namespace spelled correctly? `sagasu tags` lists them)");
        return Ok(());
    }
    print_tag_counts(store, namespace, limit)
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
