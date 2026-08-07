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

use crate::json;
use crate::output::{print_fresh, warn_fresh, Output, Report};
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

    /// Config file for the live scan (default: ./sagasu.toml when present).
    /// Same reasoning as `--ext`: its `[text]` section must match the build.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Removed in issue #6: the two config files were merged into `sagasu.toml`
    /// and this flag became `--config`.
    #[arg(long = "text-config", hide = true)]
    text_config: Option<PathBuf>,
}

pub fn cmd_search(args: SearchArgs, mode: Output) -> Result<()> {
    let mut report = Report::new(mode);
    crate::index::reject_removed_config_flag("--text-config", args.text_config.as_deref())?;

    // The delta merge needs the metadata index (it holds the crawl root and the
    // freshness marker). Without it we can still search the bare tantivy
    // directory — but then the answer is only as fresh as the index, which the
    // stale notice will say out loud.
    let have_db = !args.no_db && args.db.exists();

    if !have_db {
        return search_index_only(&args, &mut report);
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
        text_policy: crate::index::load_config(args.config.as_deref(), &args.ext)?
            .into_text_policy(),
    };

    // One query per process: a cache would only ever miss. `DeltaCache` is for
    // the interactive callers (design.md §5).
    let outcome = fresh::search(&config, None)?;

    if report.is_json() {
        json::search(
            &config.query,
            &args.db.display().to_string(),
            &args.index_dir.display().to_string(),
            &outcome,
            !args.no_fresh,
        );
    } else {
        println!("query   : {}", config.query);
        println!(
            "hits    : {} of {} docs ({} live / {} index)",
            outcome.hits.len(),
            outcome.total_docs,
            outcome.live_hits,
            outcome.hits.len() - outcome.live_hits,
        );
        // Which extension rule the live half of this answer was judged by. It
        // decides whether an edited file comes back refreshed or disappears, so
        // it belongs next to the answer rather than in the build log.
        println!("text    : {}", outcome.text_policy.describe());
        print_fresh(&outcome);
    }

    warn_fresh(&outcome, &mut report);

    if let Some(notice) = &outcome.text_policy_notice {
        report.warn(notice.clone());
    }

    if outcome.total_docs == 0 {
        report.warn(
            "the full-text index is empty. Run `sagasu index <root>` then \
             `sagasu fulltext`.",
        );
    }

    if report.is_json() {
        json::warnings(&report);
    }

    if outcome.total_docs == 0 {
        process::exit(1);
    }

    Ok(())
}

/// Search a bare tantivy directory with no metadata index behind it.
fn search_index_only(args: &SearchArgs, report: &mut Report) -> Result<()> {
    let config = SearchConfig {
        index_dir: args.index_dir.clone(),
        db_path: None,
        query: args.query.clone(),
        limit: args.limit,
        snippet_chars: args.snippet_chars,
    };

    let outcome = fulltext::search(&config)?;

    if report.is_json() {
        json::search_index_only(
            &config.query,
            &args.index_dir.display().to_string(),
            &outcome,
        );
    } else {
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
            // A moved file still resolves through its stable file_id; show where
            // it went rather than printing a path that no longer exists.
            if hit.current_path.is_some() {
                println!("          (indexed as {})", hit.indexed_path);
            }
            if !hit.snippet.is_empty() {
                println!("          {}", hit.snippet);
            }
        }
    }

    report.warn(
        "no metadata index — results are as of the last full-text build \
         and were not merged with filesystem changes since.",
    );

    if outcome.total_docs == 0 {
        report.warn(
            "the full-text index is empty. Run `sagasu index <root>` then \
             `sagasu fulltext`.",
        );
    }

    if report.is_json() {
        json::warnings(report);
    }

    if outcome.total_docs == 0 {
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

pub fn cmd_find(args: FindArgs, mode: Output) -> Result<()> {
    let mut report = Report::new(mode);
    let db = args.db.display().to_string();
    let config = FreshConfig {
        db_path: args.db,
        index_dir: None,
        query: args.query,
        limit: args.limit,
        delta_limit: args.delta_limit,
        no_delta: args.no_fresh,
        ..FreshConfig::new("", "")
    };

    // Asked before the query so an empty index is reported as an empty index
    // rather than as "no such file". `sagasu search` has warned about its own
    // empty index since M1; `find` did not, which meant the one command whose
    // whole job is "is this file indexed" answered "no" identically for
    // "not indexed" and "nothing is indexed" (docs/cli.md §7 #3).
    let live_files = sagasu_core::Store::open(&config.db_path)
        .and_then(|store| store.get_stats())
        .map(|stats| stats.live_count)
        .unwrap_or(-1);

    let outcome = fresh::find(&config, None)?;

    if report.is_json() {
        json::find(&config.query, &db, &outcome, !config.no_delta);
    } else {
        println!("query   : {}", config.query);
        println!(
            "hits    : {} ({} live / {} index)",
            outcome.hits.len(),
            outcome.live_hits,
            outcome.hits.len() - outcome.live_hits,
        );
        print_fresh(&outcome);
    }

    warn_fresh(&outcome, &mut report);

    if live_files == 0 {
        report.warn(
            "the metadata index contains zero live files, so every `sagasu find` \
             answers empty for a reason that has nothing to do with the query. \
             Run `sagasu index <root>`.",
        );
    }

    if report.is_json() {
        json::warnings(&report);
    }

    Ok(())
}
