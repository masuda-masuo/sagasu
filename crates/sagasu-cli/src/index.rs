//! The write side of the pipeline: `index` → (`hash`) → `fulltext`.
//!
//! Three subcommands in one module because they are one sequence. Each reads
//! what the previous one wrote — `hash` and `fulltext` both work from the live
//! rows of the metadata index rather than walking the filesystem again — so they
//! share one exclusion set and one set of stable file IDs, and each ends the same
//! way: a summary of what it did, and a non-zero exit when the answer was "no
//! work was performed", which is the failure that is otherwise invisible until
//! query time.

use std::path::PathBuf;
use std::process;

use anyhow::{Context, Result};
use clap::Parser;

use sagasu_core::fulltext::{self, FulltextConfig};
use sagasu_core::CrawlConfig;

use crate::output::mib;
use crate::DEFAULT_INDEX_DIR;

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

    /// Number of walker threads (0 = auto).
    #[arg(long, default_value_t = 0)]
    threads: usize,
}

pub fn cmd_index(args: IndexArgs) -> Result<()> {
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
        eprintln!(
            "WARNING: database {:?} is inside the crawl root {:?}; the database \
             file will be excluded from the index, but placing it outside the \
             crawl tree is recommended.",
            db_canon, root
        );
    }

    let config = CrawlConfig {
        root,
        db_path: args.db,
        exclude: args.exclude,
        no_default_excludes: args.no_default_excludes,
        threads: args.threads,
    };

    let summary = sagasu_core::walk::crawl(config)?;

    // Print summary.
    println!("scanned      : {}", summary.scanned);
    println!("indexed      : {}", summary.indexed);
    println!("  added      : {}", summary.added);
    println!("  changed    : {}", summary.changed);
    println!("  renamed    : {}", summary.renamed);
    println!("  deleted    : {}", summary.deleted);

    if !summary.skipped.is_empty() {
        // Sort by count descending, then by name.
        let mut skips: Vec<_> = summary.skipped.iter().collect();
        skips.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        println!("skipped      :");
        for (name, count) in skips {
            println!("  {name}: {count}");
        }
    }

    println!("elapsed      : {:.3}s", summary.elapsed_secs);

    // Zero files indexed = warning + non-zero exit.
    if summary.indexed == 0 {
        eprintln!(
            "WARNING: zero files indexed. Check that the root directory is \
             correct and not entirely excluded."
        );
        process::exit(1);
    }

    Ok(())
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

pub fn cmd_hash(args: HashArgs) -> Result<()> {
    let summary = sagasu_core::walk::hash_backfill(&args.db, args.max_size)?;

    println!("hashed             : {}", summary.hashed);
    println!("skipped (too large): {}", summary.skipped_too_large);
    println!("skipped (unreadable): {}", summary.skipped_unreadable);

    Ok(())
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

pub fn cmd_fulltext(args: FulltextArgs) -> Result<()> {
    let config = FulltextConfig {
        db_path: args.db,
        index_dir: args.index_dir,
        max_size: args.max_size,
        extra_exts: args.ext,
        no_sniff: args.no_sniff,
        threads: args.threads,
        heap_bytes: (args.heap_mb as usize) * 1024 * 1024,
    };

    let summary = fulltext::build(&config)?;

    println!("candidates   : {}", summary.candidates);
    println!("indexed      : {}", summary.indexed);
    println!("  by ext     : {}", summary.accepted_by_ext);
    println!("  by sniff   : {}", summary.accepted_by_sniff);

    // Nothing is dropped silently: every candidate is either indexed or shows
    // up here with a reason.
    if !summary.skipped.is_empty() {
        println!("skipped      : {}", summary.skipped_total());
        for (reason, count) in &summary.skipped {
            println!("  {}: {count}", reason.as_str());
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

    // An empty index is the failure that is hardest to notice from the outside:
    // "indexed but not findable" and "never indexed" look identical at search
    // time. Say so, and exit non-zero.
    if summary.indexed == 0 {
        eprintln!(
            "WARNING: zero documents indexed. Every candidate was skipped (see the \
             reasons above), or the metadata index is empty — run `sagasu index \
             <root>` first, and consider `--ext <EXT>` if your text files use an \
             extension sagasu does not know."
        );
        process::exit(1);
    }

    Ok(())
}
