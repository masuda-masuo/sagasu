//! sagasu CLI — local file search engine.
//!
//! Subcommands:
//! - `index`:    parallel metadata crawl + SQLite index.
//! - `hash`:     backfill BLAKE3 content hashes for unhashed files.
//! - `fulltext`: extract bodies from the indexed files and build the tantivy
//!   (Lindera) full-text index.
//! - `search`:   keyword search over the full-text index, score-ordered.
//! - `status`:   print index statistics.
//!
//! The pipeline order is `index` → (`hash`) → `fulltext` → `search`:
//! `fulltext` reads the live rows of the metadata index rather than walking the
//! filesystem again, so both stages share one exclusion set and one set of
//! stable file IDs.

use std::path::PathBuf;
use std::process;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use sagasu_core::fulltext::{self, FulltextConfig, SearchConfig};
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
    /// Extract bodies from the indexed files and build the full-text index.
    Fulltext(FulltextArgs),
    /// Search the full-text index (score order, path + snippet).
    Search(SearchArgs),
    /// Print index statistics.
    Status(StatusArgs),
}

/// Default location of the tantivy index, used by both `fulltext` and `search`.
const DEFAULT_INDEX_DIR: &str = "fulltext-index";

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

// ── fulltext ────────────────────────────────────────────────────────────────

#[derive(Parser)]
struct FulltextArgs {
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

fn cmd_fulltext(args: FulltextArgs) -> Result<()> {
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

/// Bytes → MiB, for reporting.
fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

// ── search ──────────────────────────────────────────────────────────────────

#[derive(Parser)]
struct SearchArgs {
    /// Query string (tantivy syntax: `AND` / `OR` / `"phrase"` / `-negation`).
    query: String,

    /// Directory of the tantivy index.
    #[arg(long, default_value = DEFAULT_INDEX_DIR)]
    index_dir: PathBuf,

    /// Metadata database used to resolve hits back to their current path.
    #[arg(long, default_value = "index.db")]
    db: PathBuf,

    /// Do not consult the metadata database (search the index alone).
    #[arg(long)]
    no_db: bool,

    /// Maximum number of hits.
    #[arg(long, short = 'n', default_value_t = 10)]
    limit: usize,

    /// Maximum snippet length in characters.
    #[arg(long, default_value_t = 160)]
    snippet_chars: usize,
}

fn cmd_search(args: SearchArgs) -> Result<()> {
    // Resolving file_id → current path needs the database; skip it silently when
    // the default path does not exist so a bare index directory still works.
    let db_path = if args.no_db || !args.db.exists() {
        None
    } else {
        Some(args.db)
    };

    let config = SearchConfig {
        index_dir: args.index_dir,
        db_path,
        query: args.query,
        limit: args.limit,
        snippet_chars: args.snippet_chars,
    };

    let outcome = fulltext::search(&config)?;

    println!("query   : {}", config.query);
    println!(
        "hits    : {} of {} docs ({:.1}ms match, {:.1}ms total)",
        outcome.hits.len(),
        outcome.total_docs,
        outcome.match_ms,
        outcome.elapsed_ms
    );

    for hit in &outcome.hits {
        let mark = if hit.deleted { " [deleted]" } else { "" };
        println!("{:>8.3}  {}{}", hit.score, hit.display_path(), mark);
        // A moved file still resolves through its stable file_id; show where it
        // went rather than printing a path that no longer exists.
        if hit.current_path.is_some() {
            println!("          (indexed as {})", hit.indexed_path);
        }
        if !hit.snippet.is_empty() {
            println!("          {}", hit.snippet);
        }
    }

    if outcome.total_docs == 0 {
        eprintln!(
            "WARNING: the full-text index is empty. Run `sagasu index <root>` then \
             `sagasu fulltext`."
        );
        process::exit(1);
    }

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

    // Full-text index state. Showing the generation it was built from makes a
    // stale full-text index visible instead of leaving it to guesswork.
    match stats.fulltext_dir {
        Some(dir) => {
            println!("full-text index: {dir}");
            println!("  documents    : {}", stats.fulltext_docs.unwrap_or(0));
            let ft_gen = stats.fulltext_scan_generation.unwrap_or(0);
            let behind = stats.scan_generation - ft_gen;
            if behind > 0 {
                println!(
                    "  built at gen : {ft_gen} ({behind} scan(s) behind — re-run `sagasu fulltext`)"
                );
            } else {
                println!("  built at gen : {ft_gen} (current)");
            }
        }
        None => println!("full-text index: (not built)"),
    }

    Ok(())
}

// ── main ────────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Index(args) => cmd_index(args),
        Command::Hash(args) => cmd_hash(args),
        Command::Fulltext(args) => cmd_fulltext(args),
        Command::Search(args) => cmd_search(args),
        Command::Status(args) => cmd_status(args),
    };

    if let Err(e) = result {
        eprintln!("error: {e:#}");
        process::exit(1);
    }
}
