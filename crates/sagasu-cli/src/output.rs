//! How the CLI renders things — the formatting helpers more than one subcommand
//! needs.
//!
//! Three things qualify so far, and they qualify for different reasons:
//! [`mib`] because `fulltext` and `status` report the same sizes;
//! [`print_fresh`] because `search` and `find` answer the same question and must
//! not be allowed to drift into answering it two different ways; and
//! [`print_tag_freshness`] because `tags` and `browse` read the same snapshot
//! and owe the user the same admission about it. The delta report, the hit list
//! and the stale notice are one thing the user reads as one block; splitting it
//! per command is how a stale notice goes missing from one of them.

use std::path::Path;

use sagasu_core::delta::{self, DeltaStatus};
use sagasu_core::fresh::{FreshOutcome, HitOrigin};
use sagasu_core::store::{IndexStats, Store};

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

// ── Tag-layer freshness ─────────────────────────────────────────────────────

/// What a tag-layer reader needs in order to report its own freshness.
pub(crate) struct TagFreshness<'a> {
    /// The database being read — its WAL/SHM siblings are excluded from the
    /// delta walk, exactly as the crawl excludes them.
    pub db: &'a Path,
    /// Skip the delta probe. Says so rather than looking checked.
    pub no_fresh: bool,
    /// Give up the probe above this many changed files.
    pub delta_limit: usize,
}

/// Report what the tag layer is, and — honestly — what it is not.
///
/// ## Why this does not say "(current)"
///
/// It used to. `tag_scan_generation == scan_generation` only means "no crawl has
/// happened since the tags were built"; it says nothing at all about the
/// filesystem, which is free to have moved on. Measured: `sagasu find` returned
/// six hits (three of them live) for a tree where `sagasu tags <tag>` returned
/// three and called itself current. That is the project's worst failure mode —
/// results silently missing — dressed up as a reassurance.
///
/// ## Why the answer is not simply merged, like `find` and `search`
///
/// A tag filter cannot be delta-merged the way a path or full-text query is.
/// Both of those can evaluate the query *against the live file itself*; a tag
/// filter would have to generate tags for every changed file and then decide how
/// a live hit ranks beside an indexed one — and, for `sagasu browse`, how a file
/// with no stored tags is supposed to enter a facet count that is aggregated in
/// SQL. So this reports rather than merges: the layer is named as a snapshot,
/// and the delta source is asked how far the filesystem has moved since — the
/// same question `fresh::find` asks, with the same stale notice on stderr, minus
/// the merge. A user gets a number instead of a false assurance.
///
/// ## Why it lives here rather than in the command
///
/// `tags` and `browse` read the same snapshot. Two copies of this block is how
/// one of them ends up a version behind on what it admits to.
pub(crate) fn print_tag_freshness(store: &Store, stats: &IndexStats, opts: &TagFreshness<'_>) {
    let Some(generation) = stats.tag_scan_generation else {
        println!("tags    : (never built — run `sagasu tag`)");
        eprintln!("WARNING: this index has no tag layer; every tag query answers empty.");
        return;
    };

    let behind = stats.scan_generation - generation;
    println!(
        "tags    : {} rows over {} files, {} distinct, built at scan generation {generation}",
        stats.tag_rows,
        stats.tag_files.unwrap_or(0),
        stats.distinct_tags,
    );
    if behind > 0 {
        println!("          ({behind} scan(s) behind the index — re-run `sagasu tag`)");
    }
    // Stated unconditionally, including when the layer is level with the index:
    // "level with the index" and "level with the filesystem" are different
    // claims, and only the first one is ever knowable from the database alone.
    println!(
        "snapshot: tags describe the corpus as of that scan. Files created or \
         renamed since carry no tags and are not merged in here the way \
         `sagasu find` merges them (issue #5); files deleted since are dropped \
         from a listing by an existence check, and reported."
    );
    if let Some(rules) = &stats.tag_rules {
        if !rules.is_empty() {
            println!("rules   : {rules}");
        }
    }

    if behind > 0 {
        eprintln!(
            "WARNING: the tag layer is {behind} scan(s) behind the metadata index — \
             a tag filter is missing every file indexed since. Re-run `sagasu tag`."
        );
    }
    print_tag_delta(store, opts, behind);
}

/// Ask the delta source how far the filesystem has moved since the crawl, and
/// say so. This is a *probe*, not a merge — see [`print_tag_freshness`].
fn print_tag_delta(store: &Store, opts: &TagFreshness<'_>, behind: i64) {
    if opts.no_fresh {
        println!("delta   : (not probed — --no-fresh)");
        eprintln!(
            "WARNING: the tag layer's freshness was not checked, so this answer is \
             of unknown completeness."
        );
        return;
    }

    // The crawl's own exclusion policy, replayed from what it recorded — the
    // same set the index was built with, not "the defaults" (design.md §5-1).
    let (marker, config) = match (
        store.delta_marker(),
        delta::DeltaConfig::from_index(store, opts.db),
    ) {
        (Ok(Some(marker)), Ok(Some(config))) => (marker, config),
        (_, Err(e)) => {
            println!("delta   : (probe failed: {e:#})");
            eprintln!("WARNING: could not replay the crawl's exclusion policy: {e:#}");
            return;
        }
        _ => {
            println!("delta   : (no freshness marker — cannot tell what changed)");
            eprintln!(
                "WARNING: index is stale: no freshness marker recorded — \
                 re-run `sagasu index <root>`"
            );
            return;
        }
    };
    let set = match delta::source_for(&config).changes_since(&marker, opts.delta_limit) {
        Ok(set) => set,
        Err(e) => {
            println!("delta   : (probe failed: {e:#})");
            eprintln!("WARNING: could not establish how stale the tag layer is: {e:#}");
            return;
        }
    };

    println!(
        "delta   : {} changed since the index was built via {} ({} scanned, {} excluded)",
        set.entries.len(),
        set.kind.as_str(),
        set.scanned,
        set.excluded,
    );

    match set.status {
        DeltaStatus::Complete => {}
        DeltaStatus::Truncated { limit } => {
            eprintln!(
                "WARNING: index is stale: more than {limit} files changed since it was \
                 built, so the probe was cut short — re-run `sagasu index <root>` and \
                 `sagasu tag`."
            );
            return;
        }
        DeltaStatus::RescanRequired(reason) => {
            eprintln!(
                "WARNING: index is stale: {} — re-run `sagasu index <root>` and \
                 `sagasu tag`.",
                reason.as_str()
            );
            return;
        }
    }

    // A changed file is not necessarily missing from the tag layer (an edit does
    // not move a tag), but a created or renamed one is, and the probe cannot
    // tell those apart without a merge. Report the bound rather than guessing.
    if !set.entries.is_empty() && behind == 0 {
        eprintln!(
            "WARNING: {} file(s) changed since the index was built. Any of them that \
             are new or renamed carry no tags, so a tag filter cannot return them — \
             re-run `sagasu index <root>` and `sagasu tag` to include them.",
            set.entries.len(),
        );
    }
}
