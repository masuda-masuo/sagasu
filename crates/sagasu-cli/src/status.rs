//! `sagasu status` — what the index currently holds, and what it does not.
//!
//! A read-only report over the metadata database, printed in the order the
//! pipeline builds things: the crawl, then the freshness marker every search
//! replays against, then the full-text index, then the tag layer. The last two
//! each name the scan generation they were built from, because a derived index
//! that is a crawl or two behind answers as if the files added since do not
//! exist, and nothing else in this output would reveal that.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use sagasu_core::walk::{self, ExcludeSet};
use sagasu_core::Store;

use crate::json;
use crate::output::{mib, Output, Report};

#[derive(Parser)]
pub struct StatusArgs {
    /// Path to the SQLite database file.
    #[arg(long, default_value = "index.db")]
    db: PathBuf,
}

/// What the index says about the exclusion policy it was crawled with.
///
/// The three states are kept apart because they mean three different things at
/// query time, and only one of them is fine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PolicyState {
    /// Recorded and readable: every query replays exactly what the crawl used.
    Present,
    /// No row at all (an older build wrote this index). Queries still merge
    /// changes, filtered with the built-in defaults.
    NotRecorded,
    /// A row that will not parse. Queries skip the delta merge entirely.
    Unreadable,
}

impl PolicyState {
    /// Stable label for the machine rendering.
    fn as_str(self) -> &'static str {
        match self {
            PolicyState::Present => "present",
            PolicyState::NotRecorded => "not_recorded",
            PolicyState::Unreadable => "unreadable",
        }
    }
}

pub fn cmd_status(args: StatusArgs, mode: Output) -> Result<()> {
    let mut report = Report::new(mode);
    let store = Store::open(&args.db)?;
    let stats = store.get_stats()?;

    // The exclusion policy, decoded once. Three states rather than one shape,
    // because they mean three different things at query time and only one of
    // them is fine — see [`PolicyState`] and the warnings at the end.
    let (policy_state, excludes, policy_detail) = match store.meta_get(walk::EXCLUDE_POLICY_KEY)? {
        Some(encoded) => match ExcludeSet::decode(&encoded) {
            Ok(excludes) => (PolicyState::Present, Some(excludes), None),
            Err(e) => (PolicyState::Unreadable, None, Some(format!("{e:#}"))),
        },
        None => (
            PolicyState::NotRecorded,
            None,
            Some("crawled by an older sagasu".to_string()),
        ),
    };
    let unreadable = store
        .meta_get(walk::SCAN_ERRORS_KEY)?
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    if !report.is_json() {
        print_status(
            &stats,
            policy_state,
            excludes.as_ref(),
            policy_detail.as_deref(),
            unreadable,
        );
    }

    warn_status(&stats, policy_state, &mut report);

    if report.is_json() {
        // Written once, at the end: a summary-shaped command is one object, so
        // it cannot be emitted until the warnings that belong in it are known.
        json::status(
            &stats,
            json::exclusion_state(
                policy_state.as_str(),
                excludes.as_ref(),
                policy_detail.as_deref(),
            ),
            unreadable,
            &report,
        );
    }

    Ok(())
}

/// The human rendering of the index report.
fn print_status(
    stats: &sagasu_core::store::IndexStats,
    policy_state: PolicyState,
    excludes: Option<&ExcludeSet>,
    policy_detail: Option<&str>,
    unreadable: u64,
) {
    println!(
        "root path      : {}",
        stats.root_path.as_deref().unwrap_or("(none)")
    );
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

    // The freshness marker is what every search replays against. Which kind it
    // is decides how the delta is read back — and, for USN, whether it is still
    // valid at all (issue #16).
    match &stats.delta_marker {
        Some(marker @ sagasu_core::ScanMarker::Usn { volume, .. }) => {
            println!("delta marker   : usn on {volume}");
            // A live estimate needs the journal's current NextUsn, which means
            // opening the volume; `sagasu status` stays read-only and prints the
            // stored inputs so the estimate is at least reproducible by hand.
            if let sagasu_core::ScanMarker::Usn {
                maximum_size,
                next_usn,
                ..
            } = marker
            {
                println!("  journal size : {:.1} MiB", mib(*maximum_size));
                println!("  marker usn   : {next_usn}");
            }
        }
        Some(sagasu_core::ScanMarker::Mtime { .. }) => {
            println!("delta marker   : mtime (wall clock; never expires)");
        }
        None => println!("delta marker   : (none — searches cannot merge changes)"),
    }

    // The exclusion policy the crawl ran under, replayed from what it stored.
    // Every query replays this too (design.md §5-1), so a surprising file count
    // is explainable from this report alone rather than from shell history.
    // Two different failures with two different consequences at query time —
    // see [`warn_status`].
    match (policy_state, excludes) {
        (PolicyState::Present, Some(excludes)) => {
            println!("exclusion      : {} dir name(s)", excludes.names().len());
            println!("  hidden       : {}", excludes.hidden_policy().as_str());
            println!(
                "  gitignore    : {}",
                if excludes.uses_gitignore() {
                    format!(
                        "{} rule(s) baked in, directories only{}",
                        excludes.gitignore_rules(),
                        match excludes.gitignore_digest() {
                            Some(d) => format!(" (digest {})", &d[..d.len().min(12)]),
                            None => String::new(),
                        }
                    )
                } else {
                    "not applied".to_string()
                }
            );
        }
        (PolicyState::Unreadable, _) => println!(
            "exclusion      : (unreadable policy: {})",
            policy_detail.unwrap_or("")
        ),
        _ => println!("exclusion      : (not recorded — crawled by an older sagasu)"),
    }

    // Entries the last crawl could not read. Persisted because the crawl's own
    // warning scrolls away and this is the report someone comes back to.
    if unreadable > 0 {
        println!("unreadable     : {unreadable} (as of the last crawl)");
    }

    println!("scan generation: {}", stats.scan_generation);
    println!("live files     : {}", stats.live_count);
    println!("tombstones     : {}", stats.tombstone_count);
    println!("NULL hashes    : {}", stats.null_hash_count);

    // Full-text index state. Showing the generation it was built from makes a
    // stale full-text index visible instead of leaving it to guesswork.
    let fulltext_docs = stats.fulltext_docs.unwrap_or(0);
    match &stats.fulltext_dir {
        Some(dir) => {
            println!("full-text index: {dir}");
            println!("  documents    : {fulltext_docs}");
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

    // Tag layer state. Same reasoning as the full-text index above: a tag layer
    // built two crawls ago answers as if the files added since do not exist,
    // and nothing else in the output would reveal that.
    match stats.tag_scan_generation {
        Some(tag_gen) => {
            println!("tags           : {} rows", stats.tag_rows);
            println!("  tagged files : {}", stats.tag_files.unwrap_or(0));
            println!("  distinct     : {}", stats.distinct_tags);
            let behind = stats.scan_generation - tag_gen;
            if behind > 0 {
                println!(
                    "  built at gen : {tag_gen} ({behind} scan(s) behind — re-run `sagasu tag`)"
                );
            } else {
                println!("  built at gen : {tag_gen} (current)");
            }
            match stats.tag_rules.as_deref() {
                Some(rules) if !rules.is_empty() => println!("  rules        : {rules}"),
                _ => println!("  rules        : (none)"),
            }
        }
        None => println!("tags           : (not built)"),
    }
}

// ── The empty index ─────────────────────────────────────────────────────────
//
// A stage that indexed nothing reports a perfectly healthy-looking zero, and at
// query time "indexed but not findable" and "never indexed" are
// indistinguishable. `index` and `fulltext` warn at build time, but the build
// scrolls away and this report is what someone comes back to (issue #15).
// Warnings go to stderr so the report itself stays parseable — and into the
// JSON, so a machine consumer does not have to parse stderr to learn the same
// thing (docs/cli.md §4-2).

/// Everything `sagasu status` owes the user beyond the numbers.
fn warn_status(
    stats: &sagasu_core::store::IndexStats,
    policy_state: PolicyState,
    report: &mut Report,
) {
    if stats.root_path.is_some() {
        // The two branches lead to different query behaviour, so they get
        // different warnings. Sharing one sentence made this report describe
        // something the `unreadable` case does not do.
        match policy_state {
            // No policy row: the delta path *does* run, filtered with the
            // built-in defaults. That is only right for an index crawled with
            // the defaults — an older build accepted `--exclude` and recorded
            // nothing, so such an index disagrees with every answer it gives.
            PolicyState::NotRecorded => report.warn(
                "this index records no exclusion policy, so searches still merge \
                 changes but filter their live scan with the built-in defaults. If it was \
                 crawled with --exclude / --skip-hidden / --use-gitignore, files it never \
                 indexed can come back as live hits with no index row behind them — \
                 re-run `sagasu index <root>`.",
            ),
            // A policy that exists but cannot be parsed: the delta query is not
            // run at all, because filtering it differently from the crawl is
            // the failure the policy exists to prevent. Searches answer from
            // the index alone and say so.
            PolicyState::Unreadable => report.warn(
                "this index's exclusion policy cannot be read back, so searches \
                 skip the delta query entirely and answer from the index alone (marked \
                 stale). Anything created, edited or deleted since the crawl is missing \
                 from every answer — re-run `sagasu index <root>`.",
            ),
            PolicyState::Present => {}
        }
    }

    let fulltext_built = stats.fulltext_dir.is_some();
    let fulltext_docs = stats.fulltext_docs.unwrap_or(0);

    if stats.root_path.is_none() {
        report.warn("this database holds no crawl — run `sagasu index <root>` first.");
    } else if stats.live_count == 0 {
        report.warn(
            "the metadata index contains zero live files. The root may be \
             wrong, or the exclusion policy above may cover all of it — re-run \
             `sagasu index <root>` and read the `skipped` breakdown.",
        );
    } else if fulltext_built && fulltext_docs == 0 {
        report.warn(
            "the full-text index exists but holds zero documents, so every \
             `sagasu search` answers empty. Re-run `sagasu fulltext` and read the \
             `skipped` breakdown — `--ext <EXT>` or a config file may be needed.",
        );
    }
}
