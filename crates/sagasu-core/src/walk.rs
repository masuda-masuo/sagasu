//! Parallel metadata crawl + rescan diff.
//!
//! The walker uses `ignore::WalkBuilder` with `build_parallel()` to traverse the
//! filesystem. By design it only collects `stat`-derived metadata — no file is
//! opened, so `blake3` and `magic` stay NULL after `index`.
//!
//! ## Built-in exclusion
//!
//! The following directory *names* (basename only, depth ≥ 1) are excluded by
//! default: `node_modules`, `target`, `__pycache__`, `.git`, `.hg`, `.svn`,
//! `.venv`, `venv`, `.cache`, `.npm`, `.cargo/registry`.  `--no-default-excludes`
//! drops this list; `--exclude <NAME>` adds to it.
//!
//! ## Rescan diff
//!
//! On rescan the store is not blindly upserted. Instead the walker compares each
//! encountered file against the existing index and classifies it as unchanged,
//! changed, renamed, or new.  Paths present in the index but absent from the
//! filesystem become tombstones.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Instant, UNIX_EPOCH};

use anyhow::{Context, Result};
use ignore::{WalkBuilder, WalkState};

use crate::store::{self, FileEntry, Store};

// ── Default excludes ────────────────────────────────────────────────────────

/// Directory basenames excluded by default (case-insensitive on the walk).
/// Note: `.cargo` is special-cased — we only skip `.cargo/registry`, not
/// `.cargo` itself or other children.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "node_modules",
    "target",
    "__pycache__",
    ".git",
    ".hg",
    ".svn",
    ".venv",
    "venv",
    ".cache",
    ".npm",
    ".cargo", // see special-case logic below
];

/// The effective exclusion set of a crawl: the built-in list (unless dropped)
/// plus whatever the user added.
///
/// This is a type rather than a `Vec<String>` because the *same* set has to be
/// applied in two places that do not share a walker: the crawl below, and the
/// search-time delta set ([`crate::delta`]). A USN Journal read returns raw
/// volume-wide change records, so unless the very same rule is applied there,
/// the delta set fills up with build artefacts and telemetry the index never
/// contained — measured at a 94% noise ratio on a real machine (issue #16).
#[derive(Debug, Clone)]
pub struct ExcludeSet {
    names: Vec<String>,
}

impl ExcludeSet {
    /// Build the set from the built-in list plus `extra`. `no_default` drops the
    /// built-in list, leaving only `extra`.
    pub fn new(extra: &[String], no_default: bool) -> Self {
        let mut names: Vec<String> = if no_default {
            Vec::new()
        } else {
            DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect()
        };
        for e in extra {
            if !names.iter().any(|n| n.eq_ignore_ascii_case(e)) {
                names.push(e.clone());
            }
        }
        Self { names }
    }

    /// The excluded directory basenames, in effective order.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Whether a directory basename is excluded (case-insensitive).
    pub fn contains(&self, name: &str) -> bool {
        self.names.iter().any(|e| e.eq_ignore_ascii_case(name))
    }

    /// If `path` lives under an excluded directory at or below `root`, return
    /// that directory's lowercased basename (the key used in skip counts).
    ///
    /// `.cargo` is special-cased: only `.cargo/registry` is skipped, not
    /// `.cargo` itself or its other children.
    pub fn matched_dir(&self, path: &Path, root: &Path) -> Option<String> {
        for ancestor in path.ancestors().skip(1) {
            if ancestor == root {
                break;
            }
            let Some(name) = ancestor.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let hit = if name.eq_ignore_ascii_case(".cargo") {
                self.contains(name) && path.starts_with(ancestor.join("registry"))
            } else {
                self.contains(name)
            };
            if hit {
                return Some(name.to_lowercase());
            }
        }
        None
    }
}

/// Resolve a database path to its canonical (absolute) form, tolerating a file
/// that does not exist yet: the parent directory is canonicalized and the file
/// name appended.
pub fn canonical_db_path(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    match abs.parent() {
        Some(parent) => match parent.canonicalize() {
            Ok(cp) => cp.join(abs.file_name().unwrap_or_default()),
            Err(_) => abs,
        },
        None => abs,
    }
}

/// The database file and its WAL/SHM siblings, canonicalized.
///
/// These must never be indexed, and must never enter a search-time delta set
/// either: SQLite writes to them on every scan, so they would look like a
/// changed file on every single query.
pub fn db_sibling_paths(db_path: &Path) -> Vec<PathBuf> {
    let db_canon = canonical_db_path(db_path);
    vec![
        db_canon.clone(),
        PathBuf::from(format!("{}-wal", db_canon.display())),
        PathBuf::from(format!("{}-shm", db_canon.display())),
    ]
}

/// Path equality that tolerates case differences (Windows filesystems).
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
}

// ── Config / Summary ────────────────────────────────────────────────────────

/// Configuration for a metadata crawl.
#[derive(Debug, Clone)]
pub struct CrawlConfig {
    /// Root directory to crawl.
    pub root: PathBuf,
    /// Path to the SQLite database.
    pub db_path: PathBuf,
    /// Additional directory basenames to exclude (appended to built-in list).
    pub exclude: Vec<String>,
    /// If true, drop the built-in exclusion list.
    pub no_default_excludes: bool,
    /// Number of walker threads (0 = auto).
    pub threads: usize,
}

/// Summary of a crawl operation.
#[derive(Debug, Clone, Default)]
pub struct CrawlSummary {
    /// Total files examined by the walker (before filtering).
    pub scanned: u64,
    /// Files that were successfully indexed (new + unchanged + changed + renamed).
    pub indexed: u64,
    /// Per-exclusion-name skip counts (lowercased basename → count).
    pub skipped: HashMap<String, u64>,
    /// Files newly inserted.
    pub added: u64,
    /// Files whose metadata changed in place.
    pub changed: u64,
    /// Files detected as renames/moves (same fs_id or size+mtime).
    pub renamed: u64,
    /// Files that were in the index but absent on disk (tombstoned).
    pub deleted: u64,
    /// Wall-clock duration of the crawl.
    pub elapsed_secs: f64,
}

// ── Hash summary ────────────────────────────────────────────────────────────

/// Summary of a hash backfill pass.
#[derive(Debug, Clone, Default)]
pub struct HashSummary {
    /// Files successfully hashed.
    pub hashed: u64,
    /// Files skipped because they exceeded `--max-size`.
    pub skipped_too_large: u64,
    /// Files skipped because they were unreadable at hash time.
    pub skipped_unreadable: u64,
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Run a metadata crawl of `config.root` and persist results into the SQLite
/// store at `config.db_path`.
///
/// On the *first* scan every file is "added". On subsequent scans the diff
/// classifies each entry as unchanged / changed / renamed / new, and marks
/// missing files as deleted (tombstoned).
///
/// # Errors
///
/// Returns an error if the database cannot be opened or the root does not exist.
/// If zero files are indexed the function still returns `Ok(summary)` — the
/// caller is responsible for checking `summary.indexed == 0` and issuing a
/// warning / non-zero exit.
pub fn crawl(config: CrawlConfig) -> Result<CrawlSummary> {
    let root = config
        .root
        .canonicalize()
        .with_context(|| format!("root not found: {}", config.root.display()))?;

    let store = Store::open(&config.db_path)?;
    store.ensure_schema_version()?;

    // Scan marker (unix ns when the scan started).
    let scan_marker_ns = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);

    // Build the effective exclude list.
    let excludes = ExcludeSet::new(&config.exclude, config.no_default_excludes);

    // The database file (and its WAL/SHM siblings) must never be indexed. If the
    // user pointed `--db` inside the crawl root, skip those paths explicitly.
    let db_skip = db_sibling_paths(&config.db_path);

    // Point-in-time marker for the search-time delta merge (design.md §5).
    // Taken *before* the walk: anything that changes while the crawl is running
    // falls on the delta side of the next search, which is the safe direction.
    // On Windows this is a USN Journal position when one is available, and a
    // wall-clock instant otherwise; the source that produced it also knows how
    // to read a range back from it.
    let delta_marker = crate::delta::source_for(&crate::delta::DeltaConfig {
        root: root.clone(),
        excludes: excludes.clone(),
        skip_paths: db_skip.clone(),
        threads: config.threads,
    })
    .current_marker()
    .unwrap_or(crate::delta::ScanMarker::Mtime {
        started_ns: scan_marker_ns,
    });

    // ── Walk in parallel, collect into a channel ────────────────────────────
    let (tx, rx) = mpsc::channel::<FileEntry>();
    let skip_map = Arc::new(Mutex::new(HashMap::<String, u64>::new()));
    let scanned = Arc::new(AtomicU64::new(0));

    let threads = if config.threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    } else {
        config.threads
    };

    // Clone Arcs before spawning so the main thread retains its references.
    let skip_map_ref = skip_map.clone();
    let scanned_ref = scanned.clone();
    let root_ref = root.clone();
    let db_skip_ref = db_skip.clone();

    let mut builder = WalkBuilder::new(&root);
    builder
        .threads(threads)
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false);

    let walker = builder.build_parallel();
    let t0 = Instant::now();

    // ── Transaction: meta writes AND file diff commit atomically ────────────
    // Begin the transaction FIRST so that root_path / scan_marker /
    // scan_generation are only persisted when the crawl completes successfully.
    // A failed crawl rolls the whole thing back: no phantom generation bump,
    // no misleading marker age in `sagasu status`.
    let tx_db = store.begin_tx()?;
    Store::meta_set_tx(&tx_db, "root_path", &root.to_string_lossy())?;
    Store::meta_set_tx(&tx_db, "scan_marker", &scan_marker_ns.to_string())?;
    Store::meta_set_tx(&tx_db, "delta_marker", &delta_marker.encode())?;
    let scan_gen = Store::next_scan_generation_tx(&tx_db)?;

    // Capture the summary from within the scope via Arc<Mutex>.
    let result = Arc::new(Mutex::new(None::<Result<CrawlSummary>>));
    let result_ref = result.clone();

    let tx_walk = tx.clone();
    let excludes_walk = excludes.clone();

    std::thread::scope(|s| {
        s.spawn(move || {
            walker.run(|| {
                let tx = tx_walk.clone();
                let excludes = excludes_walk.clone();
                let skip_map = skip_map_ref.clone();
                let scanned = scanned_ref.clone();
                let root = root_ref.clone();
                let db_skip = db_skip_ref.clone();

                Box::new(move |entry| {
                    let Ok(entry) = entry else {
                        return WalkState::Continue;
                    };

                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        return WalkState::Continue;
                    }

                    // Never index our own database / WAL / SHM files.
                    if db_skip.iter().any(|p| same_path(p, entry.path())) {
                        return WalkState::Continue;
                    }

                    scanned.fetch_add(1, Ordering::Relaxed);

                    // Check parent directories against the exclude list.
                    if let Some(name) = excludes.matched_dir(entry.path(), &root) {
                        let mut sm = skip_map.lock().unwrap();
                        *sm.entry(name).or_insert(0) += 1;
                        return WalkState::Skip;
                    }

                    let Ok(meta) = entry.metadata() else {
                        return WalkState::Continue;
                    };

                    let size = meta.len() as i64;
                    let mtime_ns = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);
                    let ctime_ns = meta
                        .created()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);

                    let path = entry.path();
                    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase());
                    let fs_id = store::fs_id_from_metadata(&meta);

                    let _ = tx.send(FileEntry {
                        path: path.to_string_lossy().into_owned(),
                        size,
                        mtime_ns,
                        ctime_ns,
                        ext,
                        fs_id,
                        scan_gen,
                    });

                    WalkState::Continue
                })
            });
            // tx dropped → channel closes.
        });

        // Drop our copy so the channel closes when the walker thread exits.
        drop(tx);

        // ── Main thread: apply to store with rescan diff ────────────────────
        let outcome = (|| -> Result<CrawlSummary> {
            // Snapshot existing live-file mappings: path → file_id.
            let mut existing_paths: HashMap<String, i64> = HashMap::new();
            for (id, p) in store.get_live_paths(&tx_db)? {
                existing_paths.insert(p, id);
            }

            // Track which existing file_ids have been claimed this scan.
            let mut seen_ids: HashMap<i64, bool> = HashMap::new();
            for &id in existing_paths.values() {
                seen_ids.insert(id, false);
            }

            let mut added: u64 = 0;
            let mut changed: u64 = 0;
            let mut renamed: u64 = 0;
            let mut indexed: u64 = 0;

            for entry in rx {
                let existing = store.find_by_path(&tx_db, &entry.path)?;

                if let Some(row) = existing {
                    let unchanged = row.size == entry.size && row.mtime_ns == entry.mtime_ns;
                    seen_ids.insert(row.file_id, true);

                    if unchanged {
                        store.touch_file(&tx_db, row.file_id, entry.scan_gen)?;
                    } else {
                        store.update_file_changed(&tx_db, row.file_id, &entry)?;
                        changed += 1;
                    }
                } else {
                    // Try rename/move detection.
                    let matched = if let Some(ref fs_id) = entry.fs_id {
                        store.find_by_fs_id(&tx_db, fs_id)?
                    } else {
                        None
                    };

                    let matched = match matched {
                        Some(ref row) if !seen_ids.get(&row.file_id).copied().unwrap_or(true) => {
                            Some(row.clone())
                        }
                        _ => {
                            let candidate =
                                store.find_by_size_mtime(&tx_db, entry.size, entry.mtime_ns)?;
                            match candidate {
                                Some(ref row)
                                    if !seen_ids.get(&row.file_id).copied().unwrap_or(true) =>
                                {
                                    Some(row.clone())
                                }
                                _ => None,
                            }
                        }
                    };

                    if let Some(row) = matched {
                        if seen_ids.get(&row.file_id).copied().unwrap_or(false) {
                            store.insert_file(&tx_db, &entry)?;
                            added += 1;
                        } else {
                            seen_ids.insert(row.file_id, true);
                            if row.size == entry.size && row.mtime_ns == entry.mtime_ns {
                                // Pure rename/move: content unchanged.
                                store.update_file_renamed(
                                    &tx_db,
                                    row.file_id,
                                    &entry.path,
                                    entry.scan_gen,
                                )?;
                                renamed += 1;
                            } else {
                                // Renamed AND modified: update all metadata and
                                // null blake3/magic (stale content hash) so
                                // `sagasu hash` recomputes it.
                                store.update_file_changed(&tx_db, row.file_id, &entry)?;
                                changed += 1;
                            }
                        }
                    } else {
                        store.insert_file(&tx_db, &entry)?;
                        added += 1;
                    }
                }

                indexed += 1;
            }

            let deleted = store.tombstone_unseen(&tx_db, scan_gen)?;

            tx_db.commit()?;

            let elapsed_secs = t0.elapsed().as_secs_f64();
            store.wal_checkpoint()?;

            let skipped = skip_map.lock().unwrap().clone();
            let scanned_total = scanned.load(Ordering::Relaxed);

            Ok(CrawlSummary {
                scanned: scanned_total,
                indexed,
                skipped,
                added,
                changed,
                renamed,
                deleted,
                elapsed_secs,
            })
        })();

        *result_ref.lock().unwrap() = Some(outcome);
    });

    // Extract the result from the Arc before local variables are dropped.
    let mut guard = result.lock().unwrap();
    guard
        .take()
        .context("walk: internal error — summary not captured")?
}

// ── Hash backfill ───────────────────────────────────────────────────────────

/// Backfill `blake3` and `magic` for live files that have NULL hashes.
///
/// Files larger than `max_size` are skipped (they keep NULL).  The function
/// returns a summary of how many files were hashed and how many were skipped.
pub fn hash_backfill(db_path: &Path, max_size: u64) -> Result<HashSummary> {
    const BATCH: i64 = 1000;

    let store = Store::open(db_path)?;
    let mut summary = HashSummary::default();
    let mut cursor: i64 = 0;

    loop {
        let files = store.get_unhashed_files_batch(cursor, BATCH)?;
        let Some(last) = files.last() else {
            break;
        };
        cursor = last.file_id;

        for row in &files {
            if row.size as u64 > max_size {
                summary.skipped_too_large += 1;
                continue;
            }

            let contents = match std::fs::read(&row.path) {
                Ok(c) => c,
                Err(_) => {
                    summary.skipped_unreadable += 1;
                    continue;
                }
            };

            let blake3_hash = blake3::hash(&contents);
            let magic_len = store::MAGIC_LEN.min(contents.len());
            let magic = &contents[..magic_len];

            store.update_hash(row.file_id, blake3_hash.as_bytes(), magic)?;
            summary.hashed += 1;
        }
    }

    store.wal_checkpoint()?;

    Ok(summary)
}
