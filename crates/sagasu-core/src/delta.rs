//! Search-time delta sources — steps 1 and 2 of the freshness design (design.md §5).
//!
//! ## Why this exists
//!
//! sagasu's central bet is that **the index is allowed to be stale**. What is
//! not allowed to be stale is the *answer*. So every search asks a delta source
//! for "everything that changed since the marker recorded at index time",
//! live-greps that (normally tiny) set, and merges it over the index result —
//! see [`crate::fresh`] for the merge.
//!
//! ## The marker
//!
//! [`ScanMarker`] is the point-in-time token `sagasu index` writes into the
//! `delta_marker` meta key:
//!
//! - [`ScanMarker::Mtime`] — the wall-clock instant the crawl started. Never
//!   expires, works everywhere, costs a full `stat` walk to read back.
//! - [`ScanMarker::Usn`] — an NTFS USN Journal position: volume, **journal id**,
//!   next USN, the journal's `MaximumSize`, and when it was taken. Reading a
//!   range back is a journal read (tens of ms) instead of a walk, but the marker
//!   has a finite lifetime.
//!
//! ## A USN marker expires — that is normal operation, not an error path
//!
//! The USN Journal is a ring buffer. On a working machine it is consumed at
//! roughly 8 MiB per few minutes, so a marker older than the journal window is
//! the *expected* state after a lunch break, not a rare corruption case
//! (issue #16). Two independent things can invalidate it:
//!
//! 1. **The records rolled off**: `marker.next_usn < journal.first_usn`, or the
//!    read fails with `ERROR_JOURNAL_ENTRY_DELETED` (0x8007049D).
//! 2. **The journal was recreated**: the USN number space restarts, so number
//!    comparison alone is not enough — the **journal id** must match too.
//!
//! Both land on [`DeltaStatus::RescanRequired`], which is a *different* branch
//! from [`DeltaStatus::Truncated`]: "the delta set is too big to live-grep" and
//! "we can no longer tell what the delta set is" call for different things (one
//! degrades to a warning, the other demands a re-crawl).
//!
//! [`estimate_lifetime`] turns the values stored in the marker into a remaining
//! lifetime estimate, so a caller can warn *before* the marker expires.
//!
//! ## One exclusion set, not two
//!
//! The delta set goes through the same [`ExcludeSet`] as the crawl. This matters
//! most on the USN path, which returns raw volume-wide change records: without
//! the filter the delta set is dominated by telemetry (`.etl`), PowerShell
//! temporaries and build artefacts the index never contained — measured at a
//! 94% noise ratio on a real machine. [`DeltaSet::excluded`] reports how many
//! records the filter dropped so that ratio stays visible.
//!
//! ## Future sources
//!
//! macOS FSEvents and Linux fanotify are additional [`DeltaSource`] impls; both
//! need a resident watcher, which is out of M2 scope. [`DeltaSourceKind`] names
//! them so the seam is explicit rather than implied.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, UNIX_EPOCH};

use anyhow::Result;
use ignore::{WalkBuilder, WalkState};

use crate::walk::{self, ExcludeSet};

// ── Constants ───────────────────────────────────────────────────────────────

/// Default cap on the size of a delta set.
///
/// Above this the live grep stops being "unnoticeable" and the honest answer is
/// to say the index is stale rather than to spend a second reading files. It is
/// deliberately generous relative to the 0.1%-of-corpus figure the design
/// assumes (design.md §5): 2000 changed files is 0.1% of two million.
pub const DEFAULT_DELTA_LIMIT: usize = 2000;

/// Default lifetime of a cached delta set, in milliseconds.
///
/// Sized for interactive search: a burst of keystrokes shares one delta query,
/// and a file touched in another window shows up within a second. See
/// [`DeltaCache`].
pub const DEFAULT_DELTA_TTL_MS: u64 = 1000;

// ── Marker ──────────────────────────────────────────────────────────────────

/// The point-in-time token recorded by a crawl and replayed by a search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanMarker {
    /// Wall-clock instant (unix ns) at which the crawl started.
    Mtime {
        /// Unix nanoseconds.
        started_ns: i64,
    },
    /// NTFS USN Journal position.
    Usn {
        /// Volume the journal belongs to, e.g. `C:`.
        volume: String,
        /// `UsnJournalID`. A recreated journal gets a new id and restarts its
        /// number space, so this must match before `next_usn` means anything.
        journal_id: u64,
        /// `NextUsn` at marker time: the first record a delta read asks for.
        next_usn: i64,
        /// `MaximumSize` of the journal in bytes — the ring-buffer capacity the
        /// lifetime estimate is measured against.
        maximum_size: u64,
        /// Unix ns at which the marker was taken (the other half of the rate
        /// calculation in [`estimate_lifetime`]).
        recorded_ns: i64,
    },
}

impl ScanMarker {
    /// Wall-clock instant the marker was taken, in unix ns.
    ///
    /// Every marker carries one, which is what lets the mtime source stand in
    /// for a USN source that is unavailable at search time (no admin rights, a
    /// non-NTFS volume) instead of giving up.
    pub fn wall_clock_ns(&self) -> i64 {
        match self {
            ScanMarker::Mtime { started_ns } => *started_ns,
            ScanMarker::Usn { recorded_ns, .. } => *recorded_ns,
        }
    }

    /// Short label for reporting (`mtime` / `usn`).
    pub fn kind(&self) -> &'static str {
        match self {
            ScanMarker::Mtime { .. } => "mtime",
            ScanMarker::Usn { .. } => "usn",
        }
    }

    /// Encode for the `meta` table (TEXT). Pipe-separated because `|` cannot
    /// appear in a Windows volume specifier while `:` obviously can.
    pub fn encode(&self) -> String {
        match self {
            ScanMarker::Mtime { started_ns } => format!("mtime|{started_ns}"),
            ScanMarker::Usn {
                volume,
                journal_id,
                next_usn,
                maximum_size,
                recorded_ns,
            } => format!("usn|{volume}|{journal_id}|{next_usn}|{maximum_size}|{recorded_ns}"),
        }
    }

    /// Parse a marker written by [`ScanMarker::encode`]. Returns `None` for an
    /// unknown or malformed encoding — the caller then has no usable marker and
    /// must treat the whole index as stale.
    pub fn decode(s: &str) -> Option<Self> {
        let mut parts = s.split('|');
        match parts.next()? {
            "mtime" => Some(ScanMarker::Mtime {
                started_ns: parts.next()?.parse().ok()?,
            }),
            "usn" => Some(ScanMarker::Usn {
                volume: parts.next()?.to_string(),
                journal_id: parts.next()?.parse().ok()?,
                next_usn: parts.next()?.parse().ok()?,
                maximum_size: parts.next()?.parse().ok()?,
                recorded_ns: parts.next()?.parse().ok()?,
            }),
            _ => None,
        }
    }
}

// ── Marker lifetime (issue #16, requirement 3) ──────────────────────────────

/// How much of a USN marker's runway is left.
///
/// The USN of a record is its byte offset in the journal, so "USN numbers
/// consumed since the marker" and "journal bytes written since the marker" are
/// the same quantity. That makes the estimate a straight division against
/// `MaximumSize` — no extra bookkeeping, using only what the marker already
/// stores.
#[derive(Debug, Clone)]
pub struct MarkerLifetime {
    /// `MaximumSize` recorded with the marker (ring-buffer capacity, bytes).
    pub maximum_size: u64,
    /// Journal bytes written since the marker was taken.
    pub consumed: u64,
    /// Capacity left before the marker's records roll off.
    pub headroom: u64,
    /// Seconds since the marker was taken.
    pub elapsed_secs: f64,
    /// Observed journal write rate, bytes/second.
    pub rate_bytes_per_sec: f64,
    /// Estimated seconds until the marker expires. `None` when the rate cannot
    /// be observed yet (no elapsed time, or nothing written since the marker).
    pub remaining_secs: Option<f64>,
    /// True when the marker's records have already rolled off.
    pub expired: bool,
}

/// Estimate how long a marker still has, given the journal's current `NextUsn`
/// and the current wall clock.
///
/// Returns `None` for an mtime marker: a wall-clock instant does not expire.
///
/// The two inputs come from a live `FSCTL_QUERY_USN_JOURNAL`; keeping them as
/// parameters (rather than querying inside) makes the arithmetic testable on any
/// platform, which is the only part of the USN path that can be.
pub fn estimate_lifetime(
    marker: &ScanMarker,
    next_usn_now: i64,
    now_ns: i64,
) -> Option<MarkerLifetime> {
    let ScanMarker::Usn {
        next_usn,
        maximum_size,
        recorded_ns,
        ..
    } = marker
    else {
        return None;
    };

    let consumed = next_usn_now.saturating_sub(*next_usn).max(0) as u64;
    let headroom = maximum_size.saturating_sub(consumed);
    let elapsed_secs = ((now_ns - recorded_ns).max(0) as f64) / 1e9;
    let rate_bytes_per_sec = if elapsed_secs > 0.0 {
        consumed as f64 / elapsed_secs
    } else {
        0.0
    };
    let remaining_secs = (rate_bytes_per_sec > 0.0).then(|| headroom as f64 / rate_bytes_per_sec);

    Some(MarkerLifetime {
        maximum_size: *maximum_size,
        consumed,
        headroom,
        elapsed_secs,
        rate_bytes_per_sec,
        remaining_secs,
        expired: headroom == 0,
    })
}

// ── Live journal probe for `status --check-journal` (issue #60) ─────────────

/// The reason `--check-journal` did not run because it was not asked for.
///
/// The one not-checked case the CLI decides itself (the flag is a CLI concept);
/// every other reason comes from [`check_journal`]. Kept as a named constant so
/// the human rendering can tell "nothing new to print" from "the probe failed".
pub const JOURNAL_CHECK_NOT_REQUESTED: &str = "not requested (--check-journal)";

/// Live journal values from one `FSCTL_QUERY_USN_JOURNAL`, in the shape the
/// lifetime check needs.
///
/// Platform-neutral on purpose: the fetch ([`crate::usn::query_live_journal`])
/// is Windows-only, but the decision on top of it must be testable everywhere.
#[derive(Debug, Clone, Copy)]
pub struct LiveJournal {
    /// `UsnJournalID` — the identity check against the marker's.
    pub journal_id: u64,
    /// `FirstUsn` — the oldest record still in the ring.
    pub first_usn: i64,
    /// `NextUsn` — where the journal writes next; the live half of the
    /// consumed-bytes arithmetic.
    pub next_usn: i64,
    /// `MaximumSize` — the ring capacity the journal reports today.
    pub maximum_size: u64,
}

/// What the optional live journal probe found, or why it did not run.
///
/// One value for both renderings of `sagasu status` (docs/cli.md §4-6): the
/// human report and the JSON object split from this, never recompute it.
#[derive(Debug, Clone)]
pub enum JournalCheck {
    /// The probe did not run. `checked: false` with the reason, whatever the
    /// reason is — a probe that did not happen is never reported as one that
    /// did.
    NotChecked {
        /// Why: not requested, not a USN marker, no marker at all, not
        /// Windows, or a Win32 failure (volume open / journal query).
        reason: String,
    },
    /// The probe ran. The lifetime estimate is delivered alongside the two
    /// verdicts that decide whether it means anything.
    Checked {
        /// The journal's live `NextUsn` (goes out as a string: USN values can
        /// exceed 2^53, docs/cli.md §4-3).
        next_usn_now: i64,
        /// The estimate against the marker's recorded capacity.
        lifetime: MarkerLifetime,
        /// `MaximumSize` the journal reports **now**. Recorded separately from
        /// the marker's copy because the two can differ (the journal can be
        /// resized, and NTFS lets the ring exceed its target before trimming),
        /// and that difference is exactly what makes the estimate in
        /// `lifetime` approximate. Reported so the discrepancy is visible
        /// instead of silently skewing a number.
        live_maximum_size: u64,
        /// Whether the live journal id still matches the marker's. False means
        /// the USN number space restarted and every number is meaningless.
        journal_matches: bool,
        /// Whether the marker's records have already rolled off the ring.
        ///
        /// This is the **authoritative** check and it mirrors the delta read
        /// exactly ([`crate::usn`]): the journal's live `FirstUsn` has passed
        /// the marker. The estimate's own `expired` flag deliberately does not
        /// feed in — it is computed against the `MaximumSize` recorded at index
        /// time, and NTFS treats that as a target rather than a hard cap
        /// (trimming is lazy; a journal observed on real hardware held far more
        /// than its `MaximumSize` without wrapping, issue #37). Letting the
        /// estimate decide would tell a user their index is dead and demand a
        /// full reindex while the next search would in fact merge normally.
        rolled_off: bool,
    },
}

impl JournalCheck {
    /// The not-checked variant with a reason.
    pub fn not_checked(reason: impl Into<String>) -> Self {
        JournalCheck::NotChecked {
            reason: reason.into(),
        }
    }
}

/// Probe the live journal for a marker, if the platform and the marker allow.
///
/// The single entry point `sagasu status --check-journal` calls; it decides
/// everything the report needs to print, so the CLI never branches on the
/// platform. Returns the not-checked variant with a reason when there is no
/// marker, when the marker is an mtime marker, when the platform has no USN
/// journal to ask, and when the Win32 probe itself fails (volume open needs
/// administrator rights; a disabled or non-NTFS journal is reported by the
/// query). The classification itself is [`classify_journal`], kept separate so
/// it is testable with observable inputs on any platform.
pub fn check_journal(marker: Option<&ScanMarker>, now_ns: i64) -> JournalCheck {
    let Some(marker) = marker else {
        return JournalCheck::not_checked("no delta marker in the index");
    };
    match marker {
        ScanMarker::Mtime { .. } => JournalCheck::not_checked(
            "the delta marker is an mtime marker (wall clock; never expires)",
        ),
        ScanMarker::Usn { volume, .. } => match fetch_live_journal(volume) {
            Ok(live) => classify_journal(marker, &live, now_ns),
            Err(e) => JournalCheck::not_checked(format!("{e:#}")),
        },
    }
}

/// Classify a marker against live journal values: still fine, about to expire,
/// or already gone — or why the check cannot apply to this marker.
///
/// Pure, so it runs and means something on every platform: the values a real
/// `FSCTL_QUERY_USN_JOURNAL` would return are passed in, and the verdicts the
/// CLI prints and warns about come out. `None` is impossible to produce here —
/// `estimate_lifetime` only returns `None` for an mtime marker, which this
/// function has already returned as not-checked.
pub fn classify_journal(marker: &ScanMarker, live: &LiveJournal, now_ns: i64) -> JournalCheck {
    let ScanMarker::Usn {
        journal_id,
        next_usn,
        ..
    } = marker
    else {
        return JournalCheck::not_checked(
            "the delta marker is an mtime marker (wall clock; never expires)",
        );
    };
    let lifetime = estimate_lifetime(marker, live.next_usn, now_ns)
        .expect("a USN marker always yields a lifetime estimate");
    JournalCheck::Checked {
        next_usn_now: live.next_usn,
        journal_matches: live.journal_id == *journal_id,
        // Authoritative only: exactly the condition the delta read applies
        // (`marker.next_usn < journal.FirstUsn`). See the field's doc comment
        // for why `lifetime.expired` is deliberately not ORed in here.
        rolled_off: live.first_usn > *next_usn,
        live_maximum_size: live.maximum_size,
        lifetime,
    }
}

/// Fetch the live journal values for `volume`.
///
/// Windows: one volume open + one `FSCTL_QUERY_USN_JOURNAL`, exactly what the
/// delta source's own probe does. Everywhere else there is no USN journal to
/// ask, and the probe reports itself as not-checked with that reason.
#[cfg(windows)]
fn fetch_live_journal(volume: &str) -> Result<LiveJournal> {
    crate::usn::query_live_journal(volume)
}

#[cfg(not(windows))]
fn fetch_live_journal(_volume: &str) -> Result<LiveJournal> {
    anyhow::bail!("USN journal checks are only available on Windows")
}

// ── Delta set ───────────────────────────────────────────────────────────────

/// One file the delta source says changed since the marker.
#[derive(Debug, Clone)]
pub struct DeltaEntry {
    /// Absolute path.
    pub path: String,
    /// Size at delta time, or 0 when the file is gone.
    pub size: i64,
    /// Modification time at delta time (unix ns), or 0 when the file is gone.
    pub mtime_ns: i64,
    /// False when the source positively knows the file was deleted (a USN
    /// `FILE_DELETE` record). The mtime source can only see what still exists,
    /// so deletions reach the answer through the merge's existence check
    /// instead — see [`crate::fresh`].
    pub exists: bool,
}

/// Why a delta query could not produce a usable set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescanReason {
    /// `marker.next_usn < journal.first_usn`: the records rolled off the ring.
    MarkerExpired,
    /// The journal was recreated — its id no longer matches the marker, so the
    /// USN numbers refer to a different number space entirely.
    JournalIdMismatch,
    /// The read returned `ERROR_JOURNAL_ENTRY_DELETED` (0x8007049D).
    JournalEntryDeleted,
    /// The journal could not be queried at all (disabled, not NTFS, or the
    /// process lacks the rights to open the volume).
    JournalUnavailable,
    /// No marker was recorded, or the recorded one could not be parsed.
    MarkerMissing,
    /// The marker was taken by a different delta source (or on another volume),
    /// so its position means nothing to this one.
    MarkerKindMismatch,
    /// The crawl's exclusion policy could not be read back, so the delta set
    /// cannot be filtered the way the crawl was. Answering anyway would mix a
    /// differently-scoped live scan into the index result.
    PolicyUnreadable,
}

impl RescanReason {
    /// Stable one-line explanation used in CLI output.
    pub fn as_str(self) -> &'static str {
        match self {
            RescanReason::MarkerExpired => "USN marker rolled off the journal (marker < first USN)",
            RescanReason::JournalIdMismatch => "USN journal was recreated (journal id mismatch)",
            RescanReason::JournalEntryDeleted => {
                "USN journal entries were deleted (ERROR_JOURNAL_ENTRY_DELETED)"
            }
            RescanReason::JournalUnavailable => "USN journal unavailable",
            RescanReason::MarkerMissing => "no usable index marker",
            RescanReason::PolicyUnreadable => "the index's exclusion policy could not be read back",
            RescanReason::MarkerKindMismatch => {
                "index marker was taken by a different delta source"
            }
        }
    }
}

/// What a delta query managed to determine. Three outcomes, not two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaStatus {
    /// The set is complete: everything that changed since the marker is in it.
    Complete,
    /// The set hit the configured cap and collection stopped. The entries that
    /// *are* present are still correct — but changes are missing, so the answer
    /// must be labelled stale.
    Truncated {
        /// The cap that was hit.
        limit: usize,
    },
    /// The delta could not be determined; only a full re-crawl restores
    /// freshness. Independent of [`DeltaStatus::Truncated`] on purpose.
    RescanRequired(RescanReason),
}

impl DeltaStatus {
    /// Whether the answer built on this delta set may be missing changes.
    pub fn is_stale(self) -> bool {
        !matches!(self, DeltaStatus::Complete)
    }
}

/// The result of one delta query.
#[derive(Debug, Clone)]
pub struct DeltaSet {
    /// Files that changed since the marker.
    pub entries: Vec<DeltaEntry>,
    /// Completeness of `entries`.
    pub status: DeltaStatus,
    /// Which source produced it.
    pub kind: DeltaSourceKind,
    /// Whether the producing source can see a rename at all — see
    /// [`DeltaSource::detects_renames`]. When false, a file renamed since the
    /// crawl disappears from the answer rather than moving in it, and callers
    /// must not read "not in the delta set" as "not renamed".
    pub detects_renames: bool,
    /// Files stat'ed (mtime source) or journal records read (USN source).
    pub scanned: u64,
    /// Candidates dropped by the exclusion set. `excluded / (excluded +
    /// entries)` is the noise ratio issue #16 measured at 94%.
    pub excluded: u64,
    /// Entries the scan could not read (an unopenable directory, a file it
    /// could not stat). Not exclusions — nobody asked for these to be dropped
    /// — so they are counted apart, and a delta set with errors in it is
    /// reported as possibly incomplete rather than as complete.
    pub errors: u64,
    /// The first few descriptions behind [`DeltaSet::errors`].
    pub error_samples: Vec<String>,
    /// Wall-clock cost of the query in milliseconds — the number the freshness
    /// claim lives or dies by.
    pub elapsed_ms: f64,
    /// The marker this set was taken against. A cached set whose marker no
    /// longer matches the index has to be thrown away.
    pub marker: ScanMarker,
}

impl DeltaSet {
    /// An empty set carrying a failure status.
    fn failed(kind: DeltaSourceKind, marker: &ScanMarker, reason: RescanReason, ms: f64) -> Self {
        Self {
            entries: Vec::new(),
            status: DeltaStatus::RescanRequired(reason),
            kind,
            // Nothing was determined at all, so claiming rename coverage would
            // be the wrong kind of optimism.
            detects_renames: false,
            scanned: 0,
            excluded: 0,
            errors: 0,
            error_samples: Vec::new(),
            elapsed_ms: ms,
            marker: marker.clone(),
        }
    }

    /// Paths in the set, for O(1) membership tests during the merge.
    pub fn path_set(&self) -> HashSet<&str> {
        self.entries.iter().map(|e| e.path.as_str()).collect()
    }
}

// ── The abstraction ─────────────────────────────────────────────────────────

/// Which mechanism a [`DeltaSource`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaSourceKind {
    /// Full `stat` walk of the root, filtered by mtime. Slow but universal;
    /// this is the fallback every platform always has.
    Mtime,
    /// NTFS USN Journal range read (Windows).
    Usn,
    /// macOS FSEvents. Not implemented — needs a resident watcher (post-M2).
    FsEvents,
    /// Linux fanotify. Not implemented — needs a resident watcher (post-M2).
    Fanotify,
}

impl DeltaSourceKind {
    /// Stable label used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            DeltaSourceKind::Mtime => "mtime",
            DeltaSourceKind::Usn => "usn",
            DeltaSourceKind::FsEvents => "fsevents",
            DeltaSourceKind::Fanotify => "fanotify",
        }
    }
}

/// Configuration shared by every delta source.
#[derive(Debug, Clone)]
pub struct DeltaConfig {
    /// Canonical crawl root. Changes outside it are not our business.
    pub root: PathBuf,
    /// The crawl's exclusion set — the same one, by construction.
    pub excludes: ExcludeSet,
    /// Paths that must never enter a delta set (the database and its WAL/SHM
    /// siblings, which SQLite rewrites on every scan).
    pub skip_paths: Vec<PathBuf>,
    /// Walker threads for the mtime source (0 = auto).
    pub threads: usize,
}

impl DeltaConfig {
    /// Config for `root` with the crawl's default exclusion set and no database
    /// to skip.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            excludes: ExcludeSet::new(&[], false),
            skip_paths: Vec::new(),
            threads: 0,
        }
    }

    /// Config that replays what the crawl recorded in `db_path`'s index.
    ///
    /// This is the only supported way to build a delta config for an existing
    /// index. Reconstructing "the defaults" instead is what let `sagasu index
    /// --exclude` disagree with every later query (design.md §5-2): the crawl
    /// never saw those files, the delta scan did, and a live hit appeared for a
    /// path with no index row behind it.
    ///
    /// An index written before the policy was persisted has no row and falls
    /// back to the built-in defaults. That is **not** a guarantee that it is
    /// what the index was crawled with — an older build accepted `--exclude`
    /// and recorded nothing, so such an index really does disagree with the
    /// answers it now gives, in exactly the way described above. The fallback
    /// exists so an old database still answers at all; `sagasu status` warns
    /// that it is a guess and says to re-crawl.
    ///
    /// # Errors
    ///
    /// Returns an error if the index records no root, or if the stored policy
    /// cannot be parsed (see [`ExcludeSet::decode`]).
    pub fn from_index(store: &crate::Store, db_path: &Path) -> Result<Option<Self>> {
        let Some(root) = store.meta_get("root_path")? else {
            return Ok(None);
        };
        let root = PathBuf::from(root);
        let excludes = match store.meta_get(walk::EXCLUDE_POLICY_KEY)? {
            Some(encoded) => ExcludeSet::decode(&encoded)?,
            None => ExcludeSet::new(&[], false),
        };
        Ok(Some(Self {
            root,
            excludes,
            skip_paths: walk::db_sibling_paths(db_path),
            threads: 0,
        }))
    }

    /// Why `path` is not part of the indexed set, if it is not.
    pub(crate) fn rejection(&self, path: &Path) -> Option<Rejection> {
        if !path_under(&self.root, path) {
            return Some(Rejection::OutOfScope);
        }
        if let Some(reason) = self.excludes.reason_for_path(path, &self.root) {
            return Some(Rejection::Excluded(reason));
        }
        if self.skip_paths.iter().any(|p| walk::same_path(p, path)) {
            return Some(Rejection::OutOfScope);
        }
        None
    }
}

/// Why a candidate path did not make it into a delta set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rejection {
    /// Dropped by the crawl's exclusion policy.
    Excluded(walk::ExcludeReason),
    /// Outside the crawl root, or one of our own database files. A single path,
    /// not a rule about a subtree.
    OutOfScope,
}

impl Rejection {
    /// Whether the whole directory the path sits in can be abandoned, or only
    /// this one entry. A name or gitignore rule is a statement about the
    /// directory; a hidden *file* and a database sibling are not.
    pub(crate) fn prunes_directory(&self) -> bool {
        matches!(
            self,
            Rejection::Excluded(walk::ExcludeReason::Name(_) | walk::ExcludeReason::Gitignore)
        )
    }
}

/// Whether `path` lies inside `root`, on a whole-component boundary.
///
/// [`Path::starts_with`] is case-*sensitive* on every platform, but NTFS is
/// case-insensitive: a path the OS resolved for us (a USN record run back
/// through `GetFinalPathNameByHandleW`) can differ in case from the root the
/// user configured, and rejecting it would silently empty the delta set. On Unix
/// the case fold would be wrong, so it is not applied there.
pub(crate) fn path_under(root: &Path, path: &Path) -> bool {
    if path.starts_with(root) {
        return true;
    }
    cfg!(windows) && starts_with_ignore_ascii_case(root, path)
}

/// Component-boundary prefix test with ASCII case folding.
///
/// Split out from [`path_under`] so the comparison itself is exercised by the
/// unit tests on any host, even though only Windows calls it.
fn starts_with_ignore_ascii_case(root: &Path, path: &Path) -> bool {
    let root = root.to_string_lossy();
    let path = path.to_string_lossy();
    let root = root.trim_end_matches(['\\', '/']).as_bytes();
    let path = path.as_bytes();

    root.len() < path.len()
        && path[..root.len()].eq_ignore_ascii_case(root)
        && matches!(path[root.len()], b'\\' | b'/')
}

/// A source of "what changed since the marker".
///
/// The trait is the seam that keeps the search pipeline from knowing whether it
/// is talking to a USN Journal, a `stat` walk, or (later) a resident watcher.
/// Implementations must be cheap to call repeatedly: an interactive UI asks on
/// every keystroke, mediated by a [`DeltaCache`].
pub trait DeltaSource: Send + Sync {
    /// Which mechanism this is.
    fn kind(&self) -> DeltaSourceKind;

    /// Take a marker for *now* — what a crawl records so a later search can ask
    /// this source for the range since.
    fn current_marker(&self) -> Result<ScanMarker>;

    /// Everything that changed since `marker`, capped at `limit` entries.
    ///
    /// Returning `Ok` with [`DeltaStatus::RescanRequired`] rather than `Err` is
    /// deliberate: an expired marker is normal operation on Windows (issue #16),
    /// and the caller has to *report* it, not abort on it. `Err` is reserved for
    /// failures that say nothing about freshness.
    fn changes_since(&self, marker: &ScanMarker, limit: usize) -> Result<DeltaSet>;

    /// Whether this source can observe a rename.
    ///
    /// Not a detail: when it is false, a file renamed since the crawl leaves the
    /// answer entirely — the index hit is dropped because its old path is gone,
    /// and nothing takes its place. Callers surface it rather than letting the
    /// gap look like "no such file".
    ///
    /// The USN Journal reports renames explicitly. The `stat` walk infers them
    /// from the inode change time, which exists on Unix and has no `std::fs`
    /// equivalent on Windows (NTFS leaves LastWriteTime untouched across a
    /// rename, and ChangeTime is not exposed).
    fn detects_renames(&self) -> bool;
}

/// Environment variable that selects or overrides the delta source.
///
/// - `SAGASU_DELTA_SOURCE=mtime` forces the `stat` walk on Windows.
/// - `SAGASU_DELTA_SOURCE=usn`  selects the USN source (now redundant; the default).
/// - Unset / any other value: the default behaviour (USN probe, silent mtime fallback).
///
/// Non-Windows platforms: always the mtime walk; the env var has no effect.
pub const DELTA_SOURCE_ENV: &str = "SAGASU_DELTA_SOURCE";

/// The delta source to use for this platform and configuration.
///
/// On Windows the USN Journal source is tried first, silently falling back to
/// the `stat` walk when the journal cannot be opened (non-NTFS, journal disabled,
/// or no administrator rights).  The fallback is silent by design — slower, not
/// wrong — and [`DeltaSet::kind`] reports which source actually ran.
///
/// `SAGASU_DELTA_SOURCE=mtime` (case-insensitive) forces the `stat` walk on
/// Windows — the escape hatch now that USN is the default.  `SAGASU_DELTA_SOURCE=usn`
/// still selects USN explicitly (now redundant; kept for backward compatibility).
/// Any other value or unset → the default behaviour (USN probe, silent mtime
/// fallback).
///
/// Non-Windows platforms always return the `stat` walk; the env var has no effect.
///
/// ## History
///
/// The USN source was opt-in (gated behind `SAGASU_DELTA_SOURCE=usn`) because it
/// had never run on real hardware — the development environment is Linux, and CI
/// only compiled it.  The first time the USN source ran on a real NTFS volume it
/// produced an empty delta set, masked in production by a `\\?\` verbatim-path
/// issue that prevented the source from being reached at all.  Both bugs were
/// fixed, and the source was verified on real hardware on 2026-08-08 (issue #37):
/// normal add/change/delete/rename deltas correct, ~17× faster than the stat walk,
/// silent fallback confirmed for non-administrator and journal-absent cases.
/// The opt-in gate was then removed.
///
/// ## Upgrading from a pre-USN-default index
///
/// An index recorded under the old default holds a `stat`-walk marker, which the
/// USN source rejects as a marker-kind mismatch: the first search after the
/// upgrade reports [`DeltaStatus::RescanRequired`] once, warns, and answers from
/// the index as of the last crawl until `sagasu index` is re-run.  One-time,
/// visible, and observed behaving exactly this way on real hardware (issue #37).
pub fn source_for(config: &DeltaConfig) -> Box<dyn DeltaSource> {
    #[cfg(windows)]
    {
        if !forces_mtime(std::env::var(DELTA_SOURCE_ENV).ok().as_deref()) {
            if let Some(src) = crate::usn::UsnDeltaSource::for_config(config) {
                return Box::new(src);
            }
        }
    }
    Box::new(MtimeDeltaSource::new(config.clone()))
}

/// True when the [`DELTA_SOURCE_ENV`] value asks for the `stat` walk.
///
/// Pure so the override decision is unit-testable on every platform;
/// [`source_for`] feeds it the real environment (Windows only — elsewhere the
/// walk is unconditional).
#[cfg_attr(not(windows), allow(dead_code))]
fn forces_mtime(env_value: Option<&str>) -> bool {
    env_value.is_some_and(|v| v.eq_ignore_ascii_case("mtime"))
}

// ── mtime fallback ──────────────────────────────────────────────────────────

/// The universal fallback: walk the root and keep everything newer than the
/// marker.
///
/// It is a full `stat` walk, so its cost scales with the corpus rather than with
/// the number of changes — the price of needing no journal, no watcher and no
/// privileges. On Unix the comparison also looks at the inode change time
/// (`st_ctime`), which is what makes a plain rename visible: a rename leaves
/// mtime untouched but always bumps ctime. Windows has no equivalent through
/// `std::fs` (`created()` is a birth time), which is exactly the gap the USN
/// source fills on that platform.
pub struct MtimeDeltaSource {
    config: DeltaConfig,
}

impl MtimeDeltaSource {
    /// A source that walks `config.root`.
    pub fn new(config: DeltaConfig) -> Self {
        Self { config }
    }
}

impl DeltaSource for MtimeDeltaSource {
    fn kind(&self) -> DeltaSourceKind {
        DeltaSourceKind::Mtime
    }

    /// Unix only, and via [`changed_since`]'s `st_ctime` check rather than
    /// mtime: on Windows a rename leaves every timestamp `std::fs` can read
    /// untouched, so this source cannot see one at all.
    fn detects_renames(&self) -> bool {
        cfg!(unix)
    }

    fn current_marker(&self) -> Result<ScanMarker> {
        Ok(ScanMarker::Mtime {
            started_ns: now_ns(),
        })
    }

    fn changes_since(&self, marker: &ScanMarker, limit: usize) -> Result<DeltaSet> {
        let since_ns = marker.wall_clock_ns();
        let t0 = Instant::now();

        let entries: Mutex<Vec<DeltaEntry>> = Mutex::new(Vec::new());
        let scanned = AtomicU64::new(0);
        let excluded = AtomicU64::new(0);
        let errors = walk::ErrorLog::default();
        let found = AtomicU64::new(0);
        let truncated = AtomicBool::new(false);

        let threads = if self.config.threads == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        } else {
            self.config.threads
        };

        let mut builder = WalkBuilder::new(&self.config.root);
        builder
            .threads(threads)
            .hidden(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .follow_links(false);

        builder.build_parallel().run(|| {
            let (config, entries, errors) = (&self.config, &entries, &errors);
            let (scanned, excluded, found, truncated) = (&scanned, &excluded, &found, &truncated);

            Box::new(move |entry| {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        scanned.fetch_add(1, Ordering::Relaxed);
                        errors.record_walk_error(&e);
                        return WalkState::Continue;
                    }
                };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    return WalkState::Continue;
                }
                if truncated.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }

                scanned.fetch_add(1, Ordering::Relaxed);

                if let Some(rejection) = config.rejection(entry.path()) {
                    excluded.fetch_add(1, Ordering::Relaxed);
                    // Abandoning the whole directory is only safe for a rule
                    // that is about the directory; a database sibling or a
                    // hidden file is a single entry.
                    return match rejection.prunes_directory() {
                        true => WalkState::Skip,
                        false => WalkState::Continue,
                    };
                }

                let meta = match entry.metadata() {
                    Ok(meta) => meta,
                    Err(e) => {
                        errors.record_walk_error(&e);
                        return WalkState::Continue;
                    }
                };
                if !changed_since(&meta, since_ns) {
                    return WalkState::Continue;
                }

                // Claim a slot before pushing so the cap is honoured even with
                // every walker thread racing for the last one.
                if found.fetch_add(1, Ordering::Relaxed) >= limit as u64 {
                    truncated.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }

                entries.lock().unwrap().push(DeltaEntry {
                    path: entry.path().to_string_lossy().into_owned(),
                    size: meta.len() as i64,
                    mtime_ns: mtime_ns(&meta),
                    exists: true,
                });
                WalkState::Continue
            })
        });

        let mut entries = entries.into_inner().unwrap_or_else(|e| e.into_inner());
        // Parallel collection order is nondeterministic; sort so the same tree
        // produces the same delta set (and the same merged result order).
        entries.sort_by(|a, b| a.path.cmp(&b.path));

        let status = if truncated.load(Ordering::Relaxed) {
            DeltaStatus::Truncated { limit }
        } else {
            DeltaStatus::Complete
        };

        let (error_count, error_samples) = errors.drain();

        Ok(DeltaSet {
            entries,
            status,
            kind: DeltaSourceKind::Mtime,
            detects_renames: self.detects_renames(),
            scanned: scanned.load(Ordering::Relaxed),
            excluded: excluded.load(Ordering::Relaxed),
            errors: error_count,
            error_samples,
            elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
            marker: marker.clone(),
        })
    }
}

/// A source that reports "nothing changed" without looking.
///
/// Used when a marker is missing or unusable: the caller still gets a report it
/// can render (with [`DeltaStatus::RescanRequired`]) instead of a special case.
pub struct NullDeltaSource {
    kind: DeltaSourceKind,
    reason: RescanReason,
}

impl NullDeltaSource {
    /// A source that always reports `reason`.
    pub fn new(kind: DeltaSourceKind, reason: RescanReason) -> Self {
        Self { kind, reason }
    }
}

impl DeltaSource for NullDeltaSource {
    fn kind(&self) -> DeltaSourceKind {
        self.kind
    }

    fn detects_renames(&self) -> bool {
        false
    }

    fn current_marker(&self) -> Result<ScanMarker> {
        Ok(ScanMarker::Mtime {
            started_ns: now_ns(),
        })
    }

    fn changes_since(&self, marker: &ScanMarker, _limit: usize) -> Result<DeltaSet> {
        Ok(DeltaSet::failed(self.kind, marker, self.reason, 0.0))
    }
}

// ── Cache ───────────────────────────────────────────────────────────────────

/// Statistics of a [`DeltaCache`], for measurement.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeltaCacheStats {
    /// Queries served from the cached set.
    pub hits: u64,
    /// Queries that had to run the source.
    pub misses: u64,
    /// Of `misses`, how many were caused by the index marker having moved
    /// (i.e. a re-crawl happened).
    pub marker_invalidations: u64,
}

/// A short-lived cache in front of a [`DeltaSource`].
///
/// ## Why this is the M2 design question
///
/// Incremental search fires a query per keystroke. With the mtime fallback each
/// query is a full `stat` walk of the root, so "ask the source every time" turns
/// a 7 ms cost into 7 ms × every character typed, and the freshness mechanism
/// becomes the thing that makes the UI feel slow. Caching it is therefore not an
/// optimisation, it is what makes the design usable interactively.
///
/// ## Invalidation rules
///
/// A cached set is reused only when **all** of these hold:
///
/// 1. **Age < `ttl`.** The whole point of the delta set is freshness, so the
///    cache trades a bounded amount of it for latency. At the default 1 s, a
///    file changed in another window is visible within a second while a burst of
///    typing shares one query.
/// 2. **The marker is unchanged.** A re-crawl moves the marker, which makes
///    every cached delta entry meaningless (it is relative to the old marker).
///    Counted separately in [`DeltaCacheStats`] because it is a correctness
///    invalidation, not an expiry.
///
/// [`DeltaCache::invalidate`] drops the entry explicitly — a caller that just
/// wrote a file should not wait out the TTL to see it.
///
/// Deliberately *not* cached across process runs: a persisted delta set would
/// have to be re-validated against the filesystem anyway, which costs the same
/// as recomputing it.
pub struct DeltaCache {
    ttl_ms: u64,
    entry: Mutex<Option<CachedDelta>>,
    stats: Mutex<DeltaCacheStats>,
}

struct CachedDelta {
    taken_at: Instant,
    set: Arc<DeltaSet>,
}

impl DeltaCache {
    /// A cache with the default TTL ([`DEFAULT_DELTA_TTL_MS`]).
    pub fn new() -> Self {
        Self::with_ttl_ms(DEFAULT_DELTA_TTL_MS)
    }

    /// A cache with an explicit TTL. `0` disables reuse (every query runs the
    /// source), which is what the CLI wants: one query per process.
    pub fn with_ttl_ms(ttl_ms: u64) -> Self {
        Self {
            ttl_ms,
            entry: Mutex::new(None),
            stats: Mutex::new(DeltaCacheStats::default()),
        }
    }

    /// Return the delta set for `marker`, running `source` only when the cached
    /// one is missing, expired, or was taken against a different marker.
    ///
    /// The second element of the returned pair is `true` when the answer came
    /// from the cache.
    pub fn get(
        &self,
        source: &dyn DeltaSource,
        marker: &ScanMarker,
        limit: usize,
    ) -> Result<(Arc<DeltaSet>, bool)> {
        let mut slot = self.entry.lock().unwrap();
        let mut stats = self.stats.lock().unwrap();

        if let Some(cached) = slot.as_ref() {
            let fresh_enough = cached.taken_at.elapsed().as_millis() as u64 <= self.ttl_ms;
            let same_marker = &cached.set.marker == marker;
            if !same_marker {
                stats.marker_invalidations += 1;
            }
            if fresh_enough && same_marker && self.ttl_ms > 0 {
                stats.hits += 1;
                return Ok((cached.set.clone(), true));
            }
        }

        stats.misses += 1;
        drop(stats);

        let set = Arc::new(source.changes_since(marker, limit)?);
        *slot = Some(CachedDelta {
            taken_at: Instant::now(),
            set: set.clone(),
        });
        Ok((set, false))
    }

    /// Drop the cached set. Use after performing a filesystem change yourself.
    pub fn invalidate(&self) {
        *self.entry.lock().unwrap() = None;
    }

    /// Hit / miss counters.
    pub fn stats(&self) -> DeltaCacheStats {
        *self.stats.lock().unwrap()
    }

    /// Configured TTL in milliseconds.
    pub fn ttl_ms(&self) -> u64 {
        self.ttl_ms
    }
}

impl Default for DeltaCache {
    fn default() -> Self {
        Self::new()
    }
}

// ── internal helpers ────────────────────────────────────────────────────────

/// Current wall clock in unix nanoseconds.
pub(crate) fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Modification time in unix ns (0 when unavailable), matching what the crawl
/// stores in `files.mtime_ns`.
fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Whether a file looks touched since `since_ns`.
///
/// On Unix the inode change time is consulted as well as mtime: `rename(2)`
/// bumps ctime but not mtime, so a file moved into the tree after the crawl
/// would otherwise be invisible to the fallback source.
fn changed_since(meta: &std::fs::Metadata, since_ns: i64) -> bool {
    if mtime_ns(meta) > since_ns {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ctime_ns = meta
            .ctime()
            .saturating_mul(1_000_000_000)
            .saturating_add(meta.ctime_nsec());
        if ctime_ns > since_ns {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_round_trips_through_the_meta_encoding() {
        let mtime = ScanMarker::Mtime {
            started_ns: 1_737_000_000_000_000_000,
        };
        assert_eq!(ScanMarker::decode(&mtime.encode()), Some(mtime));

        let usn = ScanMarker::Usn {
            volume: "C:".into(),
            journal_id: 0x01d9_abcd_ef01_2345,
            next_usn: 123_456_789,
            maximum_size: 32 * 1024 * 1024,
            recorded_ns: 1_737_000_000_000_000_000,
        };
        assert_eq!(ScanMarker::decode(&usn.encode()), Some(usn));
    }

    #[test]
    fn marker_decoding_rejects_garbage_instead_of_guessing() {
        assert_eq!(ScanMarker::decode(""), None);
        assert_eq!(ScanMarker::decode("mtime"), None);
        assert_eq!(ScanMarker::decode("mtime|not-a-number"), None);
        assert_eq!(ScanMarker::decode("fsevents|1"), None);
        // A USN marker missing its trailing fields is not half-usable.
        assert_eq!(ScanMarker::decode("usn|C:|1|2"), None);
    }

    #[test]
    fn lifetime_estimate_divides_headroom_by_the_observed_rate() {
        // 8 MiB consumed in 300 s against a 32 MiB journal: 24 MiB of headroom
        // at ~27.9 KiB/s leaves roughly 900 s.
        let marker = ScanMarker::Usn {
            volume: "C:".into(),
            journal_id: 7,
            next_usn: 1_000_000,
            maximum_size: 32 * 1024 * 1024,
            recorded_ns: 0,
        };
        let now = 300i64 * 1_000_000_000;
        let est = estimate_lifetime(&marker, 1_000_000 + 8 * 1024 * 1024, now).unwrap();

        assert_eq!(est.consumed, 8 * 1024 * 1024);
        assert_eq!(est.headroom, 24 * 1024 * 1024);
        assert!(!est.expired);
        let remaining = est.remaining_secs.unwrap();
        assert!((remaining - 900.0).abs() < 1.0, "{remaining}");
    }

    #[test]
    fn lifetime_estimate_reports_an_already_expired_marker() {
        let marker = ScanMarker::Usn {
            volume: "C:".into(),
            journal_id: 7,
            next_usn: 0,
            maximum_size: 8 * 1024 * 1024,
            recorded_ns: 0,
        };
        let est = estimate_lifetime(&marker, 64 * 1024 * 1024, 60 * 1_000_000_000).unwrap();
        assert!(est.expired);
        assert_eq!(est.headroom, 0);
    }

    #[test]
    fn lifetime_estimate_is_meaningless_for_a_wall_clock_marker() {
        let marker = ScanMarker::Mtime { started_ns: 0 };
        assert!(estimate_lifetime(&marker, 1, 1).is_none());
    }

    #[test]
    fn case_folded_containment_respects_component_boundaries() {
        let root = Path::new(r"C:\Users\RUNNER\data");

        assert!(starts_with_ignore_ascii_case(
            root,
            Path::new(r"C:\users\runner\DATA\src\lib.rs")
        ));
        // A trailing separator on the root must not change the answer.
        assert!(starts_with_ignore_ascii_case(
            Path::new(r"C:\Users\RUNNER\data\"),
            Path::new(r"C:\Users\RUNNER\data\a.txt")
        ));
        // `data2` is a sibling, not a child: prefix matching alone would say yes.
        assert!(!starts_with_ignore_ascii_case(
            root,
            Path::new(r"C:\Users\RUNNER\data2\a.txt")
        ));
        // The root itself is not *under* the root.
        assert!(!starts_with_ignore_ascii_case(root, root));
    }

    #[test]
    fn path_under_never_folds_case_on_unix() {
        let root = Path::new("/srv/data");
        assert!(path_under(root, Path::new("/srv/data/a.txt")));
        assert!(!path_under(root, Path::new("/srv/data2/a.txt")));
        // Two files that differ only in case are two different files on Unix.
        assert_eq!(
            path_under(root, Path::new("/srv/DATA/a.txt")),
            cfg!(windows)
        );
    }

    #[test]
    fn usn_is_the_default_on_windows_with_mtime_fallback() {
        // On non-Windows platforms the default is always the mtime source.
        // On Windows the USN source is tried first; it falls back silently
        // to mtime when the probe fails (no admin rights, non-NTFS, etc.).
        let source = source_for(&DeltaConfig::new("."));
        #[cfg(not(windows))]
        {
            assert_eq!(source.kind(), DeltaSourceKind::Mtime);
            assert_eq!(source.detects_renames(), cfg!(unix));
        }
        #[cfg(windows)]
        {
            // The probe may succeed or fail depending on the CI runner's
            // privileges and filesystem — both outcomes are valid defaults.
            assert!(matches!(
                source.kind(),
                DeltaSourceKind::Usn | DeltaSourceKind::Mtime
            ));
        }
    }

    #[test]
    fn only_an_explicit_mtime_value_forces_the_stat_walk() {
        // The override decision is pure so it can be pinned on every platform
        // without mutating the process environment.
        assert!(forces_mtime(Some("mtime")));
        assert!(forces_mtime(Some("MTIME")));
        assert!(!forces_mtime(Some("usn")));
        assert!(!forces_mtime(Some("")));
        assert!(!forces_mtime(Some("anything-else")));
        assert!(!forces_mtime(None));
    }

    #[test]
    fn a_usn_marker_still_gives_the_mtime_source_a_threshold() {
        let marker = ScanMarker::Usn {
            volume: "C:".into(),
            journal_id: 1,
            next_usn: 5,
            maximum_size: 1,
            recorded_ns: 42,
        };
        assert_eq!(marker.wall_clock_ns(), 42);
    }
}
