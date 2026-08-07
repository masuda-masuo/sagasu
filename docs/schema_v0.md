# sagasu Schema v0

## DDL

```sql
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
```

## Key columns

| Column | Type | NULL semantics |
|--------|------|----------------|
| `file_id` | `INTEGER PK AUTOINCREMENT` | Stable, never-reused identifier. Survives rename/move; persists after deletion (tombstone). |
| `blake3` | `BLOB NULL` | `NULL` = not yet computed. Non-`NULL` = BLAKE3 hash (32 bytes). Filled only by `sagasu hash`. |
| `magic` | `BLOB NULL` | `NULL` = not yet read. Non-`NULL` = first 512 bytes of file content. Filled only by `sagasu hash`. |
| `fs_id` | `BLOB NULL` | Platform file identity. Unix: 16-byte `(dev_be, ino_be)`. Windows M0: `NULL`. Primary key for rename/move detection. |

## `meta` keys

| Key | Written by | Meaning |
|-----|------------|---------|
| `schema_version` | `sagasu index` | DDL version of this database. |
| `root_path` | `sagasu index` | Canonical root of the last crawl. |
| `scan_marker` | `sagasu index` | Unix ns at which the last crawl *started* (freshness marker, design.md §5). |
| `scan_generation` | `sagasu index` | Monotonic crawl counter; `last_seen_scan` / `deleted_at` are stamped with it. |
| `fulltext_dir` | `sagasu fulltext` | Canonical directory of the tantivy index built from this database. |
| `fulltext_marker` | `sagasu fulltext` | Unix ns at which the full-text build started. |
| `fulltext_docs` | `sagasu fulltext` | Number of documents written in that build. |
| `fulltext_scan_generation` | `sagasu fulltext` | `scan_generation` the full-text index was built from. Less than `scan_generation` → the full-text index is behind the metadata index. |

## Metadata ↔ full-text link

The tantivy index stores `file_id`, `path`, `mtime_ns` and the extracted `body`.
`file_id` is the join key: it is stable across rename and move and survives
deletion as a tombstone, so a full-text hit produced by an older build still
resolves to the current path (or is reported as deleted) without rebuilding the
index. `sagasu search --db <db>` performs that resolution; without `--db` the
path recorded at build time is shown as-is.

The full-text stage reads live rows from this table rather than walking the
filesystem, so the crawl's exclusion set is the *only* exclusion set. Which
files get a body is then decided by extension allowlist → extension denylist →
content sniffing, and every rejection is reported with a reason.

## Tombstone rule

Deleted files become tombstones (`deleted_at` set to the scan generation in which they disappeared). Rows are **never** deleted.

- `deleted_at IS NULL` → live file.
- `deleted_at IS NOT NULL` → tombstone (file was present in a past scan, now absent).

## NULL semantics summary

- `blake3 IS NULL` → content hash not computed (indexed but not yet hashed, or stale after a change).
- `blake3 IS NOT NULL` → content hash is current.
- `magic IS NULL` → magic bytes not yet read.
- `fs_id IS NULL` → platform identity not available (Windows M0, or certain filesystem types).
- `deleted_at IS NULL` → file is live.
- `deleted_at IS NOT NULL` → file was deleted (tombstone).

Crawl (`sagasu index`) never opens files, so `blake3` and `magic` are always `NULL` immediately after indexing. Content hashes are backfilled by `sagasu hash`.

## Database placement

Keep the SQLite database **outside the crawl tree** (`sagasu index <root> --db <outside>`). If the database is placed inside the crawled root, the walker would encounter the DB file and its WAL/SHM siblings on every scan; SQLite writes to the DB on each scan, so it would be perpetually detected as "changed". The crawler excludes the DB file and its `-wal`/`-shm` siblings from the index automatically, and the CLI prints a warning when it detects the database inside the crawl root, but placing it outside is the supported configuration.
