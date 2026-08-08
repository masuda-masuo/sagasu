//! How the CLI renders things — the output mode, the warning channel, and the
//! formatting helpers more than one subcommand needs.
//!
//! ## Two renderings of one answer
//!
//! Since issue #6 every subcommand can render its result twice: for a person
//! (`Output::Human`) and for a program (`Output::Json`, docs/cli.md §4). The
//! rule that keeps them honest is that **they render the same value** — the
//! command computes, then hands the finished thing to one renderer or the
//! other. Nothing is computed inside a `println!`. That is why
//! [`TagFreshnessReport`] exists at all: the freshness block used to be a
//! function that probed the delta source *and* printed as it went, which is a
//! shape that can only ever have one renderer.
//!
//! ## Warnings go to stderr in both modes
//!
//! [`Report::warn`] always writes `WARNING: …` to stderr, exactly as before,
//! and *also* collects the message so the JSON rendering can carry it
//! (docs/cli.md §4-2). Neither channel is a substitute for the other: a person
//! piping stdout to `jq` still gets the warnings on their terminal, and a
//! program reading the stream does not have to parse stderr to find out that
//! the answer may be incomplete.
//!
//! ## What is shared, and why
//!
//! [`mib`] because `fulltext` and `status` report the same sizes;
//! [`print_fresh`] because `search` and `find` answer the same question and must
//! not be allowed to drift into answering it two different ways; and
//! [`tag_freshness`] because `tags` and `browse` read the same snapshot and owe
//! the user the same admission about it. The delta report, the hit list and the
//! stale notice are one thing the user reads as one block; splitting it per
//! command is how a stale notice goes missing from one of them.

use std::path::Path;

use sagasu_core::delta::{self, DeltaStatus};
use sagasu_core::fresh::{FreshOutcome, HitOrigin};
use sagasu_core::store::{IndexStats, Store};

// ── Output mode and the warning channel ─────────────────────────────────────

/// Which rendering the caller asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Output {
    /// The labelled, human-readable report.
    Human,
    /// Machine-readable (docs/cli.md §4).
    Json,
}

impl Output {
    /// Pick the mode from the global `--json` flag.
    pub(crate) fn from_flag(json: bool) -> Self {
        if json {
            Output::Json
        } else {
            Output::Human
        }
    }

    /// Whether the machine-readable rendering was asked for.
    pub(crate) fn is_json(self) -> bool {
        self == Output::Json
    }
}

/// The output mode plus everything the command warned about along the way.
pub(crate) struct Report {
    mode: Output,
    warnings: Vec<String>,
}

impl Report {
    /// A report in the given mode with no warnings yet.
    pub(crate) fn new(mode: Output) -> Self {
        Self {
            mode,
            warnings: Vec::new(),
        }
    }

    /// Whether the machine-readable rendering was asked for.
    pub(crate) fn is_json(&self) -> bool {
        self.mode.is_json()
    }

    /// Warn, on stderr, now — and remember it for the JSON rendering.
    ///
    /// Written immediately rather than buffered so the human-facing ordering of
    /// stdout and stderr is exactly what it was before `--json` existed.
    pub(crate) fn warn(&mut self, message: impl Into<String>) {
        let message = message.into();
        eprintln!("WARNING: {message}");
        self.warnings.push(message);
    }

    /// Everything warned about so far.
    pub(crate) fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// Bytes → MiB, for reporting.
pub(crate) fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Seconds between a unix-ns timestamp and now.
pub(crate) fn age_secs(ns: i64) -> f64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    ((now - ns).max(0) as f64) / 1e9
}

// ── Merged search / find ────────────────────────────────────────────────────

/// Collect the warnings a merged answer owes the user.
///
/// Separate from [`print_fresh`] because both renderings owe the same ones, and
/// because `search` and `find` must not each decide for themselves what counts
/// as worth admitting.
pub(crate) fn warn_fresh(outcome: &FreshOutcome, report: &mut Report) {
    // The whole design rests on the index being allowed to be stale. That only
    // works if "stale" is never silent.
    if let Some(notice) = &outcome.stale {
        report.warn(notice.message.clone());
    }
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
            "delta   : {} changed via {}{} ({} scanned, {} excluded = {:.0}% noise{})",
            d.entries,
            d.kind.as_str(),
            if d.cached { ", cached" } else { "" },
            d.scanned,
            d.excluded,
            ratio,
            // Records whose parent directory no longer exists (issue #57): the
            // path is gone, so they are dropped without losing a real change —
            // counted apart from exclusions and errors. The Win32 codes ride
            // along because the set that counts as "gone" is documented rather
            // than observed on NTFS, and this is what settles it.
            if d.gone > 0 {
                format!(
                    ", {} dropped (parent dir gone{})",
                    d.gone,
                    if d.frn_error_codes.is_empty() {
                        String::new()
                    } else {
                        format!(
                            ", win32 {}",
                            d.frn_error_codes
                                .iter()
                                .map(|c| c.to_string())
                                .collect::<Vec<_>>()
                                .join("/")
                        )
                    }
                )
            } else {
                String::new()
            },
        );
        if let DeltaStatus::RescanRequired(reason) = d.status {
            println!("          rescan required: {}", reason.as_str());
        }
        // Unreadable entries are not exclusions: a directory the live scan
        // could not open may hold changes this answer does not know about.
        if d.errors > 0 {
            println!(
                "          note: {} entr(ies) were unreadable during the live scan, so \
                 changes below them are not in this answer",
                d.errors
            );
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

/// What the delta probe found, or why it did not run.
///
/// Five outcomes rather than an `Option`: "not asked", "no marker to compare
/// against", "the probe itself failed" and "the probe ran" call for four
/// different next moves, and collapsing them loses exactly the information the
/// user needs.
pub(crate) enum TagDelta {
    /// `--no-fresh`.
    NotProbed,
    /// The index has no freshness marker.
    NoMarker,
    /// The probe could not run or could not finish.
    Failed(String),
    /// The probe ran.
    Probed {
        entries: usize,
        source: &'static str,
        scanned: u64,
        excluded: u64,
        status: DeltaStatus,
    },
}

/// The tag layer's own account of itself.
pub(crate) struct TagFreshnessReport {
    /// Whether `sagasu tag` has ever run against this index.
    pub built: bool,
    pub rows: i64,
    pub files: i64,
    pub distinct: i64,
    /// Scan generation the layer was built from.
    pub generation: Option<i64>,
    pub scan_generation: i64,
    /// Crawls that have happened since. `0` = level with the metadata index —
    /// which is *not* the same as level with the filesystem.
    pub behind: i64,
    pub rules: Option<String>,
    pub delta: TagDelta,
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
pub(crate) fn tag_freshness(
    store: &Store,
    stats: &IndexStats,
    opts: &TagFreshness<'_>,
    report: &mut Report,
) -> TagFreshnessReport {
    let Some(generation) = stats.tag_scan_generation else {
        report.warn("this index has no tag layer; every tag query answers empty.");
        return TagFreshnessReport {
            built: false,
            rows: 0,
            files: 0,
            distinct: 0,
            generation: None,
            scan_generation: stats.scan_generation,
            behind: 0,
            rules: None,
            delta: TagDelta::NotProbed,
        };
    };

    let behind = stats.scan_generation - generation;
    if behind > 0 {
        report.warn(format!(
            "the tag layer is {behind} scan(s) behind the metadata index — \
             a tag filter is missing every file indexed since. Re-run `sagasu tag`."
        ));
    }

    let delta = probe_tag_delta(store, opts, behind, report);

    TagFreshnessReport {
        built: true,
        rows: stats.tag_rows,
        files: stats.tag_files.unwrap_or(0),
        distinct: stats.distinct_tags,
        generation: Some(generation),
        scan_generation: stats.scan_generation,
        behind,
        rules: stats.tag_rules.clone().filter(|r| !r.is_empty()),
        delta,
    }
}

/// Ask the delta source how far the filesystem has moved since the crawl. This
/// is a *probe*, not a merge — see [`tag_freshness`].
fn probe_tag_delta(
    store: &Store,
    opts: &TagFreshness<'_>,
    behind: i64,
    report: &mut Report,
) -> TagDelta {
    if opts.no_fresh {
        report.warn(
            "the tag layer's freshness was not checked, so this answer is \
             of unknown completeness.",
        );
        return TagDelta::NotProbed;
    }

    // The crawl's own exclusion policy, replayed from what it recorded — the
    // same set the index was built with, not "the defaults" (design.md §5-1).
    let (marker, config) = match (
        store.delta_marker(),
        delta::DeltaConfig::from_index(store, opts.db),
    ) {
        (Ok(Some(marker)), Ok(Some(config))) => (marker, config),
        (_, Err(e)) => {
            report.warn(format!(
                "could not replay the crawl's exclusion policy: {e:#}"
            ));
            return TagDelta::Failed(format!("{e:#}"));
        }
        _ => {
            report.warn(
                "index is stale: no freshness marker recorded — \
                 re-run `sagasu index <root>`",
            );
            return TagDelta::NoMarker;
        }
    };
    let set = match delta::source_for(&config).changes_since(&marker, opts.delta_limit) {
        Ok(set) => set,
        Err(e) => {
            report.warn(format!(
                "could not establish how stale the tag layer is: {e:#}"
            ));
            return TagDelta::Failed(format!("{e:#}"));
        }
    };

    match set.status {
        DeltaStatus::Complete => {
            // A changed file is not necessarily missing from the tag layer (an
            // edit does not move a tag), but a created or renamed one is, and
            // the probe cannot tell those apart without a merge. Report the
            // bound rather than guessing.
            if !set.entries.is_empty() && behind == 0 {
                report.warn(format!(
                    "{} file(s) changed since the index was built. Any of them that \
                     are new or renamed carry no tags, so a tag filter cannot return them — \
                     re-run `sagasu index <root>` and `sagasu tag` to include them.",
                    set.entries.len(),
                ));
            }
        }
        DeltaStatus::Truncated { limit } => report.warn(format!(
            "index is stale: more than {limit} files changed since it was \
             built, so the probe was cut short — re-run `sagasu index <root>` and \
             `sagasu tag`."
        )),
        DeltaStatus::RescanRequired(reason) => report.warn(format!(
            "index is stale: {} — re-run `sagasu index <root>` and `sagasu tag`.",
            reason.as_str()
        )),
    }

    TagDelta::Probed {
        entries: set.entries.len(),
        source: set.kind.as_str(),
        scanned: set.scanned,
        excluded: set.excluded,
        status: set.status,
    }
}

/// The human rendering of [`tag_freshness`]'s result.
pub(crate) fn print_tag_freshness(r: &TagFreshnessReport) {
    if !r.built {
        println!("tags    : (never built — run `sagasu tag`)");
        return;
    }

    println!(
        "tags    : {} rows over {} files, {} distinct, built at scan generation {}",
        r.rows,
        r.files,
        r.distinct,
        r.generation.unwrap_or(0),
    );
    if r.behind > 0 {
        println!(
            "          ({} scan(s) behind the index — re-run `sagasu tag`)",
            r.behind
        );
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
    if let Some(rules) = &r.rules {
        println!("rules   : {rules}");
    }

    match &r.delta {
        TagDelta::NotProbed => println!("delta   : (not probed — --no-fresh)"),
        TagDelta::NoMarker => {
            println!("delta   : (no freshness marker — cannot tell what changed)")
        }
        TagDelta::Failed(e) => println!("delta   : (probe failed: {e})"),
        TagDelta::Probed {
            entries,
            source,
            scanned,
            excluded,
            ..
        } => println!(
            "delta   : {entries} changed since the index was built via {source} \
             ({scanned} scanned, {excluded} excluded)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_warning_reaches_both_channels() {
        // The contract `--json` rests on (docs/cli.md §4-2): stderr keeps saying
        // it, *and* the message is kept so the JSON rendering can carry it.
        // Neither channel substitutes for the other, so a `warn` that only
        // printed would leave a machine consumer parsing stderr — and one that
        // only collected would leave a person piping stdout to `jq` with no
        // sign that the answer may be incomplete.
        let mut report = Report::new(Output::Json);
        assert!(report.warnings().is_empty());

        report.warn("index is stale");
        report.warn(format!("{} file(s) changed", 3));

        assert_eq!(
            report.warnings(),
            ["index is stale", "3 file(s) changed"],
            "in the order they were raised"
        );
        // The human rendering collects too: the same command may exit non-zero
        // on a warning, and the decision must not depend on the mode.
        let mut human = Report::new(Output::Human);
        human.warn("zero files indexed");
        assert_eq!(human.warnings().len(), 1);
        assert!(!human.is_json());
    }
}
