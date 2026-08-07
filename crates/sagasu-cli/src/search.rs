//! The read side: `search` (full-text) and `find` (path substring).
//!
//! Two subcommands in one module because they are the same query with two
//! matchers behind it. Both are **fresh by default**: before answering they ask
//! a delta source what changed since the index was built, live-scan that set and
//! merge it over the index result (design.md §5). Both print that merge through
//! the same [`crate::output::print_fresh`], and both say so out loud when
//! `--no-fresh` turned it off — an unmerged answer that looks fresh is the exact
//! failure this design exists to prevent, and it is not a failure either command
//! gets to define for itself.

use std::path::PathBuf;
use std::process;

use anyhow::Result;
use clap::Parser;

use sagasu_core::delta;
use sagasu_core::fresh::{self, FreshConfig};
use sagasu_core::fulltext::{self, SearchConfig};

use crate::output::print_fresh;
use crate::DEFAULT_INDEX_DIR;

// ── search ──────────────────────────────────────────────────────────────────

#[derive(Parser)]
pub struct SearchArgs {
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

    /// Answer from the index alone: skip the search-time delta merge.
    #[arg(long)]
    no_fresh: bool,

    /// Give up the live scan above this many changed files and report the index
    /// as stale.
    #[arg(long, default_value_t = delta::DEFAULT_DELTA_LIMIT)]
    delta_limit: usize,

    /// Additional extension the live scan treats as text (repeatable, no leading
    /// dot). Pass the same set `sagasu fulltext --ext` was built with, or an
    /// edit to such a file drops out of the result instead of being refreshed.
    #[arg(long = "ext")]
    ext: Vec<String>,

    /// Text config file for the live scan (default: ./sagasu-text.toml when
    /// present). Same reasoning as `--ext`: it must match the build.
    #[arg(long = "text-config")]
    text_config: Option<PathBuf>,
}

pub fn cmd_search(args: SearchArgs) -> Result<()> {
    // The delta merge needs the metadata index (it holds the crawl root and the
    // freshness marker). Without it we can still search the bare tantivy
    // directory — but then the answer is only as fresh as the index, which the
    // stale notice will say out loud.
    let have_db = !args.no_db && args.db.exists();

    if !have_db {
        return search_index_only(&args);
    }

    let config = FreshConfig {
        db_path: args.db.clone(),
        index_dir: Some(args.index_dir.clone()),
        query: args.query.clone(),
        limit: args.limit,
        delta_limit: args.delta_limit,
        no_delta: args.no_fresh,
        snippet_chars: args.snippet_chars,
        max_size: fulltext::DEFAULT_MAX_SIZE,
        text_policy: crate::index::load_text_policy(args.text_config.as_deref(), &args.ext)?,
    };

    // One query per process: a cache would only ever miss. `DeltaCache` is for
    // the interactive callers (design.md §5).
    let outcome = fresh::search(&config, None)?;

    println!("query   : {}", config.query);
    println!(
        "hits    : {} of {} docs ({} live / {} index)",
        outcome.hits.len(),
        outcome.total_docs,
        outcome.live_hits,
        outcome.hits.len() - outcome.live_hits,
    );
    print_fresh(&outcome);

    if outcome.total_docs == 0 {
        eprintln!(
            "WARNING: the full-text index is empty. Run `sagasu index <root>` then \
             `sagasu fulltext`."
        );
        process::exit(1);
    }

    Ok(())
}

/// Search a bare tantivy directory with no metadata index behind it.
fn search_index_only(args: &SearchArgs) -> Result<()> {
    let config = SearchConfig {
        index_dir: args.index_dir.clone(),
        db_path: None,
        query: args.query.clone(),
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

    eprintln!(
        "WARNING: no metadata index — results are as of the last full-text build \
         and were not merged with filesystem changes since."
    );

    if outcome.total_docs == 0 {
        eprintln!(
            "WARNING: the full-text index is empty. Run `sagasu index <root>` then \
             `sagasu fulltext`."
        );
        process::exit(1);
    }

    Ok(())
}

// ── find ────────────────────────────────────────────────────────────────────

#[derive(Parser)]
pub struct FindArgs {
    /// Literal, case-insensitive substring of the path.
    query: String,

    /// Path to the SQLite database file.
    #[arg(long, default_value = "index.db")]
    db: PathBuf,

    /// Maximum number of hits.
    #[arg(long, short = 'n', default_value_t = 20)]
    limit: usize,

    /// Answer from the index alone: skip the search-time delta merge.
    #[arg(long)]
    no_fresh: bool,

    /// Give up the live scan above this many changed files and report the index
    /// as stale.
    #[arg(long, default_value_t = delta::DEFAULT_DELTA_LIMIT)]
    delta_limit: usize,
}

pub fn cmd_find(args: FindArgs) -> Result<()> {
    let config = FreshConfig {
        db_path: args.db,
        index_dir: None,
        query: args.query,
        limit: args.limit,
        delta_limit: args.delta_limit,
        no_delta: args.no_fresh,
        ..FreshConfig::new("", "")
    };

    let outcome = fresh::find(&config, None)?;

    println!("query   : {}", config.query);
    println!(
        "hits    : {} ({} live / {} index)",
        outcome.hits.len(),
        outcome.live_hits,
        outcome.hits.len() - outcome.live_hits,
    );
    print_fresh(&outcome);

    Ok(())
}
