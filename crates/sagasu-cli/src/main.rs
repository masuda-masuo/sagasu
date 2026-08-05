//! sagasu CLI — local file search engine.
//!
//! Subcommands:
//! - `index`:  parallel metadata crawl + SQLite index.
//! - `hash`:   backfill BLAKE3 content hashes for unhashed files.
//! - `status`: print index statistics.

use std::path::PathBuf;
use std::process;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use sagasu_core::{self, CrawlConfig, Store};

#[derive(Parser)]
#[command(name = "sagasu", about = "Local file search engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Crawl a directory tree and build (or update) a metadata index.
    Index(IndexArgs),
    /// Backfill BLAKE3 content hashes for files that don't have one yet.
    Hash(HashArgs),
    /// Print index statistics.
    Status(StatusArgs),
}

// ── index ───────────────────────────────────────────────────────────────────

#[derive(Parser)]
struct IndexArgs {
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

fn cmd_index(args: IndexArgs) -> Result<()> {
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
struct HashArgs {
    /// Path to the SQLite database file.
    #[arg(long, default_value = "index.db")]
    db: PathBuf,

    /// Skip files larger than this (bytes). Default 4 MiB.
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    max_size: u64,
}

fn cmd_hash(args: HashArgs) -> Result<()> {
    let summary = sagasu_core::walk::hash_backfill(&args.db, args.max_size)?;

    println!("hashed             : {}", summary.hashed);
    println!("skipped (too large): {}", summary.skipped_too_large);
    println!("skipped (unreadable): {}", summary.skipped_unreadable);

    Ok(())
}

// ── status ──────────────────────────────────────────────────────────────────

#[derive(Parser)]
struct StatusArgs {
    /// Path to the SQLite database file.
    #[arg(long, default_value = "index.db")]
    db: PathBuf,
}

fn cmd_status(args: StatusArgs) -> Result<()> {
    let store = Store::open(&args.db)?;
    let stats = store.get_stats()?;

    println!("root path      : {}", stats.root_path.as_deref().unwrap_or("(none)"));
    println!("schema version : {}", stats.schema_version);

    if let Some(marker_ns) = stats.scan_marker_ns {
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let age_secs = ((now_ns - marker_ns) as f64) / 1e9;
        println!("scan marker    : {:.1}s ago", age_secs);
    } else {
        println!("scan marker    : (never scanned)");
    }

    println!("scan generation: {}", stats.scan_generation);
    println!("live files     : {}", stats.live_count);
    println!("tombstones     : {}", stats.tombstone_count);
    println!("NULL hashes    : {}", stats.null_hash_count);

    Ok(())
}

// ── main ────────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Index(args) => cmd_index(args),
        Command::Hash(args) => cmd_hash(args),
        Command::Status(args) => cmd_status(args),
    };

    if let Err(e) = result {
        eprintln!("error: {e:#}");
        process::exit(1);
    }
}
