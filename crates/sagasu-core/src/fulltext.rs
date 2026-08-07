//! Full-text index (tantivy + Lindera) — pipeline stage 2 of design.md §4.
//!
//! ## Where the documents come from
//!
//! The full-text stage does **not** walk the filesystem. It reads the live rows
//! of the SQLite metadata index built by [`crate::walk::crawl`] and extracts a
//! body for each. Two things fall out of that:
//!
//! 1. **ID linkage.** Every tantivy document carries the schema-v0 `file_id`, so
//!    a hit resolves back to the metadata row that owns the tags, hashes and
//!    access history. A rename between the crawl and the search is transparent:
//!    the ID is stable, only the path moves.
//! 2. **One exclusion set, not two.** Whatever the crawler decided to index is
//!    exactly what the full-text stage sees. The default excludes are build
//!    artefacts and caches (`node_modules`, `target`, `__pycache__`, …) — *not*
//!    gitignore rules and *not* hidden/dot directories. `.github/`, `.config/`,
//!    `.vscode/` are content a user wants to find, and inheriting a VCS
//!    exclusion rule into a search engine is what made an earlier measurement
//!    lose two thirds of its corpus without saying so.
//!
//! ## What gets a body
//!
//! Plain text and code only; PDF / Office are out of scope for M1. The decision
//! lives in [`crate::text`]: an extension allowlist as a fast entrance, an
//! extension denylist to reject known-binary formats without opening them, and
//! content sniffing for everything else. Every rejection is counted with a
//! reason and reported in [`FulltextSummary`] — a file must never disappear from
//! the index silently.
//!
//! ## Rebuild semantics (M1)
//!
//! `build` recreates the index from scratch. `file_id` is indexed as a term so
//! an incremental `delete_term` + re-add path is available later, but tracking
//! which documents are stale needs per-document state that schema v0 does not
//! carry yet; that belongs with the M2 freshness work.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Instant, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, INDEXED, STORED,
    STRING,
};
use tantivy::{doc, Index, IndexWriter, TantivyDocument};

use crate::store::{FileRow, Store};
use crate::text::{self, ExtVerdict};

// ── Constants ───────────────────────────────────────────────────────────────

/// Name the Lindera segmenter is registered under in the tantivy schema.
/// It is baked into `meta.json`, so it must stay stable across versions.
pub const JA_TOKENIZER: &str = "lang_ja";

/// Default body-extraction size limit (design.md §11: "本文抽出の…サイズ上限").
pub const DEFAULT_MAX_SIZE: u64 = 2 * 1024 * 1024;

/// Default total writer memory budget. tantivy splits this across its worker
/// threads and needs ≥15 MB per thread, so it also caps the thread count.
pub const DEFAULT_HEAP_BYTES: usize = 128 * 1024 * 1024;

/// Rows pulled from SQLite per round of the build loop.
const BATCH: i64 = 4096;

/// `meta` keys describing the full-text index (documented in docs/schema_v0.md).
/// They are written together at the end of a successful build and cleared at the
/// start of one, so they always describe an index that actually exists.
const FULLTEXT_META_KEYS: &[&str] = &[
    "fulltext_dir",
    "fulltext_marker",
    "fulltext_docs",
    "fulltext_scan_generation",
];

// ── Skip reasons ────────────────────────────────────────────────────────────

/// Why a live metadata row did not get a full-text document.
///
/// These are reported per-reason in the build summary. "Indexed 35 files" with
/// no explanation of the other 60 is the failure mode this enum exists to
/// prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkipReason {
    /// Larger than the configured body-extraction limit.
    TooLarge,
    /// Zero bytes — nothing to index.
    Empty,
    /// Extension is on the known-binary denylist. Includes PDF / Office, whose
    /// body extraction is deliberately deferred past M1.
    UnsupportedExt,
    /// Content sniffing said this is not UTF-8 text (or is an encoding we
    /// cannot decode yet, e.g. UTF-16 / Shift_JIS).
    BinaryContent,
    /// Present in the index but unreadable now (permissions, or deleted between
    /// the crawl and this pass).
    Unreadable,
}

impl SkipReason {
    /// All reasons, in report order.
    pub const ALL: [SkipReason; 5] = [
        SkipReason::TooLarge,
        SkipReason::Empty,
        SkipReason::UnsupportedExt,
        SkipReason::BinaryContent,
        SkipReason::Unreadable,
    ];

    /// Stable label used in CLI output.
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::TooLarge => "too large",
            SkipReason::Empty => "empty",
            SkipReason::UnsupportedExt => "unsupported format (PDF/Office/media/binary)",
            SkipReason::BinaryContent => "binary or undecodable content",
            SkipReason::Unreadable => "unreadable",
        }
    }

    fn idx(self) -> usize {
        match self {
            SkipReason::TooLarge => 0,
            SkipReason::Empty => 1,
            SkipReason::UnsupportedExt => 2,
            SkipReason::BinaryContent => 3,
            SkipReason::Unreadable => 4,
        }
    }
}

// ── Config / Summary ────────────────────────────────────────────────────────

/// Configuration for a full-text index build.
#[derive(Debug, Clone)]
pub struct FulltextConfig {
    /// SQLite metadata index to read live files from.
    pub db_path: PathBuf,
    /// Directory the tantivy index is (re)created in.
    pub index_dir: PathBuf,
    /// Body-extraction size limit in bytes.
    pub max_size: u64,
    /// Extra extensions to treat as text (extends the built-in allowlist).
    pub extra_exts: Vec<String>,
    /// Disable content sniffing: only the extension allowlist decides.
    pub no_sniff: bool,
    /// File-reading threads (0 = auto).
    pub threads: usize,
    /// Total tantivy writer memory budget in bytes.
    pub heap_bytes: usize,
}

impl FulltextConfig {
    /// Config with the documented defaults for the given database and index dir.
    pub fn new(db_path: impl Into<PathBuf>, index_dir: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            index_dir: index_dir.into(),
            max_size: DEFAULT_MAX_SIZE,
            extra_exts: Vec::new(),
            no_sniff: false,
            threads: 0,
            heap_bytes: DEFAULT_HEAP_BYTES,
        }
    }
}

/// Summary of a full-text index build.
#[derive(Debug, Clone, Default)]
pub struct FulltextSummary {
    /// Live metadata rows considered.
    pub candidates: u64,
    /// Documents written to the index.
    pub indexed: u64,
    /// Of `indexed`, how many were accepted by the extension allowlist.
    pub accepted_by_ext: u64,
    /// Of `indexed`, how many were accepted by content sniffing. A large number
    /// here means the allowlist is missing formats worth adding.
    pub accepted_by_sniff: u64,
    /// Per-reason skip counts. Reasons with zero hits are omitted.
    pub skipped: BTreeMap<SkipReason, u64>,
    /// Total decoded body bytes fed to the indexer.
    pub text_bytes: u64,
    /// On-disk size of the index directory after commit.
    pub index_bytes: u64,
    /// Wall-clock duration of the build.
    pub elapsed_secs: f64,
}

impl FulltextSummary {
    /// Total number of skipped rows across all reasons.
    pub fn skipped_total(&self) -> u64 {
        self.skipped.values().sum()
    }
}

// ── Schema / tokenizer ──────────────────────────────────────────────────────

/// Field handles for the full-text schema.
#[derive(Debug, Clone, Copy)]
struct Fields {
    file_id: Field,
    path: Field,
    mtime_ns: Field,
    body: Field,
}

impl Fields {
    fn resolve(schema: &Schema) -> Result<Self> {
        Ok(Self {
            file_id: schema.get_field("file_id")?,
            path: schema.get_field("path")?,
            mtime_ns: schema.get_field("mtime_ns")?,
            body: schema.get_field("body")?,
        })
    }
}

/// Build the tantivy schema.
///
/// `file_id` is INDEXED (not just stored) so a future incremental pass can
/// `delete_term` a single document, and FAST so collectors can read it without
/// a store lookup.
fn build_schema() -> Schema {
    let mut b = Schema::builder();
    b.add_i64_field("file_id", INDEXED | STORED | FAST);
    b.add_text_field("path", STRING | STORED);
    b.add_i64_field("mtime_ns", STORED);
    b.add_text_field(
        "body",
        TextOptions::default().set_stored().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(JA_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        ),
    );
    b.build()
}

/// Register the Lindera (IPADIC) analyzer under [`JA_TOKENIZER`].
///
/// Must be called on every `Index` handle — both when creating and when opening
/// — because tokenizers live on the handle, not in `meta.json`.
///
/// Lindera segments; a `LowerCaser` is chained behind it so English matching is
/// case-insensitive (`BTreeMap` and `btreemap` are the same term). Japanese is
/// unaffected — it has no case — and both sides of a query go through the same
/// analyzer, which is what keeps a mixed 日本語 + English query consistent.
fn register_ja_tokenizer(index: &Index) -> Result<()> {
    use lindera::dictionary::{load_embedded_dictionary, DictionaryKind};
    use lindera::mode::Mode;
    use lindera::segmenter::Segmenter;
    use lindera_tantivy::tokenizer::LinderaTokenizer;
    use tantivy::tokenizer::{LowerCaser, TextAnalyzer};

    let dictionary = load_embedded_dictionary(DictionaryKind::IPADIC)
        .map_err(|e| anyhow!("failed to load the embedded IPADIC dictionary: {e}"))?;
    let segmenter = Segmenter::new(Mode::Normal, dictionary, None);
    let analyzer = TextAnalyzer::builder(LinderaTokenizer::from_segmenter(segmenter))
        .filter(LowerCaser)
        .build();
    index.tokenizers().register(JA_TOKENIZER, analyzer);
    Ok(())
}

// ── Build ───────────────────────────────────────────────────────────────────

/// Shared, lock-free counters for the parallel extraction workers.
#[derive(Default)]
struct Counters {
    by_ext: AtomicU64,
    by_sniff: AtomicU64,
    text_bytes: AtomicU64,
    skipped: [AtomicU64; 5],
}

impl Counters {
    fn skip(&self, reason: SkipReason) {
        self.skipped[reason.idx()].fetch_add(1, Ordering::Relaxed);
    }
}

/// Options the per-file extraction needs (a trimmed-down [`FulltextConfig`]).
struct ExtractOpts {
    max_size: u64,
    extra_exts: Vec<String>,
    no_sniff: bool,
}

/// Build (or rebuild) the full-text index from the live files of the metadata
/// index at `config.db_path`.
///
/// # Errors
///
/// Fails if the database or index directory cannot be opened, or if tantivy
/// rejects a write. A build that produces **zero** documents is *not* an error
/// — it returns `Ok(summary)` with `indexed == 0`, and the caller is expected to
/// warn and exit non-zero rather than let a silent empty index look like
/// success.
pub fn build(config: &FulltextConfig) -> Result<FulltextSummary> {
    let store = Store::open(&config.db_path)
        .with_context(|| format!("failed to open metadata index {:?}", config.db_path))?;

    if store.meta_get("root_path")?.is_none() {
        bail!(
            "{:?} has no crawl recorded — run `sagasu index <root> --db {}` first",
            config.db_path,
            config.db_path.display()
        );
    }

    // Clear the recorded full-text state *before* touching the directory. A
    // rebuild destroys the previous index, so if this build fails, `sagasu
    // status` must say "not built" rather than keep pointing at what is now a
    // half-written directory.
    for key in FULLTEXT_META_KEYS {
        store.meta_delete(key)?;
    }

    prepare_index_dir(&config.index_dir)?;

    let schema = build_schema();
    let index = Index::create_in_dir(&config.index_dir, schema.clone())
        .with_context(|| format!("failed to create index at {:?}", config.index_dir))?;
    register_ja_tokenizer(&index)?;
    let fields = Fields::resolve(&schema)?;

    let mut writer: IndexWriter = index
        .writer(config.heap_bytes)
        .context("failed to create the tantivy writer (memory budget too small?)")?;

    let threads = if config.threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    } else {
        config.threads
    };

    let opts = ExtractOpts {
        max_size: config.max_size,
        extra_exts: config.extra_exts.clone(),
        no_sniff: config.no_sniff,
    };
    let counters = Counters::default();
    // First fatal error from any worker. The worker abandons its chunk; the
    // build aborts at the batch boundary so the error is not lost.
    let failure: Mutex<Option<anyhow::Error>> = Mutex::new(None);

    let marker_ns = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let t0 = Instant::now();

    let mut candidates: u64 = 0;
    let mut cursor: i64 = 0;

    loop {
        let rows = store.get_live_files_batch(cursor, BATCH)?;
        let Some(last) = rows.last() else {
            break;
        };
        cursor = last.file_id;
        candidates += rows.len() as u64;

        // Reading files dominates this stage, so fan the batch out over threads
        // and let each worker call `add_document` directly (tantivy's writer
        // takes `&self` and is safe to share).
        let chunk_len = rows.len().div_ceil(threads.max(1)).max(1);
        std::thread::scope(|s| {
            for chunk in rows.chunks(chunk_len) {
                let (writer, fields, opts, counters, failure) =
                    (&writer, &fields, &opts, &counters, &failure);
                s.spawn(move || {
                    for row in chunk {
                        if let Err(e) = extract_and_add(row, fields, writer, opts, counters) {
                            let mut slot = failure.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(e);
                            }
                            return;
                        }
                    }
                });
            }
        });

        if let Some(e) = failure.lock().unwrap().take() {
            return Err(e);
        }
    }

    writer
        .commit()
        .context("failed to commit the full-text index")?;

    let elapsed_secs = t0.elapsed().as_secs_f64();
    let index_bytes = dir_size(&config.index_dir);
    let indexed =
        counters.by_ext.load(Ordering::Relaxed) + counters.by_sniff.load(Ordering::Relaxed);

    // Record the link back to the metadata index so `sagasu status` can show
    // whether the full-text index has fallen behind the crawl.
    let index_dir_canon = config
        .index_dir
        .canonicalize()
        .unwrap_or_else(|_| config.index_dir.clone());
    store.meta_set("fulltext_dir", &index_dir_canon.to_string_lossy())?;
    store.meta_set("fulltext_marker", &marker_ns.to_string())?;
    store.meta_set("fulltext_docs", &indexed.to_string())?;
    store.meta_set(
        "fulltext_scan_generation",
        &store.scan_generation().to_string(),
    )?;
    store.wal_checkpoint()?;

    let mut skipped = BTreeMap::new();
    for reason in SkipReason::ALL {
        let n = counters.skipped[reason.idx()].load(Ordering::Relaxed);
        if n > 0 {
            skipped.insert(reason, n);
        }
    }

    Ok(FulltextSummary {
        candidates,
        indexed,
        accepted_by_ext: counters.by_ext.load(Ordering::Relaxed),
        accepted_by_sniff: counters.by_sniff.load(Ordering::Relaxed),
        skipped,
        text_bytes: counters.text_bytes.load(Ordering::Relaxed),
        index_bytes,
        elapsed_secs,
    })
}

/// Decide whether one metadata row gets a document, and add it if so.
///
/// Returns `Err` only for failures that should abort the whole build (tantivy
/// write errors). Per-file problems are recorded as skip reasons.
fn extract_and_add(
    row: &FileRow,
    fields: &Fields,
    writer: &IndexWriter,
    opts: &ExtractOpts,
    counters: &Counters,
) -> Result<()> {
    if row.size == 0 {
        counters.skip(SkipReason::Empty);
        return Ok(());
    }
    if row.size as u64 > opts.max_size {
        counters.skip(SkipReason::TooLarge);
        return Ok(());
    }

    let (bytes, by_ext) = match text::classify_ext(row.ext.as_deref(), &opts.extra_exts) {
        ExtVerdict::Binary => {
            counters.skip(SkipReason::UnsupportedExt);
            return Ok(());
        }
        ExtVerdict::Text => match std::fs::read(&row.path) {
            Ok(b) => (b, true),
            Err(_) => {
                counters.skip(SkipReason::Unreadable);
                return Ok(());
            }
        },
        ExtVerdict::Unknown => {
            if opts.no_sniff {
                counters.skip(SkipReason::UnsupportedExt);
                return Ok(());
            }
            // Prefer the `magic` bytes already captured by `sagasu hash`: when
            // they are present a binary file is rejected without any I/O.
            let sample = match row.magic.as_deref() {
                Some(m) if !m.is_empty() => m.to_vec(),
                _ => match read_head(&row.path, text::SNIFF_LEN) {
                    Ok(s) => s,
                    Err(_) => {
                        counters.skip(SkipReason::Unreadable);
                        return Ok(());
                    }
                },
            };
            if !text::sniff_is_text(&sample) {
                counters.skip(SkipReason::BinaryContent);
                return Ok(());
            }
            match std::fs::read(&row.path) {
                Ok(b) => (b, false),
                Err(_) => {
                    counters.skip(SkipReason::Unreadable);
                    return Ok(());
                }
            }
        }
    };

    let body = text::decode(&bytes);
    if body.trim().is_empty() {
        counters.skip(SkipReason::Empty);
        return Ok(());
    }

    counters
        .text_bytes
        .fetch_add(body.len() as u64, Ordering::Relaxed);

    writer
        .add_document(doc!(
            fields.file_id => row.file_id,
            fields.path    => row.path.clone(),
            fields.mtime_ns => row.mtime_ns,
            fields.body    => body,
        ))
        .with_context(|| format!("failed to add {} to the full-text index", row.path))?;

    if by_ext {
        counters.by_ext.fetch_add(1, Ordering::Relaxed);
    } else {
        counters.by_sniff.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

// ── Search ──────────────────────────────────────────────────────────────────

/// Configuration for a full-text search.
#[derive(Debug, Clone)]
pub struct SearchConfig {
    /// Directory of the tantivy index.
    pub index_dir: PathBuf,
    /// Optional metadata index. When given, each hit's `file_id` is resolved
    /// back to SQLite so a rename since the build shows the current path and a
    /// deletion is flagged instead of silently returning a dead path.
    pub db_path: Option<PathBuf>,
    /// Query string (tantivy query syntax over the body field).
    pub query: String,
    /// Maximum number of hits.
    pub limit: usize,
    /// Maximum snippet length in characters.
    pub snippet_chars: usize,
}

impl SearchConfig {
    /// Config with the documented defaults for the given index dir and query.
    pub fn new(index_dir: impl Into<PathBuf>, query: impl Into<String>) -> Self {
        Self {
            index_dir: index_dir.into(),
            db_path: None,
            query: query.into(),
            limit: 10,
            snippet_chars: 160,
        }
    }
}

/// One search result.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// BM25 score (results are returned in descending score order).
    pub score: f32,
    /// Stable schema-v0 file ID.
    pub file_id: i64,
    /// Path recorded when the document was indexed.
    pub indexed_path: String,
    /// Current path from SQLite, when it differs from `indexed_path`.
    /// `None` means "unchanged" or "not resolved" (no `--db` given).
    pub current_path: Option<String>,
    /// True when SQLite says this `file_id` is a tombstone.
    pub deleted: bool,
    /// Modification time recorded when the document was indexed (unix ns).
    pub mtime_ns: i64,
    /// One-line excerpt of the body around the first matched term.
    pub snippet: String,
}

impl SearchHit {
    /// The path to show the user: the current one when known, else the indexed one.
    pub fn display_path(&self) -> &str {
        self.current_path.as_deref().unwrap_or(&self.indexed_path)
    }
}

/// Result of a full-text search.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub hits: Vec<SearchHit>,
    /// The query's analyzed terms, split by boolean polarity. The freshness
    /// merge ([`crate::fresh`]) applies these to files the index has not seen
    /// yet, so both sides of a merged result answer the same question.
    pub terms: LiveTerms,
    /// Documents present in the index.
    pub total_docs: u64,
    /// Matching + ranking latency in milliseconds. This is the number that
    /// scales with index size.
    pub match_ms: f64,
    /// Total latency in milliseconds: matching plus reading each hit's stored
    /// body, building its snippet and resolving its `file_id` in SQLite. That
    /// part scales with `limit` and document size rather than with the index —
    /// worth keeping separate so a slow query and a fat result set do not look
    /// alike.
    pub elapsed_ms: f64,
}

/// Search the full-text index.
///
/// The query goes through tantivy's query parser over the `body` field, so
/// `AND` / `OR` / `"phrase"` / `-negation` all work. Japanese text is segmented
/// by Lindera on both sides, which is what makes a mixed 日本語 + English query
/// behave.
pub fn search(config: &SearchConfig) -> Result<SearchOutcome> {
    if config.query.trim().is_empty() {
        bail!("empty query");
    }

    let index = Index::open_in_dir(&config.index_dir).with_context(|| {
        format!(
            "no full-text index at {:?} — run `sagasu fulltext` first",
            config.index_dir
        )
    })?;
    register_ja_tokenizer(&index)?;
    let schema = index.schema();
    let fields = Fields::resolve(&schema)?;

    let reader = index.reader()?;
    let searcher = reader.searcher();
    let total_docs = searcher.num_docs();

    // Resolving hits back to SQLite is what proves the ID link; it is optional
    // so `search` still works against a bare index directory. Opened before the
    // timer so the measurement covers querying, not setup.
    let store = match &config.db_path {
        Some(p) => {
            Some(Store::open(p).with_context(|| format!("failed to open metadata index {p:?}"))?)
        }
        None => None,
    };

    let parser = QueryParser::for_index(&index, vec![fields.body]);
    let query = parser
        .parse_query(&config.query)
        .with_context(|| format!("could not parse query {:?}", config.query))?;

    let t0 = Instant::now();
    let top = searcher.search(&query, &TopDocs::with_limit(config.limit.max(1)))?;
    let match_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let live_terms = split_query_terms(&*query, fields.body);
    let terms = body_query_terms(&live_terms);

    let mut hits = Vec::with_capacity(top.len());
    for (score, addr) in top {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let file_id = doc
            .get_first(fields.file_id)
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        let indexed_path = doc
            .get_first(fields.path)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let mtime_ns = doc
            .get_first(fields.mtime_ns)
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        let snippet = doc
            .get_first(fields.body)
            .and_then(|v| v.as_str())
            .map(|body| build_snippet(body, &terms, config.snippet_chars))
            .unwrap_or_default();

        let (current_path, deleted) = match (&store, file_id) {
            (Some(store), id) if id >= 0 => match store.find_by_file_id(id)? {
                Some(row) => {
                    let moved = (row.path != indexed_path).then_some(row.path);
                    (moved, row.deleted_at.is_some())
                }
                None => (None, false),
            },
            _ => (None, false),
        };

        hits.push(SearchHit {
            score,
            file_id,
            indexed_path,
            current_path,
            deleted,
            mtime_ns,
            snippet,
        });
    }

    Ok(SearchOutcome {
        hits,
        terms: live_terms,
        total_docs,
        match_ms,
        elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
    })
}

// ── internal helpers ────────────────────────────────────────────────────────

/// Empty the index directory for a full rebuild, refusing to touch a directory
/// that does not look like a tantivy index (a mistyped `--index-dir` should not
/// delete someone's documents).
fn prepare_index_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        if !dir.is_dir() {
            bail!("{:?} exists and is not a directory", dir);
        }
        let empty = std::fs::read_dir(dir)?.next().is_none();
        if !empty && !dir.join("meta.json").exists() {
            bail!(
                "refusing to rebuild {:?}: not empty and not a tantivy index (no meta.json). \
                 Point --index-dir at a dedicated directory.",
                dir
            );
        }
        std::fs::remove_dir_all(dir)
            .with_context(|| format!("failed to clear the index directory {dir:?}"))?;
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create the index directory {dir:?}"))?;
    Ok(())
}

/// Read at most `n` leading bytes of a file.
fn read_head(path: &str, n: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; n];
    let read = f.read(&mut buf)?;
    buf.truncate(read);
    Ok(buf)
}

/// Recursive on-disk size of a directory. Best effort: unreadable entries count
/// as zero rather than failing a build that has already succeeded.
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            _ => e.metadata().map(|m| m.len()).unwrap_or(0),
        })
        .sum()
}

/// Collect the analyzed terms a query looks for in `field`.
///
/// These are post-analysis terms: Lindera-segmented and lower-cased, exactly as
/// they appear in the index, which is what makes them findable in the document
/// text below.
pub(crate) fn body_query_terms(terms: &LiveTerms) -> Vec<String> {
    let mut all: Vec<String> = terms
        .required
        .iter()
        .chain(terms.optional.iter())
        .cloned()
        .collect();
    all.sort();
    all.dedup();
    all
}

/// A parsed query reduced to analyzed terms and their boolean polarity.
///
/// This is what lets a file the index has never seen be judged by the *same*
/// query as the index hits it will be merged with. The terms are post-analysis
/// (Lindera-segmented, lower-cased), so they are exactly the strings tantivy
/// matched on.
///
/// It is an approximation of the parsed query, and knowingly so: positions are
/// dropped, which means a phrase query degrades to its terms, and proximity is
/// not enforced. The alternative — building a throwaway in-RAM tantivy index
/// over the delta set for exact semantics — costs a Lindera pass over every
/// changed file on every query, i.e. roughly a hundred times the live grep, and
/// the delta set is by design a rounding error of the corpus. The merged result
/// labels which side each hit came from, so the approximation is visible rather
/// than silent.
#[derive(Debug, Clone, Default)]
pub struct LiveTerms {
    /// Terms every match must contain (`+term`, or all terms of an AND query).
    pub required: Vec<String>,
    /// Terms a match should contain at least one of (the default OR case).
    pub optional: Vec<String>,
    /// Terms a match must not contain (`-term`).
    pub excluded: Vec<String>,
}

impl LiveTerms {
    /// True when the query analyzed to nothing matchable (an empty or
    /// stop-word-only query).
    pub fn is_empty(&self) -> bool {
        self.required.is_empty() && self.optional.is_empty()
    }

    /// Score `text` against these terms: total occurrences of the matching
    /// terms, or `0` when the document does not match at all.
    ///
    /// Matching is substring-based over an ASCII-case-folded copy. Terms arrive
    /// already lower-cased from the analyzer, and non-ASCII scripts (Japanese
    /// included) have no case to fold — the same trade-off, for the same reason,
    /// as [`build_snippet`].
    pub fn score(&self, text: &str) -> u32 {
        if self.is_empty() {
            return 0;
        }
        let hay = text.to_ascii_lowercase();
        if self.excluded.iter().any(|t| hay.contains(t.as_str())) {
            return 0;
        }
        let count = |t: &String| hay.matches(t.as_str()).count() as u32;

        let required: u32 = self.required.iter().map(count).sum();
        if !self.required.is_empty() && self.required.iter().any(|t| !hay.contains(t.as_str())) {
            return 0;
        }
        let optional: u32 = self.optional.iter().map(count).sum();
        if self.required.is_empty() && optional == 0 {
            return 0;
        }
        required + optional
    }
}

/// Split a parsed query into required / optional / excluded terms over `field`.
///
/// `BooleanQuery` is unwrapped explicitly so `MustNot` clauses land in
/// `excluded` instead of being counted as things to look for; anything else
/// (term, phrase, range, …) contributes through `query_terms`, inheriting the
/// polarity of the clause it sits under.
fn split_query_terms(query: &dyn tantivy::query::Query, field: Field) -> LiveTerms {
    use std::collections::BTreeSet;
    use tantivy::query::{BooleanQuery, Occur};

    #[derive(Default)]
    struct Buckets {
        required: BTreeSet<String>,
        optional: BTreeSet<String>,
        excluded: BTreeSet<String>,
    }

    fn walk(q: &dyn tantivy::query::Query, field: Field, occur: Occur, out: &mut Buckets) {
        if let Some(b) = q.downcast_ref::<BooleanQuery>() {
            for (sub_occur, sub) in b.clauses() {
                // A clause under a MustNot stays negated however it is nested.
                let effective = match (occur, sub_occur) {
                    (Occur::MustNot, _) => Occur::MustNot,
                    (_, o) => *o,
                };
                walk(sub.as_ref(), field, effective, out);
            }
            return;
        }
        let bucket = match occur {
            Occur::Must => &mut out.required,
            Occur::Should => &mut out.optional,
            Occur::MustNot => &mut out.excluded,
        };
        q.query_terms(&mut |term, _| {
            if term.field() == field {
                if let Some(s) = term.value().as_str() {
                    bucket.insert(s.to_string());
                }
            }
        });
    }

    let mut buckets = Buckets::default();
    // A bare query with no boolean wrapper is a single mandatory clause.
    walk(query, field, Occur::Must, &mut buckets);

    LiveTerms {
        required: buckets.required.into_iter().collect(),
        optional: buckets.optional.into_iter().collect(),
        excluded: buckets.excluded.into_iter().collect(),
    }
}

/// Build the displayed snippet: a window of the body around the first query
/// term that occurs in it, or the head of the document when none does.
///
/// We deliberately do *not* use tantivy's `SnippetGenerator` here. It clones the
/// field's analyzer for every document, and cloning a Lindera analyzer deep-copies
/// the whole IPADIC dictionary — measured at ~6 ms per hit, roughly ten times the
/// cost of the actual search. Locating the term by substring instead is an
/// approximation (it can land inside a longer word), but a snippet is a display
/// affordance, not a matching decision: the hit set is still decided by tantivy.
pub(crate) fn build_snippet(body: &str, terms: &[String], max_chars: usize) -> String {
    let text = clean_whitespace(body);
    match find_first_term(&text, terms) {
        Some(at) => snippet_around(&text, at, max_chars),
        None => truncate_chars(&text, max_chars),
    }
}

/// Byte offset of the earliest occurrence of any term, matched
/// case-insensitively for ASCII.
///
/// `to_ascii_lowercase` is used rather than `to_lowercase` on purpose: it maps
/// only A–Z, so every byte offset in the folded haystack is still valid in the
/// original. Terms arrive already lower-cased by the analyzer, and non-ASCII
/// scripts (Japanese included) have no case to fold.
fn find_first_term(text: &str, terms: &[String]) -> Option<usize> {
    if terms.is_empty() {
        return None;
    }
    let haystack = text.to_ascii_lowercase();
    terms.iter().filter_map(|t| haystack.find(t.as_str())).min()
}

/// Take a `max_chars` window of `text` positioned so the match at byte offset
/// `at` sits about a third of the way in, with ellipses where text was cut.
fn snippet_around(text: &str, at: usize, max_chars: usize) -> String {
    let lead = max_chars / 3;

    // Snap to a character boundary at or before the match.
    let mut anchor = at.min(text.len());
    while anchor > 0 && !text.is_char_boundary(anchor) {
        anchor -= 1;
    }

    // Step back `lead` characters from the anchor.
    let start = text[..anchor]
        .char_indices()
        .rev()
        .take(lead)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(anchor);

    let tail = &text[start..];
    let end = tail
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(tail.len());

    let mut out = String::with_capacity(end + 8);
    if start > 0 {
        out.push('…');
    }
    out.push_str(&tail[..end]);
    if end < tail.len() {
        out.push('…');
    }
    out
}

/// Collapse every whitespace run to a single space so a snippet stays on one
/// terminal line.
fn clean_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_space && !out.is_empty() {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// Truncate to `n` characters (not bytes), appending an ellipsis when cut.
fn truncate_chars(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((byte_idx, _)) => format!("{}…", &s[..byte_idx]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_whitespace_collapses_runs() {
        assert_eq!(clean_whitespace("  a \n\n b\t c  "), "a b c");
        assert_eq!(clean_whitespace("\n\n"), "");
    }

    #[test]
    fn truncate_chars_respects_char_boundaries() {
        assert_eq!(truncate_chars("あいうえお", 3), "あいう…");
        assert_eq!(truncate_chars("abc", 10), "abc");
    }

    #[test]
    fn find_first_term_folds_ascii_case_only() {
        let text = "use BTreeMap; 日本語の索引";
        assert_eq!(find_first_term(text, &["btreemap".into()]), Some(4));
        assert_eq!(find_first_term(text, &["索引".into()]), text.find("索引"));
        assert_eq!(find_first_term(text, &["missing".into()]), None);
        assert_eq!(find_first_term(text, &[]), None);
    }

    #[test]
    fn snippet_around_centres_the_match_and_marks_cuts() {
        let text = "あ".repeat(100) + "目印" + &"い".repeat(100);
        let at = text.find("目印").unwrap();
        let snippet = snippet_around(&text, at, 30);

        assert!(snippet.contains("目印"), "{snippet}");
        assert!(
            snippet.starts_with('…') && snippet.ends_with('…'),
            "{snippet}"
        );
        // 30 characters of body plus the two ellipses.
        assert_eq!(snippet.chars().count(), 32, "{snippet}");
    }

    #[test]
    fn snippet_around_does_not_pad_at_the_start_of_a_short_text() {
        let snippet = snippet_around("短い本文", 0, 80);
        assert_eq!(snippet, "短い本文");
    }

    #[test]
    fn build_snippet_falls_back_to_the_document_head() {
        let body = "一行目\n二行目\n三行目";
        assert_eq!(build_snippet(body, &["無関係".into()], 5), "一行目 二…");
    }

    #[test]
    fn skip_reasons_have_distinct_slots() {
        let mut seen = [false; 5];
        for r in SkipReason::ALL {
            assert!(!seen[r.idx()], "duplicate slot for {r:?}");
            seen[r.idx()] = true;
        }
        assert!(seen.iter().all(|&b| b));
    }
}
