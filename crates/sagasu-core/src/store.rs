//! SQLite-backed file metadata index (schema v0).
//!
//! Schema invariants:
//! - `file_id` is a stable, never-reused identifier. It survives renames/moves and
//!   persists as a tombstone after deletion.
//! - `blake3` and `magic` are NULL until explicitly filled by a hash pass. Crawl
//!   never opens files, so they stay NULL after `index`.
//! - `fs_id` is a platform file-identity blob: (dev, ino) on Unix, NULL on Windows
//!   (M0). It is the primary key for rename/move detection during rescan.
//! - Deleted files become tombstones (`deleted_at` set); rows are never removed.
//! - `tags` / `file_tags` hold the machine-generated semantic layer (design.md
//!   §6). They join on `file_id`, so a rename does not disturb them.

use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

/// Number of magic bytes (start of file content) stored for file-type detection.
pub const MAGIC_LEN: usize = 512;

// ── Schema ──────────────────────────────────────────────────────────────────

/// Current schema version. Increment when the DDL changes.
///
/// | Version | Change |
/// |---|---|
/// | 1 | M0: `meta`, `files`, `access_history`. |
/// | 2 | M3: `tags`, `file_tags` (issue #4). Purely additive. |
pub const SCHEMA_VERSION: i64 = 2;

const SCHEMA_DDL: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
    file_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    path          TEXT    NOT NULL,
    ext           TEXT,
    size          INTEGER NOT NULL,
    mtime_ns      INTEGER NOT NULL,
    ctime_ns      INTEGER NOT NULL,
    magic         BLOB,
    blake3        BLOB,
    fs_id         BLOB,
    last_seen_scan INTEGER NOT NULL DEFAULT 0,
    deleted_at    INTEGER
);

CREATE INDEX IF NOT EXISTS idx_files_path ON files(path);
CREATE INDEX IF NOT EXISTS idx_files_fs_id ON files(fs_id);
CREATE INDEX IF NOT EXISTS idx_files_deleted ON files(deleted_at);
CREATE INDEX IF NOT EXISTS idx_files_size_mtime ON files(size, mtime_ns);

CREATE TABLE IF NOT EXISTS access_history (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id INTEGER NOT NULL REFERENCES files(file_id),
    ts      INTEGER NOT NULL,
    kind    TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS tags (
    tag_id    INTEGER PRIMARY KEY AUTOINCREMENT,
    namespace TEXT NOT NULL,
    value     TEXT NOT NULL,
    UNIQUE(namespace, value)
);

CREATE TABLE IF NOT EXISTS file_tags (
    file_id INTEGER NOT NULL REFERENCES files(file_id),
    tag_id  INTEGER NOT NULL REFERENCES tags(tag_id),
    sources INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (file_id, tag_id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_file_tags_tag ON file_tags(tag_id, file_id);
";

/// Read `meta.schema_version` from a connection whose schema may not exist yet.
///
/// Deliberately tolerant of a missing `meta` table — that is the brand-new
/// database case, not an error — but *not* of a `meta` table holding something
/// unparseable, which means the file is not a sagasu index (or is corrupt).
fn stored_schema_version(conn: &Connection) -> Result<Option<i64>> {
    let has_meta: bool = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='meta'",
        [],
        |row| row.get::<_, i64>(0),
    )? > 0;
    if !has_meta {
        return Ok(None);
    }
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else { return Ok(None) };
    let version = raw.parse().with_context(|| {
        format!("database has an unparseable schema_version {raw:?}; refusing to use it")
    })?;
    Ok(Some(version))
}

/// Refuse a database written by a *newer* build.
///
/// Silently operating on a schema we do not understand is the "quietly wrong
/// answer" failure this project treats as the worst kind: an unknown column
/// would simply never be read, and nothing would say so. The check is a free
/// function so it can run before the connection is wrapped in a [`Store`] — and
/// therefore before any DDL touches the file.
fn check_schema_version(stored: Option<i64>) -> Result<()> {
    let Some(from) = stored else { return Ok(()) };
    if from > SCHEMA_VERSION {
        bail!(
            "database schema version {from} is newer than this build understands \
             ({SCHEMA_VERSION}); upgrade sagasu rather than letting it read a \
             schema it does not know"
        );
    }
    Ok(())
}

// ── FileRow ─────────────────────────────────────────────────────────────────

/// A row from the `files` table.
#[derive(Debug, Clone)]
pub struct FileRow {
    pub file_id: i64,
    pub path: String,
    pub ext: Option<String>,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub magic: Option<Vec<u8>>,
    pub blake3: Option<Vec<u8>>,
    pub fs_id: Option<Vec<u8>>,
    pub last_seen_scan: i64,
    pub deleted_at: Option<i64>,
}

// ── Stats ───────────────────────────────────────────────────────────────────

/// Snapshot of index state (for `sagasu status`).
#[derive(Debug, Clone)]
pub struct IndexStats {
    pub root_path: Option<String>,
    pub schema_version: i64,
    pub scan_marker_ns: Option<i64>,
    /// The delta marker of the last crawl (design.md §5). `None` when the index
    /// predates the marker or the stored value is unparseable.
    pub delta_marker: Option<crate::delta::ScanMarker>,
    pub scan_generation: i64,
    pub live_count: i64,
    pub tombstone_count: i64,
    pub null_hash_count: i64,
    /// Directory of the full-text (tantivy) index, if one has been built.
    pub fulltext_dir: Option<String>,
    /// Number of documents in the full-text index at build time.
    pub fulltext_docs: Option<i64>,
    /// Scan generation the full-text index was built from. Compare against
    /// `scan_generation` to see whether the full-text index is behind the
    /// metadata index.
    pub fulltext_scan_generation: Option<i64>,
    /// Scan generation the tag layer was built from. `None` = `sagasu tag` has
    /// never run. Less than `scan_generation` → the tags are behind the
    /// metadata index.
    pub tag_scan_generation: Option<i64>,
    /// Live files that carried at least one tag at the end of that build.
    pub tag_files: Option<i64>,
    /// The user rule file that build used, if any.
    pub tag_rules: Option<String>,
    /// Rows currently in `file_tags`.
    pub tag_rows: i64,
    /// Distinct `namespace:value` pairs currently referenced by a live file.
    pub distinct_tags: i64,
}

// ── Store ───────────────────────────────────────────────────────────────────

/// Wraps a `rusqlite::Connection` with semantic helpers for the sagasu schema.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (or create) the index database at `path`, enabling WAL mode and
    /// ensuring the schema exists.
    ///
    /// The version gate runs **before** any DDL. Refusing a database and then
    /// having already written tables into it is not a refusal: the next build
    /// would find structures a newer sagasu did not put there. So the order is
    /// read the stored version → decide → only then create anything.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())
            .with_context(|| format!("failed to open database {:?}", path.as_ref()))?;

        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Enable foreign keys for access_history references.
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let stored = stored_schema_version(&conn)?;
        check_schema_version(stored)?;

        conn.execute_batch(SCHEMA_DDL)?;

        let store = Self { conn };
        store.migrate(stored)?;
        Ok(store)
    }

    /// Reconcile the stored `schema_version` with [`SCHEMA_VERSION`].
    ///
    /// Every DDL change so far has been **additive** (new tables and indexes,
    /// no change to an existing column), so `CREATE TABLE IF NOT EXISTS` above
    /// has already done the structural work by the time this runs; all that is
    /// left is to stamp the new version. That is only true while migrations stay
    /// additive — a version that rewrites a column needs real steps here, keyed
    /// off `from`.
    ///
    /// `from` is the version read *before* the DDL ran; the refusal of a newer
    /// schema already happened there ([`check_schema_version`]), because by this
    /// point the tables would already exist.
    fn migrate(&self, from: Option<i64>) -> Result<()> {
        let Some(from) = from else {
            // No version recorded: either a brand-new database (the DDL above
            // just created everything at the current version) or one from
            // before versioning existed. Either way the tables are current now.
            // `ensure_schema_version` stamps it at the start of the next crawl.
            return Ok(());
        };
        if from < SCHEMA_VERSION {
            self.meta_set("schema_version", &SCHEMA_VERSION.to_string())?;
        }
        Ok(())
    }

    /// Return a shared reference to the inner connection (for direct queries).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    // ── meta ────────────────────────────────────────────────────────────────

    /// Read a value from the `meta` table.
    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Write a value into the `meta` table (upsert).
    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    /// Remove a key from the `meta` table. Missing keys are a no-op.
    pub fn meta_delete(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM meta WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Ensure the schema version is set (only writes if missing).
    pub fn ensure_schema_version(&self) -> Result<()> {
        if self.meta_get("schema_version")?.is_none() {
            self.meta_set("schema_version", &SCHEMA_VERSION.to_string())?;
        }
        Ok(())
    }

    /// Read a value from the `meta` table inside a transaction.
    pub fn meta_get_tx(tx: &Transaction<'_>, key: &str) -> Result<Option<String>> {
        tx.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Write a value into the `meta` table (upsert) inside a transaction.
    pub fn meta_set_tx(tx: &Transaction<'_>, key: &str, value: &str) -> Result<()> {
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    /// Get the current scan generation counter inside a transaction,
    /// incrementing it first. Returns the *new* generation number.
    pub fn next_scan_generation_tx(tx: &Transaction<'_>) -> Result<i64> {
        let current: i64 = Self::meta_get_tx(tx, "scan_generation")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let next = current + 1;
        Self::meta_set_tx(tx, "scan_generation", &next.to_string())?;
        Ok(next)
    }

    /// Get the current scan generation counter, incrementing it first.
    /// Returns the *new* generation number.
    pub fn next_scan_generation(&self) -> Result<i64> {
        let current: i64 = self
            .meta_get("scan_generation")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let next = current + 1;
        self.meta_set("scan_generation", &next.to_string())?;
        Ok(next)
    }

    /// Get the current scan generation without incrementing.
    pub fn scan_generation(&self) -> i64 {
        self.meta_get("scan_generation")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    // ── files: bulk helpers ─────────────────────────────────────────────────

    /// Begin a transaction.
    pub fn begin_tx(&self) -> Result<Transaction<'_>> {
        self.conn.unchecked_transaction().map_err(Into::into)
    }

    /// Insert a new file row. Returns the auto-generated `file_id`.
    pub fn insert_file(&self, tx: &Transaction, entry: &FileEntry) -> Result<i64> {
        tx.execute(
            "INSERT INTO files (path, ext, size, mtime_ns, ctime_ns, fs_id, last_seen_scan, deleted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
            params![
                entry.path,
                entry.ext,
                entry.size,
                entry.mtime_ns,
                entry.ctime_ns,
                entry.fs_id,
                entry.scan_gen,
            ],
        )?;
        Ok(tx.last_insert_rowid())
    }

    /// Update an existing file's metadata columns and bump `last_seen_scan`.
    /// Also nulls `blake3` and `magic` (they're stale after a content change).
    pub fn update_file_changed(
        &self,
        tx: &Transaction,
        file_id: i64,
        entry: &FileEntry,
    ) -> Result<()> {
        tx.execute(
            "UPDATE files SET path=?1, ext=?2, size=?3, mtime_ns=?4, ctime_ns=?5,
                              fs_id=?6, blake3=NULL, magic=NULL, last_seen_scan=?7,
                              deleted_at=NULL
             WHERE file_id=?8",
            params![
                entry.path,
                entry.ext,
                entry.size,
                entry.mtime_ns,
                entry.ctime_ns,
                entry.fs_id,
                entry.scan_gen,
                file_id,
            ],
        )?;
        Ok(())
    }

    /// Mark a file as seen (bump `last_seen_scan`) without changing metadata.
    pub fn touch_file(&self, tx: &Transaction, file_id: i64, scan_gen: i64) -> Result<()> {
        tx.execute(
            "UPDATE files SET last_seen_scan=?1, deleted_at=NULL WHERE file_id=?2",
            params![scan_gen, file_id],
        )?;
        Ok(())
    }

    /// Update just the path and bump scan gen (rename/move).
    pub fn update_file_renamed(
        &self,
        tx: &Transaction,
        file_id: i64,
        new_path: &str,
        scan_gen: i64,
    ) -> Result<()> {
        tx.execute(
            "UPDATE files SET path=?1, last_seen_scan=?2, deleted_at=NULL WHERE file_id=?3",
            params![new_path, scan_gen, file_id],
        )?;
        Ok(())
    }

    /// Mark a file as deleted (tombstone).
    pub fn mark_deleted(&self, tx: &Transaction, file_id: i64, scan_gen: i64) -> Result<()> {
        tx.execute(
            "UPDATE files SET deleted_at=?1 WHERE file_id=?2 AND deleted_at IS NULL",
            params![scan_gen, file_id],
        )?;
        Ok(())
    }

    // ── lookup helpers ──────────────────────────────────────────────────────

    /// Find a live file by absolute path.
    pub fn find_by_path(&self, tx: &Transaction, path: &str) -> Result<Option<FileRow>> {
        tx.query_row(
            "SELECT file_id, path, ext, size, mtime_ns, ctime_ns, magic, blake3, fs_id,
                    last_seen_scan, deleted_at
             FROM files WHERE path=?1 AND deleted_at IS NULL",
            params![path],
            row_to_file_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Find a live file by fs_id. Used for rename/move detection.
    pub fn find_by_fs_id(&self, tx: &Transaction, fs_id: &[u8]) -> Result<Option<FileRow>> {
        tx.query_row(
            "SELECT file_id, path, ext, size, mtime_ns, ctime_ns, magic, blake3, fs_id,
                    last_seen_scan, deleted_at
             FROM files WHERE fs_id=?1 AND deleted_at IS NULL",
            params![fs_id],
            row_to_file_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Find a live file by (size, mtime_ns) — must be a unique match.
    /// Returns `None` if zero or more than one match.
    pub fn find_by_size_mtime(
        &self,
        tx: &Transaction,
        size: i64,
        mtime_ns: i64,
    ) -> Result<Option<FileRow>> {
        let mut stmt = tx.prepare(
            "SELECT file_id, path, ext, size, mtime_ns, ctime_ns, magic, blake3, fs_id,
                    last_seen_scan, deleted_at
             FROM files WHERE size=?1 AND mtime_ns=?2 AND deleted_at IS NULL",
        )?;
        let rows: Vec<FileRow> = stmt
            .query_map(params![size, mtime_ns], row_to_file_row)?
            .filter_map(|r| r.ok())
            .collect();
        if rows.len() == 1 {
            Ok(Some(rows.into_iter().next().unwrap()))
        } else {
            Ok(None)
        }
    }

    /// Return all live file paths as (file_id, path). Used to detect deletions.
    pub fn get_live_paths(&self, tx: &Transaction) -> Result<Vec<(i64, String)>> {
        let mut stmt = tx.prepare("SELECT file_id, path FROM files WHERE deleted_at IS NULL")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Mark all live files that haven't been seen this scan as deleted.
    pub fn tombstone_unseen(&self, tx: &Transaction, scan_gen: i64) -> Result<u64> {
        let n = tx.execute(
            "UPDATE files SET deleted_at=?1
             WHERE deleted_at IS NULL AND last_seen_scan != ?2",
            params![scan_gen, scan_gen],
        )?;
        Ok(n as u64)
    }

    /// Look up any file (live or tombstoned) by its stable `file_id`.
    ///
    /// This is the SQLite side of the metadata ↔ full-text ID link: a tantivy
    /// document carries `file_id`, and a search resolves it back through here to
    /// get the *current* path and liveness.
    pub fn find_by_file_id(&self, file_id: i64) -> Result<Option<FileRow>> {
        self.conn
            .query_row(
                "SELECT file_id, path, ext, size, mtime_ns, ctime_ns, magic, blake3, fs_id,
                        last_seen_scan, deleted_at
                 FROM files WHERE file_id=?1",
                params![file_id],
                row_to_file_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Return up to `limit` live files with `file_id > after_id`, ordered by
    /// `file_id`. Cursor-paged so the full-text builder can stream a large index
    /// without materialising every row (and every 512-byte `magic` blob) at once.
    pub fn get_live_files_batch(&self, after_id: i64, limit: i64) -> Result<Vec<FileRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT file_id, path, ext, size, mtime_ns, ctime_ns, magic, blake3, fs_id,
                    last_seen_scan, deleted_at
             FROM files
             WHERE deleted_at IS NULL AND file_id > ?1
             ORDER BY file_id
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![after_id, limit], row_to_file_row)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Live rows whose path contains `needle`, case-insensitively.
    ///
    /// This is the metadata half of the search surface: `sagasu find` answers
    /// from here and the freshness merge overlays the delta set on top. Ordered
    /// by `file_id` so the result is stable between runs.
    ///
    /// `%` and `_` in the needle are escaped — a user typing a literal `%` is
    /// searching for a percent sign, not writing SQL.
    pub fn find_paths_like(&self, needle: &str, limit: i64) -> Result<Vec<FileRow>> {
        let pattern = format!("%{}%", escape_like(needle));
        let mut stmt = self.conn.prepare(
            "SELECT file_id, path, ext, size, mtime_ns, ctime_ns, magic, blake3, fs_id,
                    last_seen_scan, deleted_at
             FROM files
             WHERE deleted_at IS NULL AND path LIKE ?1 ESCAPE '\\'
             ORDER BY file_id
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![pattern, limit], row_to_file_row)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    // ── freshness marker (design.md §5) ─────────────────────────────────────

    /// The delta marker recorded by the last crawl, if it can still be parsed.
    ///
    /// `None` means the index predates the marker (or the value is corrupt);
    /// either way a search has no point in time to ask a delta source about and
    /// has to report the whole index as of unknown freshness.
    pub fn delta_marker(&self) -> Result<Option<crate::delta::ScanMarker>> {
        Ok(self
            .meta_get("delta_marker")?
            .as_deref()
            .and_then(crate::delta::ScanMarker::decode))
    }

    /// Record a delta marker (used by the crawl; exposed for tests and tools
    /// that need to simulate an expired or foreign marker).
    pub fn set_delta_marker(&self, marker: &crate::delta::ScanMarker) -> Result<()> {
        self.meta_set("delta_marker", &marker.encode())
    }

    // ── hash backfill ───────────────────────────────────────────────────────

    /// Return up to `limit` live files where `blake3` IS NULL and
    /// `file_id > after_id`, ordered by file_id.  The cursor (rather than a
    /// bare `LIMIT`) keeps the backfill loop from re-reading files it decided
    /// to skip (too large / unreadable), which stay NULL.
    pub fn get_unhashed_files_batch(&self, after_id: i64, limit: i64) -> Result<Vec<FileRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT file_id, path, ext, size, mtime_ns, ctime_ns, magic, blake3, fs_id,
                    last_seen_scan, deleted_at
             FROM files
             WHERE blake3 IS NULL AND deleted_at IS NULL AND file_id > ?1
             ORDER BY file_id
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![after_id, limit], row_to_file_row)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Fill in `magic` alone, leaving `blake3` untouched.
    ///
    /// The tag engine needs the leading bytes to identify a format, but not the
    /// content hash — reading 512 bytes is orders of magnitude cheaper than
    /// hashing the file, so `sagasu tag --read-magic` backfills just this column
    /// rather than forcing a full `sagasu hash` pass first. `blake3 IS NULL`
    /// still means "not hashed", so a later `sagasu hash` picks the file up
    /// exactly as before.
    pub fn update_magic(&self, file_id: i64, magic: &[u8]) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET magic=?1 WHERE file_id=?2",
            params![magic, file_id],
        )?;
        Ok(())
    }

    /// Update the hash columns for a file.
    pub fn update_hash(&self, file_id: i64, blake3_hash: &[u8], magic: &[u8]) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET blake3=?1, magic=?2 WHERE file_id=?3",
            params![blake3_hash, magic, file_id],
        )?;
        Ok(())
    }

    // ── access_history ──────────────────────────────────────────────────────

    /// Append an access-history entry.
    pub fn record_access(&self, file_id: i64, ts_ns: i64, kind: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO access_history (file_id, ts, kind) VALUES (?1, ?2, ?3)",
            params![file_id, ts_ns, kind],
        )?;
        Ok(())
    }

    // ── stats ───────────────────────────────────────────────────────────────

    /// Collect index-wide statistics.
    pub fn get_stats(&self) -> Result<IndexStats> {
        let root_path = self.meta_get("root_path")?;
        let schema_version: i64 = self
            .meta_get("schema_version")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let scan_marker_ns: Option<i64> =
            self.meta_get("scan_marker")?.and_then(|s| s.parse().ok());
        let scan_generation: i64 = self
            .meta_get("scan_generation")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let live_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM files WHERE deleted_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        let tombstone_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM files WHERE deleted_at IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        let null_hash_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM files WHERE blake3 IS NULL AND deleted_at IS NULL",
            [],
            |row| row.get(0),
        )?;

        let tag_rows: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM file_tags", [], |row| row.get(0))?;
        let distinct_tags: i64 =
            self.conn
                .query_row("SELECT COUNT(DISTINCT tag_id) FROM file_tags", [], |row| {
                    row.get(0)
                })?;

        Ok(IndexStats {
            root_path,
            schema_version,
            scan_marker_ns,
            delta_marker: self.delta_marker()?,
            scan_generation,
            live_count,
            tombstone_count,
            null_hash_count,
            fulltext_dir: self.meta_get("fulltext_dir")?,
            fulltext_docs: self.meta_get("fulltext_docs")?.and_then(|s| s.parse().ok()),
            fulltext_scan_generation: self
                .meta_get("fulltext_scan_generation")?
                .and_then(|s| s.parse().ok()),
            tag_scan_generation: self
                .meta_get("tag_scan_generation")?
                .and_then(|s| s.parse().ok()),
            tag_files: self.meta_get("tag_files")?.and_then(|s| s.parse().ok()),
            tag_rules: self.meta_get("tag_rules")?,
            tag_rows,
            distinct_tags,
        })
    }

    // ── cleanup ─────────────────────────────────────────────────────────────

    /// Checkpoint WAL into the main database file.
    pub fn wal_checkpoint(&self) -> Result<()> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }
}

// ── In-flight entry (what the walker produces) ─────────────────────────────

/// A file record produced by the walker thread and consumed by the indexer.
/// Carries the scan generation so the store can stamp `last_seen_scan`.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub ext: Option<String>,
    pub fs_id: Option<Vec<u8>>,
    pub scan_gen: i64,
}

// ── fs_id helpers ───────────────────────────────────────────────────────────

/// Build a platform file-identity blob from `std::fs::Metadata`.
///
/// Unix: 16-byte blob = `u64::to_be_bytes(dev) || u64::to_be_bytes(ino)`.
/// Windows: `None` in M0 (requires a per-file handle → M2).
#[allow(unused_variables)]
pub fn fs_id_from_metadata(meta: &std::fs::Metadata) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = meta.dev();
        let ino = meta.ino();
        let mut buf = Vec::with_capacity(16);
        buf.extend_from_slice(&dev.to_be_bytes());
        buf.extend_from_slice(&ino.to_be_bytes());
        Some(buf)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

// ── internal helpers ────────────────────────────────────────────────────────

/// Escape the LIKE wildcards `%` and `_` (and the escape character itself) so a
/// user-typed needle is matched literally.
fn escape_like(needle: &str) -> String {
    let mut out = String::with_capacity(needle.len());
    for c in needle.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub(crate) fn row_to_file_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRow> {
    Ok(FileRow {
        file_id: row.get(0)?,
        path: row.get(1)?,
        ext: row.get(2)?,
        size: row.get(3)?,
        mtime_ns: row.get(4)?,
        ctime_ns: row.get(5)?,
        magic: row.get(6)?,
        blake3: row.get(7)?,
        fs_id: row.get(8)?,
        last_seen_scan: row.get(9)?,
        deleted_at: row.get(10)?,
    })
}
