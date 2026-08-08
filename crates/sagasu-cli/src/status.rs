//! `sagasu status` — what the index currently holds, and what it does not.
//!
//! A read-only report over the metadata database, printed in the order the
//! pipeline builds things: the crawl, then the freshness marker every search
//! replays against, then the full-text index, then the tag layer. The last two
//! each name the scan generation they were built from, because a derived index
//! that is a crawl or two behind answers as if the files added since do not
//! exist, and nothing else in this output would reveal that.
//!
//! `--check-journal` is the one opt-in exception to "reads the DB and nothing
//! else": it opens the USN journal's volume to ask how much of the marker's
//! ring-buffer runway is left (docs/cli.md §9-1). Everything it learns goes
//! through [`sagasu_core::delta::check_journal`], which decides on every
//! platform what there is to report — the CLI never branches on the platform
//! to choose what to print.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use sagasu_core::delta::{self, JournalCheck};
use sagasu_core::walk::{self, ExcludeSet};
use sagasu_core::Store;

use crate::json;
use crate::output::{mib, Output, Report};
use crate::Outcome;

#[derive(Parser)]
pub struct StatusArgs {
    /// Path to the SQLite database file.
    #[arg(long, default_value = "index.db")]
    db: PathBuf,
    /// Probe the live USN journal and report the marker's remaining runway.
    /// Off by default: `status` stays a read-only report unless asked
    /// (docs/cli.md §9-1). Accepted on every platform; where there is no USN
    /// journal to ask, the report says so instead of failing.
    #[arg(long)]
    check_journal: bool,
    /// Warn when the USN marker's remaining lifetime is below this many hours.
    /// Only meaningful together with `--check-journal`.
    #[arg(long, default_value_t = 24)]
    journal_warn_hours: u64,
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

pub fn cmd_status(args: StatusArgs, mode: Output) -> Result<Outcome> {
    let mut report = Report::new(mode);
    let store = Store::open(&args.db)?;
    let stats = store.get_stats()?;
    // One wall clock for the whole report: the scan-marker age and the live
    // journal probe must not disagree about what "now" is.
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);

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

    // The live journal probe, decided once: the human report and the JSON
    // object split from this one value (docs/cli.md §4-6), so neither can
    // drift from the other. Without `--check-journal` the probe is not run and
    // the report prints exactly what it always printed; the JSON says so.
    let journal = if args.check_journal {
        delta::check_journal(stats.delta_marker.as_ref(), now_ns)
    } else {
        JournalCheck::not_checked(delta::JOURNAL_CHECK_NOT_REQUESTED)
    };

    if !report.is_json() {
        print_status(
            &stats,
            policy_state,
            excludes.as_ref(),
            policy_detail.as_deref(),
            unreadable,
            now_ns,
            &journal,
        );
    }

    warn_status(
        &stats,
        policy_state,
        &mut report,
        &journal,
        args.journal_warn_hours,
    );

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
            &journal,
        );
    }

    // `status` is a report: it always has an answer (the report itself), so
    // only 0 and 2 (errors) exist for it.
    Ok(Outcome::Success)
}

/// The human rendering of the index report.
fn print_status(
    stats: &sagasu_core::store::IndexStats,
    policy_state: PolicyState,
    excludes: Option<&ExcludeSet>,
    policy_detail: Option<&str>,
    unreadable: u64,
    now_ns: i64,
    journal: &JournalCheck,
) {
    println!(
        "root path      : {}",
        stats.root_path.as_deref().unwrap_or("(none)")
    );
    println!("schema version : {}", stats.schema_version);

    if let Some(marker_ns) = stats.scan_marker_ns {
        let age_secs = ((now_ns - marker_ns) as f64) / 1e9;
        println!("scan marker    : {:.1}s ago", age_secs);
    } else {
        println!("scan marker    : (never scanned)");
    }

    // The freshness marker is what every search replays against. Which kind it
    // is decides how the delta is read back — and, for USN, whether it is still
    // valid at all (issue #16).
    match &stats.delta_marker {
        Some(sagasu_core::ScanMarker::Usn {
            volume,
            maximum_size,
            next_usn,
            ..
        }) => {
            println!("delta marker   : usn on {volume}");
            // The stored inputs, printed even when `--check-journal` adds the
            // live ones below: the report stays readable without the probe.
            println!("  journal size : {:.1} MiB", mib(*maximum_size));
            println!("  marker usn   : {next_usn}");
        }
        Some(sagasu_core::ScanMarker::Mtime { .. }) => {
            println!("delta marker   : mtime (wall clock; never expires)");
        }
        None => println!("delta marker   : (none — searches cannot merge changes)"),
    }

    // Everything `--check-journal` adds to this report, from the one value
    // `cmd_status` decided (docs/cli.md §9-1). The not-requested case prints
    // nothing at all: that is today's output, unchanged.
    match journal {
        JournalCheck::Checked {
            lifetime,
            journal_matches,
            rolled_off,
            ..
        } => {
            // 8 MiB in 15.4 h reads as 0.8 MiB/h: bytes/s → MiB/h.
            println!(
                "  consumed     : {:.1} MiB since the marker ({:.1} MiB/h over {:.1}h)",
                mib(lifetime.consumed),
                lifetime.rate_bytes_per_sec * 3600.0 / (1024.0 * 1024.0),
                lifetime.elapsed_secs / 3600.0,
            );
            let lifetime_line = if *rolled_off {
                "expired".to_string()
            } else if !*journal_matches {
                "(journal was recreated — the stored USN numbers are meaningless)"
                    .to_string()
            } else if lifetime.expired {
                // The estimate says the recorded capacity is used up, but the
                // journal still holds the marker's records. NTFS treats
                // MaximumSize as a target and trims lazily, so this is a real
                // state, not a contradiction — and saying "expired" here would
                // demand a reindex the delta read does not need.
                "(past the capacity recorded at index time, but the marker's records are still in the journal)"
                    .to_string()
            } else {
                match lifetime.remaining_secs {
                    Some(secs) => format!("~{:.1}h remaining", secs / 3600.0),
                    None => "(not enough data to estimate yet)".to_string(),
                }
            };
            println!("  lifetime     : {lifetime_line}");
        }
        JournalCheck::NotChecked { reason }
            if reason != delta::JOURNAL_CHECK_NOT_REQUESTED =>
        {
            println!("  journal check : not checked — {reason}");
        }
        JournalCheck::NotChecked { .. } => {}
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
    journal: &JournalCheck,
    journal_warn_hours: u64,
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

    // The USN marker's runway, when the probe ran (docs/cli.md §9-1). At
    // most one of these fires: a dead marker's remaining-time warning would
    // add nothing to the rescan demand next to it.
    if let JournalCheck::Checked {
        lifetime,
        journal_matches,
        rolled_off,
        ..
    } = journal
    {
        if !journal_matches {
            report.warn(
                "the USN journal was recreated since the marker (its id no \
                 longer matches), so the marker's USN numbers are meaningless. \
                 The next search cannot compute a delta and will demand a full \
                 rescan — re-run `sagasu index <root>`.",
            );
        } else if *rolled_off {
            report.warn(
                "the USN marker has rolled off the journal, so the next search \
                 cannot compute a delta and will demand a full rescan — re-run \
                 `sagasu index <root>`.",
            );
        } else if let Some(remaining_secs) = lifetime.remaining_secs {
            if remaining_secs < journal_warn_hours as f64 * 3600.0 {
                report.warn(format!(
                    "the USN marker has roughly {:.1} hours of journal headroom \
                     left (below the {journal_warn_hours} hour warning threshold); \
                     once it rolls off, the next search cannot compute a delta \
                     and will demand a full rescan.",
                    remaining_secs / 3600.0,
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sagasu_core::delta::MarkerLifetime;

    /// An index report with nothing else wrong with it, so `warn_status`
    /// raises exactly the journal warnings under test and nothing more.
    fn quiet_stats() -> sagasu_core::store::IndexStats {
        sagasu_core::store::IndexStats {
            root_path: Some("/srv/data".into()),
            schema_version: 2,
            scan_marker_ns: None,
            delta_marker: None,
            scan_generation: 1,
            live_count: 1,
            tombstone_count: 0,
            null_hash_count: 0,
            fulltext_dir: None,
            fulltext_docs: None,
            fulltext_scan_generation: None,
            tag_scan_generation: None,
            tag_files: None,
            tag_rules: None,
            tag_rows: 0,
            distinct_tags: 0,
        }
    }

    fn checked(lifetime: MarkerLifetime, journal_matches: bool, rolled_off: bool) -> JournalCheck {
        JournalCheck::Checked {
            next_usn_now: 0,
            lifetime,
            live_maximum_size: 32 * 1024 * 1024,
            journal_matches,
            rolled_off,
        }
    }

    /// A lifetime with 24 MiB of headroom consumed at 232.7 B/s: whatever the
    /// `remaining_secs` argument says, it is a consistent value.
    fn lifetime(remaining_secs: Option<f64>, expired: bool) -> MarkerLifetime {
        MarkerLifetime {
            maximum_size: 32 * 1024 * 1024,
            consumed: 8 * 1024 * 1024,
            headroom: 24 * 1024 * 1024,
            elapsed_secs: 15.4 * 3600.0,
            rate_bytes_per_sec: 232.7,
            remaining_secs,
            expired,
        }
    }

    fn journal_warnings(journal: &JournalCheck, warn_hours: u64) -> Vec<String> {
        let mut report = Report::new(Output::Human);
        warn_status(&quiet_stats(), PolicyState::Present, &mut report, journal, warn_hours);
        report.warnings().to_vec()
    }

    #[test]
    fn a_rolled_off_marker_warns_that_the_next_search_demands_a_rescan() {
        let warnings = journal_warnings(&checked(lifetime(Some(0.0), true), true, true), 24);
        assert_eq!(warnings.len(), 1, "one warning, not a stack of them");
        assert!(warnings[0].contains("rolled off"), "{}", warnings[0]);
        assert!(
            warnings[0].contains("sagasu index <root>"),
            "the fix must be named: {}",
            warnings[0]
        );
    }

    #[test]
    fn a_recreated_journal_warns_instead_of_the_remaining_time() {
        // The id mismatch makes every number meaningless: the rescan warning
        // fires, and the remaining-time warning must not (at most one fires).
        let warnings = journal_warnings(
            &checked(lifetime(Some(10.0 * 3600.0), false), false, false),
            24,
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("recreated"), "{}", warnings[0]);
        assert!(
            warnings[0].contains("cannot compute a delta"),
            "{}",
            warnings[0]
        );
    }

    #[test]
    fn low_headroom_warns_with_the_approximate_time_left() {
        let warnings = journal_warnings(
            &checked(lifetime(Some(10.5 * 3600.0), false), true, false),
            24,
        );
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("10.5 hours"),
            "the approximate time must be in the warning: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains("24 hour"),
            "the threshold that fired must be named: {}",
            warnings[0]
        );
    }

    #[test]
    fn a_healthy_marker_does_not_warn() {
        let warnings = journal_warnings(
            &checked(lifetime(Some(48.0 * 3600.0), false), true, false),
            24,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn an_unobservable_rate_never_warns() {
        // `remaining_secs` is None: no invented number, and nothing to warn.
        let warnings = journal_warnings(&checked(lifetime(None, false), true, false), 24);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_not_checked_journal_never_warns() {
        // Neither the not-requested case nor a probe that failed is a warning:
        // the report just carries `checked: false` with its reason.
        let warnings = journal_warnings(
            &JournalCheck::not_checked(delta::JOURNAL_CHECK_NOT_REQUESTED),
            24,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let warnings = journal_warnings(
            &JournalCheck::not_checked("USN journal checks are only available on Windows"),
            24,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}
