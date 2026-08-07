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

-- Semantic layer (design.md §6, schema version 2).
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
```

## Schema versions

| Version | Change |
|---------|--------|
| 1 | M0: `meta`, `files`, `access_history`. |
| 2 | M3: `tags`, `file_tags` (issue #4). |

Every change so far has been **additive** — new tables and indexes, no change to
an existing column — so `CREATE TABLE IF NOT EXISTS` performs the migration and
`Store::open` only has to stamp the new version number. A future change that
rewrites a column needs real steps in `Store::migrate`, keyed off the stored
version.

A database whose `schema_version` is **newer** than the running build is refused,
not opened. A column the build does not know about would simply never be read,
and nothing would say so — the same class of silent wrongness the freshness
design exists to prevent.

The version is read **before** any DDL runs, so a refused database is left
byte-for-byte as it was. Creating this build's tables and *then* declining to use
the file would not be a refusal at all: the newer sagasu would come back to a
database carrying structures it never wrote.

## Key columns

| Column | Type | NULL semantics |
|--------|------|----------------|
| `file_id` | `INTEGER PK AUTOINCREMENT` | Stable, never-reused identifier. Survives rename/move; persists after deletion (tombstone). |
| `blake3` | `BLOB NULL` | `NULL` = not yet computed. Non-`NULL` = BLAKE3 hash (32 bytes). Filled only by `sagasu hash`. |
| `magic` | `BLOB NULL` | `NULL` = not yet read. Non-`NULL` = first 512 bytes of file content. Filled by `sagasu hash`, or by `sagasu tag` (which by default reads only those 512 bytes and leaves `blake3` NULL, so a later `hash` still picks the file up). |
| `fs_id` | `BLOB NULL` | Platform file identity. Unix: 16-byte `(dev_be, ino_be)`. Windows M0: `NULL`. Primary key for rename/move detection. |

## `meta` keys

| Key | Written by | Meaning |
|-----|------------|---------|
| `schema_version` | `sagasu index` | DDL version of this database. |
| `root_path` | `sagasu index` | Canonical root of the last crawl. |
| `scan_marker` | `sagasu index` | Unix ns at which the last crawl *started*. Human-facing "age of the index" for `sagasu status`. |
| `delta_marker` | `sagasu index` | The point-in-time token the search-time delta merge replays against (design.md §5). See below. |
| `scan_generation` | `sagasu index` | Monotonic crawl counter; `last_seen_scan` / `deleted_at` are stamped with it. |
| `fulltext_dir` | `sagasu fulltext` | Canonical directory of the tantivy index built from this database. |
| `fulltext_marker` | `sagasu fulltext` | Unix ns at which the full-text build started. |
| `fulltext_docs` | `sagasu fulltext` | Number of documents written in that build. |
| `fulltext_scan_generation` | `sagasu fulltext` | `scan_generation` the full-text index was built from. Less than `scan_generation` → the full-text index is behind the metadata index. |
| `tag_marker` | `sagasu tag` | Unix ns at which the tag build started. |
| `tag_scan_generation` | `sagasu tag` | `scan_generation` the tag layer was built from. Less than `scan_generation` → the tags are behind the metadata index, and a tag filter is silently missing every file added since. |
| `tag_files` | `sagasu tag` | Live files that received at least one tag. |
| `tag_rows` | `sagasu tag` | Rows written to `file_tags`. |
| `tag_rules` | `sagasu tag` | Path of the user rule file used, or empty. |
| `tag_rules_digest` | `sagasu tag` | BLAKE3 (hex) of that file's bytes, so the tags can be attributed to an exact rule set. It is recorded, not re-checked: verifying it would mean re-reading the rule file on every query. |

All seven are cleared at the start of a tag build and written together at its
end, inside the same transaction as the rows, so they never describe a build that
did not finish.

## `delta_marker` encoding

One TEXT line, pipe-separated. `|` is used rather than `:` because a Windows
volume specifier contains a colon.

```
mtime|<started_ns>
usn|<volume>|<journal_id>|<next_usn>|<maximum_size>|<recorded_ns>
```

| Field | Meaning |
|-------|---------|
| `started_ns` | Unix ns at which the crawl started. An mtime marker never expires. |
| `volume` | Volume the journal belongs to, e.g. `C:`. |
| `journal_id` | `UsnJournalID`. **Required**: a recreated journal restarts the USN number space, so comparing `next_usn` alone would silently compare against unrelated records (issue #16). |
| `next_usn` | `NextUsn` at marker time — where a delta read starts. |
| `maximum_size` | Journal `MaximumSize` in bytes. A USN is a byte offset into the journal, so `NextUsn_now - next_usn` is the bytes consumed since the marker; against `maximum_size` and `recorded_ns` that gives a remaining-lifetime estimate (`delta::estimate_lifetime`). |
| `recorded_ns` | Unix ns at which the marker was taken. Also the fallback threshold: an mtime scan can stand in for an unavailable USN journal by using this value. |

An unparseable or absent value is **not** treated as "no changes". The search
reports `RescanRequired(MarkerMissing)` and labels its answer stale — an index
whose freshness cannot be established must never look fresh.

A marker is written inside the crawl's transaction, alongside `root_path` and
`scan_generation`, so a failed crawl leaves no marker claiming a scan that did
not finish.

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

## Tag layer

`tags` holds each distinct `namespace:value` once; `file_tags` is the junction.
Both halves of a tag are lowercased before they get here, so a facet count is
never split between two spellings of the same thing.

- **`sources`** is a bitmask of the generators that produced the tag for that
  file (`ext`=1, `magic`=2, `path`=4, `name`=8, `rule`=16). A tag reachable two
  ways records both bits; the union of two sets is order-independent, so this
  column cannot make the stored result depend on evaluation order.
- **The join key is `file_id`**, so a rename or move carries the tags with the
  file, exactly as it does for full-text hits.
- **Only live files are tagged.** A tag build replaces the whole layer, which
  also drops the rows of files that became tombstones since the last one, and
  then deletes `tags` rows no live file references any more — a facet list must
  not offer buckets that hold nothing. (Keeping tags on tombstones is what the
  §9 ledger idea would want; that is deliberately not what this does yet.)
- **The build is one transaction.** Half the corpus carrying this pass's tags
  and half the last pass's would be a set of facet counts that mean nothing, and
  no later error message can undo it.

Tags are generated by `sagasu tag` from a *pure function* of the path, the crawl
root, the extension and `magic` — never from `mtime`, which changes when a file
is merely touched. See `docs/tag_rules.md` for the generators and the user rule
file format.

The facet drill-down (`sagasu browse`, `docs/browse.md`) reads these two tables
and nothing else — no new columns, no new tables. It does create one *temporary*
table per call, `temp.sagasu_browse_selection`, holding the `file_id`s of the
current selection; it lives in the connection's temp schema and never touches
the index file.

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

Crawl (`sagasu index`) never opens files, so `blake3` and `magic` are always `NULL` immediately after indexing. Content hashes are backfilled by `sagasu hash`; `magic` alone is also backfilled by `sagasu tag`, which does so by default because the `format:` tag axis is built on it.

## Database placement

Keep the SQLite database **outside the crawl tree** (`sagasu index <root> --db <outside>`). If the database is placed inside the crawled root, the walker would encounter the DB file and its WAL/SHM siblings on every scan; SQLite writes to the DB on each scan, so it would be perpetually detected as "changed". The crawler excludes the DB file and its `-wal`/`-shm` siblings from the index automatically, and the CLI prints a warning when it detects the database inside the crawl root, but placing it outside is the supported configuration.
