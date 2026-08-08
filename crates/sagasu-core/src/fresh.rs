//! Search-time delta merge — steps 3 and 4 of the freshness design (design.md §5).
//!
//! ## The claim being implemented
//!
//! "The index is allowed to be stale" only holds if the *answer* is not. This
//! module is what makes that true: it takes the index result, overlays the
//! (normally tiny) set of files that changed since the index marker, and returns
//! a merged result in which a file added, edited or deleted since the last crawl
//! behaves as if the index had been rebuilt.
//!
//! ```text
//!   index result ──┐
//!                  ├─ merge ─→ answer
//!   delta set ─────┘   · index hit that changed  → dropped, live hit replaces it
//!   (live grep)        · index hit that is gone  → dropped
//!                      · live hit not in index   → added
//! ```
//!
//! ## Deletions do not come from the delta source
//!
//! The mtime fallback cannot see a file that no longer exists — a walk only
//! reports what is there. So deletion is handled on the *other* side of the
//! merge: every index hit that survives the changed-set test is checked for
//! existence before it is returned. That check is bounded by the number of hits
//! (tens), not by the corpus, and it is what makes the fallback source complete
//! rather than merely fast. The USN source does report deletions, and they
//! simply take the same path.
//!
//! ## Ordering: live hits first
//!
//! Index hits carry a BM25 score; live hits carry a term-occurrence count from
//! [`crate::fulltext::LiveTerms`]. Those two numbers are not comparable, so
//! sorting them into one ranking would be inventing a relevance claim. Instead
//! live hits are emitted first, ordered by score then by recency:
//!
//! - it is honest — the two groups are labelled ([`HitOrigin`]) and never
//!   interleaved on a fabricated common scale;
//! - a file you just edited is a good bet for what you are looking for;
//! - it guarantees the fresh results are never the ones cut off by `--limit`,
//!   which would defeat the entire mechanism.
//!
//! Leading is not the same as owning the page, though: live hits are capped at
//! half of `--limit` while index hits exist. A common term plus a busy morning
//! of edits would otherwise push the entire ranked result out of sight, turning
//! a freshness correction into a replacement of the search.
//!
//! ## Staleness is reported, never hidden
//!
//! The one thing worse than a stale answer is a stale answer that looks fresh.
//! Every outcome carries [`FreshOutcome::stale`], set when the delta set was
//! truncated at its cap, when the marker expired (issue #16), or when the merge
//! was switched off. The CLI turns it into a visible warning.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};

use crate::delta::{self, DeltaCache, DeltaSet, DeltaSourceKind, DeltaStatus, RescanReason};
use crate::fulltext::{self, SearchConfig};
use crate::store::{FileRow, Store};
use crate::text::{self, ExtVerdict};

// ── Config ──────────────────────────────────────────────────────────────────

/// Configuration for a freshness-merged search.
#[derive(Debug, Clone)]
pub struct FreshConfig {
    /// SQLite metadata index. Supplies the crawl root, the delta marker and —
    /// for [`find`] — the index side of the result.
    pub db_path: PathBuf,
    /// tantivy index directory. Required by [`search`], ignored by [`find`].
    pub index_dir: Option<PathBuf>,
    /// The query. tantivy syntax for [`search`]; a literal path substring for
    /// [`find`].
    pub query: String,
    /// Maximum number of merged hits.
    pub limit: usize,
    /// Cap on the delta set; above it the answer is reported as stale.
    pub delta_limit: usize,
    /// Skip the delta query entirely and answer from the index alone. The
    /// outcome is still marked stale — an unmerged answer is exactly the thing
    /// this module exists to stop being invisible.
    pub no_delta: bool,
    /// Maximum snippet length in characters ([`search`] only).
    pub snippet_chars: usize,
    /// Body-extraction size limit for the live grep ([`search`] only).
    pub max_size: u64,
    /// The extension policy the live grep judges changed files by ([`search`]
    /// only).
    ///
    /// Must match what `sagasu fulltext` was built with. If the index contains
    /// `.foo` files because the user asked for them, but the live grep does not
    /// know about `.foo`, then editing one makes it vanish from the result: the
    /// index hit is dropped as changed and no live hit replaces it.
    pub text_policy: text::TextPolicy,
}

impl FreshConfig {
    /// Config with the documented defaults for the given database and query.
    pub fn new(db_path: impl Into<PathBuf>, query: impl Into<String>) -> Self {
        Self {
            db_path: db_path.into(),
            index_dir: None,
            query: query.into(),
            limit: 10,
            delta_limit: delta::DEFAULT_DELTA_LIMIT,
            no_delta: false,
            snippet_chars: 160,
            max_size: fulltext::DEFAULT_MAX_SIZE,
            text_policy: text::TextPolicy::empty(),
        }
    }
}

// ── Outcome ─────────────────────────────────────────────────────────────────

/// Which side of the merge a hit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitOrigin {
    /// From the index. Path and metadata are as of the last crawl (but the file
    /// is known to still exist and to be unchanged since the marker).
    Index,
    /// From the live grep of the delta set. Everything about it is current.
    Live,
}

impl HitOrigin {
    /// Stable label used in CLI output.
    pub fn as_str(self) -> &'static str {
        match self {
            HitOrigin::Index => "index",
            HitOrigin::Live => "live",
        }
    }
}

/// One merged result.
#[derive(Debug, Clone)]
pub struct FreshHit {
    /// Which side produced it.
    pub origin: HitOrigin,
    /// Stable schema-v0 file ID. `None` for a live hit on a file the index has
    /// never seen — it has no ID until the next crawl.
    pub file_id: Option<i64>,
    /// Current path.
    pub path: String,
    /// BM25 for index hits, term-occurrence count for live hits. Comparable
    /// only within one [`HitOrigin`] — see the module docs.
    pub score: f32,
    /// Size in bytes, freshly stat'ed for live hits.
    pub size: Option<i64>,
    /// Modification time (unix ns), freshly stat'ed for live hits.
    pub mtime_ns: Option<i64>,
    /// One-line excerpt ([`search`] only).
    pub snippet: String,
}

/// Why an answer may be missing changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleKind {
    /// The delta set hit its cap; changes beyond it were not looked at.
    DeltaTruncated,
    /// The delta could not be determined at all — only a re-crawl fixes it.
    RescanRequired(RescanReason),
    /// The merge was switched off by the caller.
    MergeDisabled,
}

/// The "your index is stale" notice, ready to print.
#[derive(Debug, Clone)]
pub struct StaleNotice {
    /// Machine-readable reason.
    pub kind: StaleKind,
    /// One-line explanation including the numbers behind it.
    pub message: String,
}

/// Timing breakdown of a merged search, in milliseconds.
///
/// Split into stages on purpose: the whole freshness claim is "the delta query
/// plus the merge is unnoticeable", and that is only checkable if `delta_ms`,
/// `live_ms` and `merge_ms` can be read separately from the index query they sit
/// next to.
#[derive(Debug, Clone, Copy, Default)]
pub struct FreshTiming {
    /// Opening the database and the tantivy index (excluded from `total_ms`;
    /// a resident process pays it once, a CLI invocation pays it every time).
    pub setup_ms: f64,
    /// Querying the index.
    pub index_ms: f64,
    /// Asking the delta source what changed. `0` on a cache hit.
    pub delta_ms: f64,
    /// Live-grepping the delta set.
    pub live_ms: f64,
    /// Merging the two result sets.
    pub merge_ms: f64,
    /// `index_ms + delta_ms + live_ms + merge_ms`.
    pub total_ms: f64,
}

impl FreshTiming {
    /// The part of the latency the freshness mechanism is responsible for.
    pub fn overhead_ms(&self) -> f64 {
        self.delta_ms + self.live_ms + self.merge_ms
    }
}

/// What the delta source reported, for display and measurement.
#[derive(Debug, Clone)]
pub struct DeltaReport {
    /// Which mechanism ran.
    pub kind: DeltaSourceKind,
    /// Whether that mechanism can see renames at all. False means a file
    /// renamed since the crawl is missing from the answer rather than moved in
    /// it — see [`crate::delta::DeltaSource::detects_renames`].
    pub detects_renames: bool,
    /// Completeness of the delta set.
    pub status: DeltaStatus,
    /// Files in the delta set after exclusion.
    pub entries: usize,
    /// Files stat'ed / journal records read.
    pub scanned: u64,
    /// Candidates dropped by the exclusion set (issue #16's noise ratio).
    pub excluded: u64,
    /// Records dropped because their parent directory no longer exists
    /// (issue #57, USN source only). The path they describe is gone, so
    /// dropping them loses no real change — counted apart from
    /// [`DeltaReport::excluded`] and [`DeltaReport::errors`] on purpose.
    pub gone: u64,
    /// Distinct Win32 codes seen on failed parent-FRN opens, sorted (issue
    /// #57, USN source only). Reported because the "which code means *gone*"
    /// set is documented rather than observed; empty in the ordinary case.
    pub frn_error_codes: Vec<u32>,
    /// Entries the scan could not read. Not exclusions: a directory the live
    /// scan cannot open may hold changes this answer does not know about.
    pub errors: u64,
    /// True when the set came from a [`DeltaCache`] rather than the source.
    pub cached: bool,
}

/// Result of a freshness-merged search.
#[derive(Debug, Clone)]
pub struct FreshOutcome {
    /// The merged hits, live first (see module docs).
    pub hits: Vec<FreshHit>,
    /// What the delta source reported. `None` when the merge was disabled.
    pub delta: Option<DeltaReport>,
    /// Set whenever the answer may be missing changes.
    pub stale: Option<StaleNotice>,
    /// Index hits examined before merging.
    pub index_candidates: usize,
    /// Index hits dropped because the delta set says they changed (the live
    /// side replaces them).
    pub dropped_changed: usize,
    /// Index hits dropped because the file no longer exists.
    pub dropped_deleted: usize,
    /// Hits contributed by the live grep.
    pub live_hits: usize,
    /// Files the live grep read ([`search`] only).
    pub live_read: usize,
    /// Documents in the full-text index ([`search`] only).
    pub total_docs: u64,
    /// The extension rule the live grep actually applied ([`search`] only).
    ///
    /// Reported rather than assumed: which rule is in force decides whether an
    /// edited file comes back refreshed or disappears, so the caller has to be
    /// able to print it next to the answer.
    pub text_policy: text::TextPolicy,
    /// Set when the caller's extension rule and the one the index was built
    /// with disagree ([`search`] only). Print it — a live grep that judges
    /// files differently from the build is exactly how a file goes missing.
    pub text_policy_notice: Option<String>,
    /// Stage-by-stage latency.
    pub timing: FreshTiming,
}

// ── Metadata search ─────────────────────────────────────────────────────────

/// Path search over the metadata index, merged with the delta set.
///
/// The query is a literal, case-insensitive substring of the path. A file
/// created, renamed or deleted since the last crawl is answered correctly
/// without re-indexing: creations and renames arrive through the delta set,
/// deletions through the existence check.
pub fn find(config: &FreshConfig, cache: Option<&DeltaCache>) -> Result<FreshOutcome> {
    if config.query.trim().is_empty() {
        bail!("empty query");
    }

    let t_setup = Instant::now();
    let store = Store::open(&config.db_path)
        .with_context(|| format!("failed to open metadata index {:?}", config.db_path))?;
    let setup_ms = ms(t_setup);

    let mut timing = FreshTiming {
        setup_ms,
        ..Default::default()
    };
    let ctx = DeltaContext::acquire(&store, config, cache, &mut timing)?;

    // The live side is pure metadata here — the delta entries already carry a
    // fresh stat, so "grepping" them is a substring test on the path. Running it
    // before the index query is what lets the over-fetch below stay tight: only
    // a delta entry that *matches* can displace an index row.
    let t_live = Instant::now();
    let needle = config.query.to_lowercase();
    let mut live: Vec<FreshHit> = ctx
        .entries()
        .iter()
        .filter(|e| e.exists && e.path.to_lowercase().contains(&needle))
        .map(|e| FreshHit {
            origin: HitOrigin::Live,
            file_id: None,
            path: e.path.clone(),
            score: 1.0,
            size: Some(e.size),
            mtime_ns: Some(e.mtime_ns),
            snippet: String::new(),
        })
        .collect();
    timing.live_ms = ms(t_live);

    // Over-fetch by the number of live matches: each of them may drop an index
    // row, and a merged page should still fill up. Deletions can shorten it
    // further — they are invisible until the existence check below.
    let over = (config.limit + live.len()) as i64;
    let t_index = Instant::now();
    let rows = store.find_paths_like(&config.query, over)?;
    timing.index_ms = ms(t_index);

    let t_merge = Instant::now();
    let changed = ctx.existence();
    let mut dropped_changed = 0usize;
    let mut dropped_deleted = 0usize;
    let index_candidates = rows.len();

    let mut indexed: Vec<FreshHit> = Vec::with_capacity(rows.len());
    for row in &rows {
        match changed.get(row.path.as_str()).copied() {
            // The delta says the file is gone — a deletion, not a change,
            // whichever source reported it.
            Some(false) => {
                dropped_deleted += 1;
                continue;
            }
            Some(true) => {
                dropped_changed += 1;
                continue;
            }
            None => {}
        }
        if !Path::new(&row.path).exists() {
            dropped_deleted += 1;
            continue;
        }
        indexed.push(index_hit(row));
    }

    // A live hit that the index also knows about carries the file's stable ID
    // through, so the caller can still join it to tags and history.
    let by_path: std::collections::HashMap<&str, &FileRow> =
        rows.iter().map(|r| (r.path.as_str(), r)).collect();
    for hit in &mut live {
        hit.file_id = by_path.get(hit.path.as_str()).map(|r| r.file_id);
    }

    let hits = order_and_truncate(live, indexed, config.limit);
    timing.merge_ms = ms(t_merge);
    timing.total_ms = timing.index_ms + timing.delta_ms + timing.live_ms + timing.merge_ms;

    Ok(FreshOutcome {
        live_hits: hits.iter().filter(|h| h.origin == HitOrigin::Live).count(),
        hits,
        delta: ctx.report(),
        stale: ctx.stale_notice(),
        index_candidates,
        dropped_changed,
        dropped_deleted,
        live_read: 0,
        total_docs: 0,
        // `find` matches on paths and never reads a body, so no extension rule
        // is applied and none is claimed.
        text_policy: text::TextPolicy::empty(),
        text_policy_notice: None,
        timing,
    })
}

// ── Full-text search ────────────────────────────────────────────────────────

/// Full-text search, merged with a live grep of the delta set.
///
/// Requires `config.index_dir`. The live grep applies the *analyzed* terms of
/// the same parsed query to the changed files, so both sides of the merge answer
/// the same question — within the approximation documented on
/// [`crate::fulltext::LiveTerms`].
pub fn search(config: &FreshConfig, cache: Option<&DeltaCache>) -> Result<FreshOutcome> {
    let Some(index_dir) = config.index_dir.clone() else {
        bail!("fresh::search needs a full-text index directory");
    };
    if config.query.trim().is_empty() {
        bail!("empty query");
    }

    let t_setup = Instant::now();
    let store = Store::open(&config.db_path)
        .with_context(|| format!("failed to open metadata index {:?}", config.db_path))?;
    let setup_ms = ms(t_setup);

    let mut timing = FreshTiming {
        setup_ms,
        ..Default::default()
    };
    let ctx = DeltaContext::acquire(&store, config, cache, &mut timing)?;

    // The extension rule the live grep judges changed files by. It comes from
    // the index unless the caller stated one, so that a search run from another
    // directory applies the same rule the build did (issue #15).
    let (text_policy, text_policy_notice) = resolve_text_policy(&store, &config.text_policy);

    // Same over-fetch reasoning as `find`, plus tantivy's own limit semantics:
    // asking for exactly `limit` and then dropping half of them would silently
    // shorten the page.
    let mut search_config = SearchConfig::new(&index_dir, &config.query);
    search_config.db_path = Some(config.db_path.clone());
    search_config.limit = (config.limit + ctx.entries().len()).max(config.limit);
    search_config.snippet_chars = config.snippet_chars;

    let t_index = Instant::now();
    let indexed = fulltext::search(&search_config)?;
    timing.index_ms = ms(t_index);
    // `fulltext::search` measures matching; the setup it does (opening the index
    // and loading the Lindera dictionary) belongs with the other fixed costs.
    timing.setup_ms += timing.index_ms - indexed.elapsed_ms;
    timing.index_ms = indexed.elapsed_ms;

    // ── Live grep of the delta set ──────────────────────────────────────────
    let t_live = Instant::now();
    let snippet_terms = fulltext::body_query_terms(&indexed.terms);
    let mut live_read = 0usize;
    let mut live: Vec<FreshHit> = Vec::new();
    for entry in ctx.entries() {
        if !entry.exists {
            continue;
        }
        let Some(body) = read_body(&entry.path, entry.size, config.max_size, &text_policy) else {
            continue;
        };
        live_read += 1;
        let score = indexed.terms.score(&body);
        if score == 0 {
            continue;
        }
        live.push(FreshHit {
            origin: HitOrigin::Live,
            file_id: None,
            path: entry.path.clone(),
            score: score as f32,
            size: Some(entry.size),
            mtime_ns: Some(entry.mtime_ns),
            snippet: fulltext::build_snippet(&body, &snippet_terms, config.snippet_chars),
        });
    }
    timing.live_ms = ms(t_live);

    // ── Merge ───────────────────────────────────────────────────────────────
    let t_merge = Instant::now();
    let changed = ctx.existence();
    let mut dropped_changed = 0usize;
    let mut dropped_deleted = 0usize;
    let index_candidates = indexed.hits.len();

    let mut from_index: Vec<FreshHit> = Vec::with_capacity(indexed.hits.len());
    for hit in &indexed.hits {
        let path = hit.display_path();
        match changed.get(path).copied() {
            // The delta says the file is gone — a deletion, not a change,
            // whichever source reported it.
            Some(false) => {
                dropped_deleted += 1;
                continue;
            }
            Some(true) => {
                dropped_changed += 1;
                continue;
            }
            None => {}
        }
        // `deleted` is what SQLite knows; `exists` is what the filesystem knows
        // right now. A file deleted since the last crawl is only visible in the
        // second, which is the whole point of asking.
        if hit.deleted || !Path::new(path).exists() {
            dropped_deleted += 1;
            continue;
        }
        from_index.push(FreshHit {
            origin: HitOrigin::Index,
            file_id: (hit.file_id >= 0).then_some(hit.file_id),
            path: path.to_string(),
            score: hit.score,
            size: None,
            mtime_ns: Some(hit.mtime_ns),
            snippet: hit.snippet.clone(),
        });
    }

    let known: std::collections::HashMap<&str, i64> = indexed
        .hits
        .iter()
        .map(|h| (h.display_path(), h.file_id))
        .collect();
    for hit in &mut live {
        hit.file_id = known.get(hit.path.as_str()).copied().filter(|id| *id >= 0);
    }

    let hits = order_and_truncate(live, from_index, config.limit);
    timing.merge_ms = ms(t_merge);
    timing.total_ms = timing.index_ms + timing.delta_ms + timing.live_ms + timing.merge_ms;

    Ok(FreshOutcome {
        live_hits: hits.iter().filter(|h| h.origin == HitOrigin::Live).count(),
        hits,
        delta: ctx.report(),
        stale: ctx.stale_notice(),
        index_candidates,
        dropped_changed,
        dropped_deleted,
        live_read,
        total_docs: indexed.total_docs,
        text_policy,
        text_policy_notice,
        timing,
    })
}

/// Decide which extension rule the live grep applies, and whether to say
/// something about it.
///
/// - The caller stated nothing → use what the full-text build recorded. This is
///   the case that used to break: `sagasu fulltext` picks up `./sagasu-text.toml`
///   automatically, and a `sagasu search` run from anywhere else did not, so the
///   two sides silently disagreed.
/// - The caller stated something → it wins, because an explicit `--ext` is the
///   escape hatch for an index that is already built. Say so when it disagrees.
/// - The recorded rule is unreadable → fall back to the caller's, and say so.
///   A search that refuses to run is worse than one that explains itself.
fn resolve_text_policy(
    store: &Store,
    requested: &text::TextPolicy,
) -> (text::TextPolicy, Option<String>) {
    let stored = match text::TextPolicy::from_index(store) {
        Ok(stored) => stored,
        Err(e) => {
            return (
                requested.clone(),
                Some(format!(
                    "the full-text index's extension policy could not be read back ({e:#}); \
                     the live scan used {} instead",
                    requested.describe()
                )),
            )
        }
    };

    match stored {
        Some(stored) if requested.is_empty() => (stored, None),
        Some(stored) if !stored.agrees_with(requested) => {
            let notice = format!(
                "the full-text index was built with {} but this search was given {}; \
                 the live scan used the latter, so a file the two disagree about is \
                 answered differently depending on whether it changed since the build",
                stored.describe(),
                requested.describe()
            );
            (requested.clone(), Some(notice))
        }
        _ => (requested.clone(), None),
    }
}

// ── Delta acquisition ───────────────────────────────────────────────────────

/// The delta set for one search, plus everything needed to report it.
///
/// Exists so `find` and `search` share exactly one implementation of "get the
/// marker, pick a source, run or reuse the query, decide whether the answer is
/// stale" — the part where a divergence between the two would be a freshness
/// bug rather than a cosmetic one.
struct DeltaContext {
    set: Option<std::sync::Arc<DeltaSet>>,
    cached: bool,
    /// Set when there is no usable marker: no delta can be taken at all.
    missing_marker: bool,
    /// Set when the crawl's exclusion policy could not be read back, with the
    /// reason. The query still answers — from the index alone, marked stale —
    /// because refusing to search at all is a worse answer than an honestly
    /// labelled partial one, and this failure can be caused by nothing more
    /// than opening a newer index with an older binary.
    policy_error: Option<String>,
}

impl DeltaContext {
    fn acquire(
        store: &Store,
        config: &FreshConfig,
        cache: Option<&DeltaCache>,
        timing: &mut FreshTiming,
    ) -> Result<Self> {
        if config.no_delta {
            return Ok(Self {
                set: None,
                cached: false,
                missing_marker: false,
                policy_error: None,
            });
        }

        let Some(marker) = store.delta_marker()? else {
            return Ok(Self {
                set: None,
                cached: false,
                missing_marker: true,
                policy_error: None,
            });
        };
        // The crawl's own exclusion policy, replayed exactly — including
        // `--exclude`, `--skip-hidden` and `--use-gitignore` (design.md §5-1).
        let delta_config = match delta::DeltaConfig::from_index(store, &config.db_path) {
            Ok(Some(config)) => config,
            Ok(None) => {
                return Ok(Self {
                    set: None,
                    cached: false,
                    missing_marker: true,
                    policy_error: None,
                })
            }
            Err(e) => {
                return Ok(Self {
                    set: None,
                    cached: false,
                    missing_marker: false,
                    policy_error: Some(format!("{e:#}")),
                })
            }
        };
        let source = delta::source_for(&delta_config);

        let t = Instant::now();
        let (set, cached) = match cache {
            Some(cache) => cache.get(source.as_ref(), &marker, config.delta_limit)?,
            None => (
                std::sync::Arc::new(source.changes_since(&marker, config.delta_limit)?),
                false,
            ),
        };
        timing.delta_ms = ms(t);

        Ok(Self {
            set: Some(set),
            cached,
            missing_marker: false,
            policy_error: None,
        })
    }

    fn entries(&self) -> &[delta::DeltaEntry] {
        self.set
            .as_ref()
            .map(|s| s.entries.as_slice())
            .unwrap_or(&[])
    }

    /// Delta paths mapped to their current existence, so the merge can tell a
    /// changed file from a deleted one when both arrive as delta entries. A
    /// walk only ever reports files it can still stat, but the USN journal
    /// reports deletions as entries with `exists == false` — classifying by
    /// set membership alone would count those as "changed".
    fn existence(&self) -> std::collections::HashMap<&str, bool> {
        self.entries()
            .iter()
            .map(|e| (e.path.as_str(), e.exists))
            .collect()
    }

    fn report(&self) -> Option<DeltaReport> {
        self.set.as_ref().map(|s| DeltaReport {
            kind: s.kind,
            detects_renames: s.detects_renames,
            status: s.status,
            entries: s.entries.len(),
            scanned: s.scanned,
            excluded: s.excluded,
            gone: s.gone,
            frn_error_codes: s.frn_error_codes.clone(),
            errors: s.errors,
            cached: self.cached,
        })
    }

    fn stale_notice(&self) -> Option<StaleNotice> {
        if let Some(reason) = &self.policy_error {
            return Some(StaleNotice {
                kind: StaleKind::RescanRequired(RescanReason::PolicyUnreadable),
                message: format!(
                    "index is stale: the crawl's exclusion policy could not be read back \
                     ({reason}), so changes since the crawl were not merged — \
                     re-run `sagasu index <root>`"
                ),
            });
        }
        if self.missing_marker {
            return Some(StaleNotice {
                kind: StaleKind::RescanRequired(RescanReason::MarkerMissing),
                message: "index is stale: no freshness marker recorded — \
                          re-run `sagasu index <root>`"
                    .to_string(),
            });
        }
        let Some(set) = self.set.as_ref() else {
            return Some(StaleNotice {
                kind: StaleKind::MergeDisabled,
                message: "index is of unknown freshness: the delta merge was disabled, \
                          so results are as of the last crawl"
                    .to_string(),
            });
        };
        match set.status {
            DeltaStatus::Complete => None,
            DeltaStatus::Truncated { limit } => Some(StaleNotice {
                kind: StaleKind::DeltaTruncated,
                message: format!(
                    "index is stale: more than {limit} files changed since it was built, \
                     so the live scan was cut short — re-run `sagasu index <root>`"
                ),
            }),
            DeltaStatus::RescanRequired(reason) => Some(StaleNotice {
                kind: StaleKind::RescanRequired(reason),
                message: format!(
                    "index is stale: {} — re-run `sagasu index <root>`",
                    reason.as_str()
                ),
            }),
        }
    }
}

// ── internal helpers ────────────────────────────────────────────────────────

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn index_hit(row: &FileRow) -> FreshHit {
    FreshHit {
        origin: HitOrigin::Index,
        file_id: Some(row.file_id),
        path: row.path.clone(),
        score: 1.0,
        size: Some(row.size),
        mtime_ns: Some(row.mtime_ns),
        snippet: String::new(),
    }
}

/// Live hits first, each group internally ordered, then cut to `limit`.
///
/// Within the live group the tie-break is recency: when several files changed
/// since the crawl and match equally well, the one touched last is the better
/// guess.
///
/// Live hits lead but do not take the whole page: at most half of `limit` while
/// there are index hits to fill the rest. Without that reservation a common term
/// plus a busy morning's worth of edits would push the entire ranked result off
/// the page — the freshness mechanism would be *replacing* the search rather
/// than correcting it. When one side runs out the other takes the free slots, so
/// the usual case (a handful of changed files) is unaffected.
fn order_and_truncate(
    mut live: Vec<FreshHit>,
    mut indexed: Vec<FreshHit>,
    limit: usize,
) -> Vec<FreshHit> {
    let limit = limit.max(1);
    live.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| b.mtime_ns.cmp(&a.mtime_ns))
            .then_with(|| a.path.cmp(&b.path))
    });
    indexed.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
    });

    let live_budget = if indexed.is_empty() {
        limit
    } else {
        limit.div_ceil(2)
    };

    let mut seen: HashSet<String> = HashSet::with_capacity(limit);
    let mut out: Vec<FreshHit> = Vec::with_capacity(limit);
    let mut live_rest: Vec<FreshHit> = Vec::new();

    for hit in live {
        if !seen.insert(hit.path.clone()) {
            continue;
        }
        if out.len() < live_budget {
            out.push(hit);
        } else {
            live_rest.push(hit);
        }
    }
    for hit in indexed {
        if out.len() >= limit {
            break;
        }
        if seen.insert(hit.path.clone()) {
            out.push(hit);
        }
    }
    // Index hits ran out before the page did: give the slots back to the live
    // side rather than returning a short page.
    for hit in live_rest {
        if out.len() >= limit {
            break;
        }
        out.push(hit);
    }
    out
}

/// Read a changed file's body if it is a text target, applying the same
/// extension / sniffing decision as the index build.
///
/// Returning `None` (too large, binary, unreadable) is not a failure: it means
/// the file has no body the full-text index would have contained either, so
/// leaving it out of the live result keeps both sides of the merge consistent.
fn read_body(path: &str, size: i64, max_size: u64, policy: &text::TextPolicy) -> Option<String> {
    if size <= 0 || size as u64 > max_size {
        return None;
    }
    match policy.classify_ext(
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
            .as_deref(),
    ) {
        ExtVerdict::Binary => None,
        // A changed `.docx` must be judged by the same rule as the indexed one,
        // or editing a document deletes it from the answer: the index hit is
        // dropped as changed and no live hit replaces it. That is the exact
        // failure the stored `text_policy` exists to prevent (design.md §4-2),
        // and a new format would have reintroduced it on the live side only.
        //
        // A parse failure here is `None` for the same reason the other arms
        // are: the index would not have held a body for this file either, so
        // both sides of the merge stay consistent. The build pass is where an
        // extraction failure gets counted and named.
        ExtVerdict::Extract(format) => {
            crate::docmeta::extract_body(Path::new(path), format, max_size).ok()
        }
        ExtVerdict::Text => std::fs::read(path).ok().map(|b| text::decode(&b)),
        ExtVerdict::Unknown => {
            let bytes = std::fs::read(path).ok()?;
            let head = &bytes[..text::SNIFF_LEN.min(bytes.len())];
            text::sniff_is_text(head).then(|| text::decode(&bytes))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(policy: Option<&str>) -> (Store, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "sagasu_fresh_policy_{}_{:p}.db",
            std::process::id(),
            policy
                .as_ref()
                .map(|s| s.as_ptr())
                .unwrap_or(std::ptr::null())
        ));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        store.ensure_schema_version().unwrap();
        if let Some(p) = policy {
            store.meta_set(text::TEXT_POLICY_KEY, p).unwrap();
        }
        (store, path)
    }

    fn policy(exts: &[&str]) -> text::TextPolicy {
        let mut p = text::TextPolicy::empty();
        p.add_text_exts(&exts.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        p
    }

    #[test]
    fn a_search_that_states_nothing_inherits_the_rule_the_index_was_built_with() {
        // The case that used to break: `sagasu fulltext` picks up
        // ./sagasu-text.toml, and a `sagasu search` run from another directory
        // found no file and silently reverted to the built-in lists — so an
        // edited `.tmpl` was judged binary by the live grep and disappeared
        // from the answer instead of being refreshed (issue #15).
        let (store, path) = store_with(Some(&policy(&["tmpl"]).encode()));
        let (effective, notice) = resolve_text_policy(&store, &text::TextPolicy::empty());

        assert_eq!(effective.text_exts(), ["tmpl"]);
        assert!(notice.is_none(), "inheriting the index's rule is not news");
        assert_eq!(effective.classify_ext(Some("tmpl")), text::ExtVerdict::Text);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_explicit_rule_wins_but_the_disagreement_is_reported() {
        let (store, path) = store_with(Some(&policy(&["tmpl"]).encode()));
        let (effective, notice) = resolve_text_policy(&store, &policy(&["hbs"]));

        assert_eq!(
            effective.text_exts(),
            ["hbs"],
            "--ext stays an escape hatch"
        );
        let notice = notice.expect("a disagreement must be reported");
        assert!(
            notice.contains("tmpl") && notice.contains("hbs"),
            "{notice}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn stating_the_same_rule_the_index_holds_is_not_a_disagreement() {
        let (store, path) = store_with(Some(&policy(&["tmpl"]).encode()));
        let (_, notice) = resolve_text_policy(&store, &policy(&[".TMPL"]));
        assert!(notice.is_none(), "compared by effect, not by spelling");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_unreadable_recorded_rule_degrades_instead_of_failing_the_search() {
        let (store, path) = store_with(Some("v99\ntext=tmpl\n"));
        let (effective, notice) = resolve_text_policy(&store, &policy(&["hbs"]));

        assert_eq!(effective.text_exts(), ["hbs"]);
        assert!(
            notice
                .expect("must say so")
                .contains("could not be read back"),
            "a search that refuses to run is worse than one that explains itself"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_index_with_no_recorded_rule_leaves_the_caller_alone() {
        let (store, path) = store_with(None);
        let (effective, notice) = resolve_text_policy(&store, &policy(&["hbs"]));
        assert_eq!(effective.text_exts(), ["hbs"]);
        assert!(notice.is_none());
        let _ = std::fs::remove_file(path);
    }
}
