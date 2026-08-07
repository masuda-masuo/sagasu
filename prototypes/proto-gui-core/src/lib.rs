//! proto-gui-core: measurement-instrument library for M2 incremental search.
//!
//! This crate contains ALL search/merge/stats logic and is fully buildable on
//! Linux without any GUI system libraries. The Tauri shell (`proto-gui`) is a
//! thin caller that delegates everything to this crate.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Instant, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::QueryParser;
use tantivy::schema::{IndexRecordOption, Schema, TextFieldIndexing, TextOptions, STORED, STRING};
use tantivy::schema::Value;
use tantivy::{doc, Index, TantivyDocument};

const TEXT_EXTS: &[&str] = &[
    "txt", "md", "rst", "adoc", "csv", "tsv", "log", "ini", "cfg", "conf", "json", "yaml",
    "yml", "toml", "xml", "html", "css", "rs", "py", "js", "ts", "go", "java", "c", "h", "cpp",
    "hpp", "cs", "rb", "sh", "ps1", "bat", "sql", "tex",
];

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Search freshness mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum SearchMode {
    /// Index-only search (no delta walk, no live-grep).
    IndexOnly,
    /// Delta walk + live-grep on every query.
    DeltaEveryQuery,
    /// Delta walk is cached for `ttl_ms` milliseconds; live-grep still runs
    /// on every query (the cache holds the changed-path *set*, not the grep
    /// results).
    DeltaCached { ttl_ms: u64 },
}

/// A single search hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    /// "index" or "live"
    pub source: String,
    pub path: String,
    /// For index hits: the tantivy score. For live hits: the match count.
    pub score_or_count: f32,
}

/// Timing breakdown for a single search, in milliseconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimingData {
    pub search_ms: f64,
    pub delta_walk_ms: f64,
    pub live_grep_ms: f64,
    pub merge_ms: f64,
    pub total_ms: f64,
    /// Client-measured IPC round-trip (filled by the shell).
    pub ipc_ms: f64,
    /// Client-measured render time (filled by the shell).
    pub render_ms: f64,
}

impl TimingData {
    pub fn zero() -> Self {
        Self {
            search_ms: 0.0,
            delta_walk_ms: 0.0,
            live_grep_ms: 0.0,
            merge_ms: 0.0,
            total_ms: 0.0,
            ipc_ms: 0.0,
            render_ms: 0.0,
        }
    }
}

/// Response from Engine::search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub hits: Vec<Hit>,
    pub total_hits_before_limit: usize,
    pub timings: TimingData,
    pub delta_changed_count: usize,
    pub delta_cache_hit: bool,
    pub delta_cache_age_ms: f64,
}

/// One JSONL record written by export_log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonlRecord {
    pub timestamp_unix_s: f64,
    pub query: String,
    pub mode: String,
    pub limit: usize,
    pub timings: TimingData,
    pub delta_changed_count: usize,
    pub delta_cache_hit: bool,
    pub delta_cache_age_ms: f64,
    pub total_hits: usize,
}

// ---------------------------------------------------------------------------
// Rolling percentile helper
// ---------------------------------------------------------------------------

/// A simple rolling-percentile calculator that keeps a fixed-size window of
/// recent samples and computes p50/p95 on demand.
#[derive(Debug, Clone)]
pub struct RollingPercentile {
    window: VecDeque<f64>,
    cap: usize,
}

impl RollingPercentile {
    /// Create a new rolling percentile calculator with the given window size.
    pub fn new(cap: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(cap),
            cap,
        }
    }

    /// Push a new sample. If the window is full, the oldest sample is dropped.
    pub fn push(&mut self, value: f64) {
        if self.window.len() >= self.cap {
            self.window.pop_front();
        }
        self.window.push_back(value);
    }

    /// Return (p50, p95) for the current window. Returns (f64::NAN, f64::NAN)
    /// when the window is empty.
    pub fn percentiles(&self) -> (f64, f64) {
        let n = self.window.len();
        if n == 0 {
            return (f64::NAN, f64::NAN);
        }
        let mut sorted: Vec<f64> = self.window.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p50 = percentile_of_sorted(&sorted, 50.0);
        let p95 = percentile_of_sorted(&sorted, 95.0);
        (p50, p95)
    }

    /// Number of samples currently held.
    pub fn len(&self) -> usize {
        self.window.len()
    }

    /// Whether the window is empty.
    pub fn is_empty(&self) -> bool {
        self.window.is_empty()
    }

    /// Clear all samples.
    pub fn clear(&mut self) {
        self.window.clear();
    }
}

/// Compute the p-th percentile from a sorted slice using linear interpolation.
fn percentile_of_sorted(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return f64::NAN;
    }
    if n == 1 {
        return sorted[0];
    }
    let rank = (p / 100.0) * (n as f64 - 1.0);
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let frac = rank - lo as f64;
    sorted[lo] + frac * (sorted[hi] - sorted[lo])
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Delta-walk cache: the changed-path set and the timestamp when it was
/// collected.
struct DeltaCache {
    /// Set of paths changed since the index marker.
    changed: HashSet<String>,
    /// Instant when the walk completed.
    collected_at: Instant,
}

/// The search engine. Holds a long-lived tantivy index handle and the
/// freshness delta cache.
pub struct Engine {
    index: Index,
    schema: Schema,
    /// Marker_ns + root read from sagasu-meta.txt.
    marker_ns: i64,
    root: PathBuf,
    /// Optional delta cache.
    delta_cache: Option<DeltaCache>,
}

// --- helpers (copied from proto-fulltext, intentionally duplicated) ---

fn is_text_target(path: &Path, size: u64, max_size: u64) -> bool {
    if size > max_size {
        return false;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| TEXT_EXTS.contains(&e.to_lowercase().as_str()))
}

fn mtime_ns_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn meta_path(index_dir: &Path) -> PathBuf {
    index_dir.join("sagasu-meta.txt")
}

#[allow(dead_code)]
fn build_schema() -> Schema {
    let mut b = Schema::builder();
    b.add_text_field("path", STRING | STORED);
    b.add_i64_field("mtime_ns", STORED);
    b.add_text_field(
        "body",
        TextOptions::default().set_stored().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("lang_ja")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        ),
    );
    b.build()
}

fn register_ja_tokenizer(index: &Index) -> Result<()> {
    use lindera::dictionary::{load_embedded_dictionary, DictionaryKind};
    use lindera::mode::Mode;
    use lindera::segmenter::Segmenter;
    use lindera_tantivy::tokenizer::LinderaTokenizer;

    let dictionary = load_embedded_dictionary(DictionaryKind::IPADIC)
        .map_err(|e| anyhow::anyhow!("IPADIC read failed: {e}"))?;
    let segmenter = Segmenter::new(Mode::Normal, dictionary, None);
    index
        .tokenizers()
        .register("lang_ja", LinderaTokenizer::from_segmenter(segmenter));
    Ok(())
}

fn read_meta_from_dir(index_dir: &Path) -> Result<(i64, PathBuf)> {
    let s = std::fs::read_to_string(meta_path(index_dir))
        .context("sagasu-meta.txt not found (run `proto-fulltext index` first)")?;
    let mut marker = None;
    let mut root = None;
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("marker_ns=") {
            marker = v.parse::<i64>().ok();
        } else if let Some(v) = line.strip_prefix("root=") {
            root = Some(PathBuf::from(v));
        }
    }
    match (marker, root) {
        (Some(m), Some(r)) => Ok((m, r)),
        _ => bail!("sagasu-meta.txt is malformed"),
    }
}

// ---------------------------------------------------------------------------
// Engine impl
// ---------------------------------------------------------------------------

impl Engine {
    /// Open the index once, register the `lang_ja` tokenizer, and read the
    /// sagasu-meta.txt marker + root. This is the fixed cost paid once at
    /// process startup.
    pub fn open(index_dir: impl AsRef<Path>) -> Result<Self> {
        let index_dir = index_dir.as_ref();
        let index =
            Index::open_in_dir(index_dir).context("cannot open index (run `proto-fulltext index` first)")?;
        register_ja_tokenizer(&index)?;
        let schema = index.schema();
        let (marker_ns, root) = read_meta_from_dir(index_dir)?;
        Ok(Self {
            index,
            schema,
            marker_ns,
            root,
            delta_cache: None,
        })
    }

    /// Execute a search. The `mode` controls freshness behaviour (see
    /// `SearchMode`), and `max_size` caps the file size for live-grep (same
    /// meaning as in proto-fulltext).
    pub fn search(
        &mut self,
        query: &str,
        limit: usize,
        mode: &SearchMode,
        max_size: u64,
    ) -> Result<SearchResponse> {
        let t_total = Instant::now();

        let body_f = self.schema.get_field("body")?;
        let path_f = self.schema.get_field("path")?;

        // --- 1. Index search ---
        let t0 = Instant::now();
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let query_parsed = QueryParser::for_index(&self.index, vec![body_f]).parse_query(query)?;
        let (top, index_match_count) =
            // tantivy 0.26: `TopDocs` is a builder; the ordering has to be
            // named.  `order_by_score()` is the 0.25 behaviour.
            searcher.search(
                &query_parsed,
                &(TopDocs::with_limit(limit).order_by_score(), Count),
            )?;
        let search_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // --- 2. Delta walk + live-grep ---
        let (live_hits, changed, delta_ms, grep_ms, cache_hit, cache_age_ms) = match mode {
            SearchMode::IndexOnly => {
                // No delta at all.
                (Vec::new(), HashSet::new(), 0.0, 0.0, false, 0.0)
            }
            SearchMode::DeltaEveryQuery => {
                // Force re-walk every time.
                self.delta_cache = None;
                let (lh, ch, dw_ms, lg_ms) =
                    live_scan(&self.root, self.marker_ns, query, max_size);
                (lh, ch, dw_ms, lg_ms, false, 0.0)
            }
            SearchMode::DeltaCached { ttl_ms } => {
                let _now = Instant::now();
                let use_cache = self
                    .delta_cache
                    .as_ref()
                    .is_some_and(|c| (c.collected_at.elapsed().as_millis() as u64) < *ttl_ms);

                if use_cache {
                    let cache = self.delta_cache.as_ref().unwrap();
                    let cache_age = cache.collected_at.elapsed().as_secs_f64() * 1000.0;
                    // Run live-grep on the cached changed set.
                    let (lh, lg_ms) = live_grep_on_set(&cache.changed, query);
                    (
                        lh,
                        cache.changed.clone(),
                        0.0, // no walk
                        lg_ms,
                        true,
                        cache_age,
                    )
                } else {
                    // Cache miss: re-walk.
                    let (lh, ch, dw_ms, lg_ms) =
                        live_scan(&self.root, self.marker_ns, query, max_size);
                    let cache_age = 0.0;
                    self.delta_cache = Some(DeltaCache {
                        changed: ch.clone(),
                        collected_at: Instant::now(),
                    });
                    (lh, ch, dw_ms, lg_ms, false, cache_age)
                }
            }
        };

        // --- 3. Merge ---
        let t2 = Instant::now();
        let mut hits: Vec<Hit> = Vec::new();
        let mut dropped_changed = 0u32;
        let mut dropped_deleted = 0u32;

        for (score, addr) in &top {
            let doc: TantivyDocument = searcher.doc(*addr)?;
            let Some(path) = doc.get_first(path_f).and_then(|v| v.as_str()) else {
                continue;
            };
            if changed.contains(path) {
                dropped_changed += 1;
                continue;
            }
            if !Path::new(path).exists() {
                dropped_deleted += 1;
                continue;
            }
            hits.push(Hit {
                source: "index".into(),
                path: path.to_string(),
                score_or_count: *score,
            });
        }
        for (path, count) in &live_hits {
            hits.push(Hit {
                source: "live".into(),
                path: path.clone(),
                score_or_count: *count as f32,
            });
        }

        // True pre-limit count: all index matches (from the Count collector,
        // not the limit-capped `top`) plus live hits. Index-side merge drops
        // (changed/deleted) are not subtracted — this counts matches, not
        // displayed rows.
        let total_before = index_match_count + live_hits.len();
        let _ = dropped_changed; // kept for trace if needed
        let _ = dropped_deleted;

        let merge_ms = t2.elapsed().as_secs_f64() * 1000.0;
        let total_ms = t_total.elapsed().as_secs_f64() * 1000.0;

        Ok(SearchResponse {
            hits,
            total_hits_before_limit: total_before,
            timings: TimingData {
                search_ms,
                delta_walk_ms: delta_ms,
                live_grep_ms: grep_ms,
                merge_ms,
                total_ms,
                ipc_ms: 0.0,
                render_ms: 0.0,
            },
            delta_changed_count: changed.len(),
            delta_cache_hit: cache_hit,
            delta_cache_age_ms: cache_age_ms,
        })
    }

    /// Manually invalidate the delta cache. The next call to `search` with
    /// `DeltaCached` will re-walk.
    pub fn refresh_delta(&mut self) {
        self.delta_cache = None;
    }

    /// Accessor for the root path (for the frontend to display).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Accessor for the marker timestamp.
    pub fn marker_ns(&self) -> i64 {
        self.marker_ns
    }
}

// ---------------------------------------------------------------------------
// Live scan (copied from proto-fulltext cmd_fresh_search)
// ---------------------------------------------------------------------------

/// Walk the filesystem, find files whose mtime > marker_ns (text files under
/// max_size), then live-grep each for the query. Returns (live_hits,
/// changed_set, delta_walk_ms, live_grep_ms).
fn live_scan(
    root: &Path,
    marker_ns: i64,
    query: &str,
    max_size: u64,
) -> (Vec<(String, usize)>, HashSet<String>, f64, f64) {
    let query_lower = query.to_lowercase();

    // 1) Delta detection: walk the tree, stat-only (fast).
    let t0 = Instant::now();
    let (tx, rx) = mpsc::channel::<String>();
    let walker = WalkBuilder::new(root)
        .threads(
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
        )
        .build_parallel();
    std::thread::scope(|s| {
        s.spawn(move || {
            walker.run(|| {
                let tx = tx.clone();
                Box::new(move |entry| {
                    let Ok(entry) = entry else {
                        return ignore::WalkState::Continue;
                    };
                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        return ignore::WalkState::Continue;
                    }
                    let Ok(meta) = entry.metadata() else {
                        return ignore::WalkState::Continue;
                    };
                    if !is_text_target(entry.path(), meta.len(), max_size) {
                        return ignore::WalkState::Continue;
                    }
                    if mtime_ns_of(&meta) > marker_ns {
                        let _ = tx.send(entry.path().to_string_lossy().into_owned());
                    }
                    ignore::WalkState::Continue
                })
            });
        });
    });
    let changed: HashSet<String> = rx.into_iter().collect();
    let delta_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // 2) Live-grep on the changed set.
    let t1 = Instant::now();
    let mut live_hits = Vec::new();
    for path in &changed {
        if let Ok(bytes) = std::fs::read(path) {
            let text = String::from_utf8_lossy(&bytes);
            let count = text.to_lowercase().matches(&query_lower).count();
            if count > 0 {
                live_hits.push((path.clone(), count));
            }
        }
    }
    let grep_ms = t1.elapsed().as_secs_f64() * 1000.0;

    (live_hits, changed, delta_ms, grep_ms)
}

/// Live-grep only (no walk) on a given set of paths.
fn live_grep_on_set(
    changed: &HashSet<String>,
    query: &str,
) -> (Vec<(String, usize)>, f64) {
    let t0 = Instant::now();
    let query_lower = query.to_lowercase();
    let mut live_hits = Vec::new();
    for path in changed {
        if let Ok(bytes) = std::fs::read(path) {
            let text = String::from_utf8_lossy(&bytes);
            let count = text.to_lowercase().matches(&query_lower).count();
            if count > 0 {
                live_hits.push((path.clone(), count));
            }
        }
    }
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    (live_hits, ms)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percentile_empty() {
        let rp = RollingPercentile::new(10);
        let (p50, p95) = rp.percentiles();
        assert!(p50.is_nan());
        assert!(p95.is_nan());
    }

    #[test]
    fn test_percentile_single() {
        let mut rp = RollingPercentile::new(10);
        rp.push(42.0);
        let (p50, p95) = rp.percentiles();
        assert!((p50 - 42.0).abs() < 0.001);
        assert!((p95 - 42.0).abs() < 0.001);
    }

    #[test]
    fn test_percentile_known_sequence() {
        // Values 1..=100, p50 = 50.5, p95 = 95.05 (linear interpolation)
        let mut rp = RollingPercentile::new(100);
        for i in 1..=100 {
            rp.push(i as f64);
        }
        let (p50, p95) = rp.percentiles();
        assert!((p50 - 50.5).abs() < 0.01, "p50={p50}");
        assert!((p95 - 95.05).abs() < 0.01, "p95={p95}");
    }

    #[test]
    fn test_percentile_rolling_window() {
        let mut rp = RollingPercentile::new(5);
        // Push 1,2,3,4,5 -> p50=3
        for i in 1..=5 {
            rp.push(i as f64);
        }
        let (p50, _) = rp.percentiles();
        assert!((p50 - 3.0).abs() < 0.01);

        // Push 6,7,8 -> window becomes [4,5,6,7,8], p50=6
        rp.push(6.0);
        rp.push(7.0);
        rp.push(8.0);
        let (p50, _) = rp.percentiles();
        assert!((p50 - 6.0).abs() < 0.01);
    }

    #[test]
    fn test_percentile_p95() {
        // 100 values 1..=100: p95 = rank 0.95*99 = 94.05 -> lo=94,hi=95 -> 94+1 + 0.05*1 = 95.05
        let mut rp = RollingPercentile::new(100);
        for i in 1..=100 {
            rp.push(i as f64);
        }
        let (_, p95) = rp.percentiles();
        assert!((p95 - 95.05).abs() < 0.01, "p95={p95}");
    }

    // --- Merge tests ---

    /// Build a small in-memory index, write sagasu-meta.txt, then exercise the
    /// merge logic via Engine::search.
    #[test]
    fn test_merge_drops_changed_paths() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("root");
        let index_dir = tmp.path().join("ft-index");

        // Create a couple of text files sharing a common term "common".
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("a.txt"), "common alpha")?;
        std::fs::write(root.join("b.txt"), "common beta")?;

        // Index them.
        let schema = build_schema();
        std::fs::create_dir_all(&index_dir)?;
        let index = Index::create_in_dir(&index_dir, schema.clone())?;
        register_ja_tokenizer(&index)?;
        let marker_ns = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_nanos() as i64;
        {
            let mut w = index.writer(50_000_000)?;
            let path_f = schema.get_field("path")?;
            let mtime_f = schema.get_field("mtime_ns")?;
            let body_f = schema.get_field("body")?;
            w.add_document(doc!(
                path_f => root.join("a.txt").to_string_lossy().as_ref(),
                mtime_f => marker_ns,
                body_f => "common alpha",
            ))?;
            w.add_document(doc!(
                path_f => root.join("b.txt").to_string_lossy().as_ref(),
                mtime_f => marker_ns,
                body_f => "common beta",
            ))?;
            w.commit()?;
        }

        // Write meta.
        std::fs::write(
            index_dir.join("sagasu-meta.txt"),
            format!("marker_ns={marker_ns}\nroot={}\n", root.display()),
        )?;

        // Touch a.txt so its mtime > marker_ns.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(root.join("a.txt"), "common alpha MODIFIED")?;

        // Open engine and search with DeltaEveryQuery.
        let mut engine = Engine::open(&index_dir)?;
        let resp = engine.search("common", 10, &SearchMode::DeltaEveryQuery, 2 * 1024 * 1024)?;

        // "a.txt" should be dropped from index hits (it changed) and appear as a live hit.
        let index_hits: Vec<_> = resp.hits.iter().filter(|h| h.source == "index").collect();
        let live_hits: Vec<_> = resp.hits.iter().filter(|h| h.source == "live").collect();

        // a.txt was changed → dropped from index, added as live
        assert!(
            !index_hits.iter().any(|h| h.path.ends_with("a.txt")),
            "a.txt should NOT be in index hits (it was changed)"
        );
        assert!(
            live_hits.iter().any(|h| h.path.ends_with("a.txt")),
            "a.txt should be in live hits (it was changed and contains 'common')"
        );
        // b.txt was not changed → still in index hits
        assert!(
            index_hits.iter().any(|h| h.path.ends_with("b.txt")),
            "b.txt should be in index hits (unchanged)"
        );

        Ok(())
    }

    #[test]
    fn test_merge_drops_deleted_paths() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("root");
        let index_dir = tmp.path().join("ft-index");

        std::fs::create_dir_all(&root)?;
        let file_path = root.join("keep.txt");
        let ghost_path = root.join("ghost.txt");
        // Both files contain "shared" so a single query can find both.
        std::fs::write(&file_path, "shared data here")?;
        std::fs::write(&ghost_path, "shared data there")?;

        let schema = build_schema();
        std::fs::create_dir_all(&index_dir)?;
        let index = Index::create_in_dir(&index_dir, schema.clone())?;
        register_ja_tokenizer(&index)?;
        let marker_ns = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_nanos() as i64;
        {
            let mut w = index.writer(50_000_000)?;
            let path_f = schema.get_field("path")?;
            let mtime_f = schema.get_field("mtime_ns")?;
            let body_f = schema.get_field("body")?;
            w.add_document(doc!(
                path_f => file_path.to_string_lossy().as_ref(),
                mtime_f => marker_ns,
                body_f => "shared data here",
            ))?;
            w.add_document(doc!(
                path_f => ghost_path.to_string_lossy().as_ref(),
                mtime_f => marker_ns,
                body_f => "shared data there",
            ))?;
            w.commit()?;
        }

        // Write meta.
        std::fs::write(
            index_dir.join("sagasu-meta.txt"),
            format!("marker_ns={marker_ns}\nroot={}\n", root.display()),
        )?;

        // Delete ghost.txt.
        std::fs::remove_file(&ghost_path)?;

        let mut engine = Engine::open(&index_dir)?;
        let resp = engine.search("shared", 10, &SearchMode::IndexOnly, 2 * 1024 * 1024)?;

        // ghost.txt was deleted → should be dropped from index results.
        let hits: Vec<_> = resp.hits.iter().filter(|h| h.source == "index").collect();
        assert!(
            !hits.iter().any(|h| h.path.ends_with("ghost.txt")),
            "ghost.txt should be dropped (deleted)"
        );
        assert!(
            hits.iter().any(|h| h.path.ends_with("keep.txt")),
            "keep.txt should still be in index hits"
        );

        Ok(())
    }

    #[test]
    fn test_total_hits_before_limit_is_pre_limit() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("root");
        let index_dir = tmp.path().join("ft-index");
        std::fs::create_dir_all(&root)?;

        let schema = build_schema();
        std::fs::create_dir_all(&index_dir)?;
        let index = Index::create_in_dir(&index_dir, schema.clone())?;
        register_ja_tokenizer(&index)?;
        let marker_ns = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_nanos() as i64;
        {
            let mut w = index.writer(50_000_000)?;
            let path_f = schema.get_field("path")?;
            let mtime_f = schema.get_field("mtime_ns")?;
            let body_f = schema.get_field("body")?;
            for i in 0..5 {
                let p = root.join(format!("f{i}.txt"));
                std::fs::write(&p, "needle content")?;
                w.add_document(doc!(
                    path_f => p.to_string_lossy().as_ref(),
                    mtime_f => marker_ns,
                    body_f => "needle content",
                ))?;
            }
            w.commit()?;
        }
        std::fs::write(
            index_dir.join("sagasu-meta.txt"),
            format!("marker_ns={marker_ns}\nroot={}\n", root.display()),
        )?;

        let mut engine = Engine::open(&index_dir)?;
        // 5 matching docs, limit 2: hits are capped but the pre-limit count is not.
        let resp = engine.search("needle", 2, &SearchMode::IndexOnly, 2 * 1024 * 1024)?;
        assert_eq!(resp.hits.len(), 2);
        assert_eq!(resp.total_hits_before_limit, 5);
        Ok(())
    }

    // --- DeltaCached tests ---

    #[test]
    fn test_delta_cached_reuses_set_within_ttl() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("root");
        let index_dir = tmp.path().join("ft-index");

        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("x.txt"), "zig zag")?;

        let schema = build_schema();
        std::fs::create_dir_all(&index_dir)?;
        let index = Index::create_in_dir(&index_dir, schema.clone())?;
        register_ja_tokenizer(&index)?;
        let marker_ns = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_nanos() as i64;
        {
            let mut w = index.writer(50_000_000)?;
            let path_f = schema.get_field("path")?;
            let mtime_f = schema.get_field("mtime_ns")?;
            let body_f = schema.get_field("body")?;
            w.add_document(tantivy::doc!(
                path_f => root.join("x.txt").to_string_lossy().as_ref(),
                mtime_f => marker_ns,
                body_f => "zig zag",
            ))?;
            w.commit()?;
        }
        std::fs::write(
            index_dir.join("sagasu-meta.txt"),
            format!("marker_ns={marker_ns}\nroot={}\n", root.display()),
        )?;

        let mut engine = Engine::open(&index_dir)?;

        // First query: cache_miss (no cache yet), does a walk.
        let resp1 = engine.search("zig", 10, &SearchMode::DeltaCached { ttl_ms: 60_000 }, 2 * 1024 * 1024)?;
        assert!(!resp1.delta_cache_hit, "first query should be cache miss");

        // Second query: should hit the cache (TTL 60s, well within).
        let resp2 = engine.search("zig", 10, &SearchMode::DeltaCached { ttl_ms: 60_000 }, 2 * 1024 * 1024)?;
        assert!(resp2.delta_cache_hit, "second query within TTL should be cache hit");
        assert!(resp2.timings.delta_walk_ms == 0.0, "delta_walk_ms should be 0 on cache hit");
        assert!(resp2.delta_cache_age_ms > 0.0, "cache_age_ms should be positive");

        Ok(())
    }

    #[test]
    fn test_delta_cached_rewalks_after_expiry() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("root");
        let index_dir = tmp.path().join("ft-index");

        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("y.txt"), "foo bar")?;

        let schema = build_schema();
        std::fs::create_dir_all(&index_dir)?;
        let index = Index::create_in_dir(&index_dir, schema.clone())?;
        register_ja_tokenizer(&index)?;
        let marker_ns = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_nanos() as i64;
        {
            let mut w = index.writer(50_000_000)?;
            let path_f = schema.get_field("path")?;
            let mtime_f = schema.get_field("mtime_ns")?;
            let body_f = schema.get_field("body")?;
            w.add_document(tantivy::doc!(
                path_f => root.join("y.txt").to_string_lossy().as_ref(),
                mtime_f => marker_ns,
                body_f => "foo bar",
            ))?;
            w.commit()?;
        }
        std::fs::write(
            index_dir.join("sagasu-meta.txt"),
            format!("marker_ns={marker_ns}\nroot={}\n", root.display()),
        )?;

        let mut engine = Engine::open(&index_dir)?;

        // First query: populates cache.
        let _resp1 = engine.search("foo", 10, &SearchMode::DeltaCached { ttl_ms: 1 }, 2 * 1024 * 1024)?;
        // Sleep past the 1ms TTL.
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Second query: TTL expired, should re-walk (cache miss).
        let resp2 = engine.search("foo", 10, &SearchMode::DeltaCached { ttl_ms: 1 }, 2 * 1024 * 1024)?;
        assert!(!resp2.delta_cache_hit, "second query after TTL expiry should be cache miss");
        assert!(resp2.timings.delta_walk_ms > 0.0, "delta_walk_ms should be >0 on cache miss");

        Ok(())
    }

    #[test]
    fn test_refresh_delta_invalidates_cache() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("root");
        let index_dir = tmp.path().join("ft-index");

        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("z.txt"), "alpha beta")?;

        let schema = build_schema();
        std::fs::create_dir_all(&index_dir)?;
        let index = Index::create_in_dir(&index_dir, schema.clone())?;
        register_ja_tokenizer(&index)?;
        let marker_ns = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_nanos() as i64;
        {
            let mut w = index.writer(50_000_000)?;
            let path_f = schema.get_field("path")?;
            let mtime_f = schema.get_field("mtime_ns")?;
            let body_f = schema.get_field("body")?;
            w.add_document(tantivy::doc!(
                path_f => root.join("z.txt").to_string_lossy().as_ref(),
                mtime_f => marker_ns,
                body_f => "alpha beta",
            ))?;
            w.commit()?;
        }
        std::fs::write(
            index_dir.join("sagasu-meta.txt"),
            format!("marker_ns={marker_ns}\nroot={}\n", root.display()),
        )?;

        let mut engine = Engine::open(&index_dir)?;

        // Populate cache.
        let _resp1 = engine.search("alpha", 10, &SearchMode::DeltaCached { ttl_ms: 60_000 }, 2 * 1024 * 1024)?;

        // Refresh delta explicitly.
        engine.refresh_delta();

        // Next query should re-walk (cache invalidated by refresh).
        let resp2 = engine.search("alpha", 10, &SearchMode::DeltaCached { ttl_ms: 60_000 }, 2 * 1024 * 1024)?;
        assert!(!resp2.delta_cache_hit, "cache should be invalidated after refresh_delta");

        Ok(())
    }

    // --- Serde round-trip tests ---

    #[test]
    fn test_search_response_serde() {
        let resp = SearchResponse {
            hits: vec![Hit {
                source: "index".into(),
                path: "/test/file.txt".into(),
                score_or_count: 1.23,
            }],
            total_hits_before_limit: 5,
            timings: TimingData {
                search_ms: 30.0,
                delta_walk_ms: 7.0,
                live_grep_ms: 2.0,
                merge_ms: 1.0,
                total_ms: 40.0,
                ipc_ms: 1.5,
                render_ms: 3.0,
            },
            delta_changed_count: 3,
            delta_cache_hit: false,
            delta_cache_age_ms: 0.0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: SearchResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.hits.len(), 1);
        assert_eq!(back.hits[0].source, "index");
        assert!((back.timings.search_ms - 30.0).abs() < 0.001);
    }

    #[test]
    fn test_jsonl_record_serde() {
        let rec = JsonlRecord {
            timestamp_unix_s: 1234567890.0,
            query: "test".into(),
            mode: "DeltaEveryQuery".into(),
            limit: 10,
            timings: TimingData {
                search_ms: 10.0,
                delta_walk_ms: 5.0,
                live_grep_ms: 2.0,
                merge_ms: 1.0,
                total_ms: 18.0,
                ipc_ms: 0.5,
                render_ms: 2.0,
            },
            delta_changed_count: 1,
            delta_cache_hit: false,
            delta_cache_age_ms: 0.0,
            total_hits: 3,
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"query\":\"test\""));
        assert!(json.contains("\"ipc_ms\":0.5"));
        let back: JsonlRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.query, "test");
        assert!((back.timings.render_ms - 2.0).abs() < 0.001);
    }

    #[test]
    fn test_search_mode_serde() {
        let m = SearchMode::DeltaCached { ttl_ms: 5000 };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("DeltaCached"));
        assert!(json.contains("5000"));
        let back: SearchMode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, SearchMode::DeltaCached { ttl_ms: 5000 });

        let m2 = SearchMode::IndexOnly;
        let json2 = serde_json::to_string(&m2).unwrap();
        let back2: SearchMode = serde_json::from_str(&json2).unwrap();
        assert_eq!(back2, SearchMode::IndexOnly);

        let m3 = SearchMode::DeltaEveryQuery;
        let json3 = serde_json::to_string(&m3).unwrap();
        let back3: SearchMode = serde_json::from_str(&json3).unwrap();
        assert_eq!(back3, SearchMode::DeltaEveryQuery);
    }
}
