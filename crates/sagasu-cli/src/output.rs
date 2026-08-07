//! How the CLI renders things — the formatting helpers more than one subcommand
//! needs.
//!
//! Only two things qualify so far, and they qualify for different reasons:
//! [`mib`] because `fulltext` and `status` report the same sizes, and
//! [`print_fresh`] because `search` and `find` answer the same question and must
//! not be allowed to drift into answering it two different ways. The delta
//! report, the hit list and the stale notice are one thing the user reads as one
//! block; splitting it per command is how a stale notice goes missing from one
//! of them.

use sagasu_core::delta::DeltaStatus;
use sagasu_core::fresh::{FreshOutcome, HitOrigin};

/// Bytes → MiB, for reporting.
pub(crate) fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Print the delta report, the hits, and — if the answer may be incomplete —
/// the stale notice.
pub(crate) fn print_fresh(outcome: &FreshOutcome) {
    if let Some(d) = &outcome.delta {
        let noise = d.excluded + d.entries as u64;
        let ratio = if noise > 0 {
            100.0 * d.excluded as f64 / noise as f64
        } else {
            0.0
        };
        println!(
            "delta   : {} changed via {}{} ({} scanned, {} excluded = {:.0}% noise)",
            d.entries,
            d.kind.as_str(),
            if d.cached { ", cached" } else { "" },
            d.scanned,
            d.excluded,
            ratio,
        );
        if let DeltaStatus::RescanRequired(reason) = d.status {
            println!("          rescan required: {}", reason.as_str());
        }
        // A source that cannot see renames makes a renamed file vanish from the
        // answer instead of moving in it. Saying so once beats letting the gap
        // read as "no such file".
        if !d.detects_renames {
            println!(
                "          note: the {} source cannot detect renames on this platform; \
                 a file renamed since the last crawl will be missing until you re-index",
                d.kind.as_str()
            );
        }
    }

    let t = &outcome.timing;
    println!(
        "timing  : index {:.1}ms | delta {:.1}ms | live {:.1}ms | merge {:.1}ms \
         (overhead {:.1}ms of {:.1}ms; setup {:.1}ms)",
        t.index_ms,
        t.delta_ms,
        t.live_ms,
        t.merge_ms,
        t.overhead_ms(),
        t.total_ms,
        t.setup_ms,
    );
    println!(
        "merged  : {} index candidates, {} dropped (changed), {} dropped (deleted)",
        outcome.index_candidates, outcome.dropped_changed, outcome.dropped_deleted,
    );

    for hit in &outcome.hits {
        println!(
            "[{:<5}] {:>8.3}  {}",
            hit.origin.as_str(),
            hit.score,
            hit.path
        );
        if hit.origin == HitOrigin::Live {
            if let (Some(size), Some(mtime)) = (hit.size, hit.mtime_ns) {
                println!(
                    "          {size} bytes, modified {:.1}s ago",
                    age_secs(mtime)
                );
            }
        }
        if !hit.snippet.is_empty() {
            println!("          {}", hit.snippet);
        }
    }

    // The whole design rests on the index being allowed to be stale. That only
    // works if "stale" is never silent.
    if let Some(notice) = &outcome.stale {
        eprintln!("WARNING: {}", notice.message);
    }
}

/// Seconds between a unix-ns timestamp and now.
fn age_secs(ns: i64) -> f64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    ((now - ns).max(0) as f64) / 1e9
}
