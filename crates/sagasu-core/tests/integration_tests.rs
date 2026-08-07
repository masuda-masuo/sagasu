//! Integration tests for sagasu-core: crawl, rescan diff, hash, schema.
//!
//! Each test creates a temporary directory and database under `std::env::temp_dir()`.
//! The temporary directories are cleaned up at the end (best-effort).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use sagasu_core::store::{Store, SCHEMA_VERSION};
use sagasu_core::walk::{self, CrawlConfig};

// ── helpers ─────────────────────────────────────────────────────────────────

/// Create a temporary working directory. Returns (data_dir, db_dir).
/// The data dir contains the files to crawl; the db dir holds the SQLite file,
/// ensuring the walker never sees the database or its WAL/SHM siblings.
fn tmp_dir(prefix: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("sagasu_test_{}_{}", prefix, std::process::id()));
    let data = base.join("data");
    let db = base.join("db");
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&db).unwrap();
    (data, db)
}

fn db_path(db_dir: &Path) -> PathBuf {
    db_dir.join("test.db")
}

fn write_file(dir: &Path, rel: &str, content: &str) -> PathBuf {
    let p = dir.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&p, content).unwrap();
    p
}

fn crawl(dir: &Path, db_dir: &Path) -> walk::CrawlSummary {
    walk::crawl(CrawlConfig {
        root: dir.to_path_buf(),
        db_path: db_path(db_dir),
        exclude: vec![],
        no_default_excludes: false,
        hidden: Default::default(),
        use_gitignore: false,
        threads: 1,
    })
    .unwrap()
}

/// Run a crawl with extra excludes.
fn crawl_exclude(
    dir: &Path,
    db_dir: &Path,
    exclude: Vec<String>,
    no_default: bool,
) -> walk::CrawlSummary {
    walk::crawl(CrawlConfig {
        root: dir.to_path_buf(),
        db_path: db_path(db_dir),
        exclude,
        no_default_excludes: no_default,
        hidden: Default::default(),
        use_gitignore: false,
        threads: 1,
    })
    .unwrap()
}

/// Open the store for inspection.
fn open_store(db_dir: &Path) -> Store {
    Store::open(db_path(db_dir)).unwrap()
}

// ── 1. Basic crawl: files are indexed, hashes stay NULL ─────────────────────

#[test]
fn basic_crawl_indexes_files_and_keeps_hashes_null() {
    let (d, db) = tmp_dir("basic");
    write_file(&d, "a.txt", "hello");
    write_file(&d, "b.txt", "world");

    let summary = crawl(&d, &db);
    assert_eq!(summary.indexed, 2);
    assert_eq!(summary.added, 2);
    assert_eq!(summary.scanned, 2);

    // Verify blake3 and magic are NULL after crawl.
    let store = open_store(&db);
    let conn = store.conn();
    let (blake3_null, magic_null): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE blake3 IS NULL AND magic IS NULL AND deleted_at IS NULL",
            [],
            |row| Ok((row.get(0)?, row.get(0)?)),
        )
        .unwrap();
    assert_eq!(blake3_null, 2);
    assert_eq!(magic_null, 2);
}

// ── 2. Rename keeps file_id via fs_id (Linux: dev+ino) ─────────────────────

#[test]
fn rename_keeps_file_id_via_fs_id() {
    let (d, db) = tmp_dir("rename");
    let f = write_file(&d, "old_name.txt", "content");

    // First crawl.
    let s1 = crawl(&d, &db);
    assert_eq!(s1.indexed, 1);
    assert_eq!(s1.added, 1);

    let store1 = open_store(&db);
    let conn1 = store1.conn();
    let old_id: i64 = conn1
        .query_row(
            "SELECT file_id FROM files WHERE path LIKE '%old_name.txt' AND deleted_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();

    // Rename the file.
    let new_path = d.join("new_name.txt");
    fs::rename(&f, &new_path).unwrap();

    // Second crawl.
    let s2 = crawl(&d, &db);
    assert_eq!(s2.indexed, 1);
    assert_eq!(s2.renamed, 1, "should detect rename via fs_id");
    assert_eq!(s2.added, 0);
    assert_eq!(s2.changed, 0);
    assert_eq!(s2.deleted, 0);

    // Same file_id, new path.
    let store2 = open_store(&db);
    let conn2 = store2.conn();
    let (new_id, path): (i64, String) = conn2
        .query_row(
            "SELECT file_id, path FROM files WHERE deleted_at IS NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(new_id, old_id, "file_id must survive rename");
    assert!(path.contains("new_name.txt"), "path should update");
}

// ── 3. Move (different parent dir) keeps file_id ────────────────────────────

#[test]
fn move_to_different_dir_keeps_file_id() {
    let (d, db) = tmp_dir("move");
    fs::create_dir_all(d.join("sub")).unwrap();
    let f = write_file(&d, "sub/movable.txt", "movable");

    let s1 = crawl(&d, &db);
    assert_eq!(s1.indexed, 1);

    let store1 = open_store(&db);
    let old_id: i64 = store1
        .conn()
        .query_row(
            "SELECT file_id FROM files WHERE path LIKE '%movable.txt' AND deleted_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();

    // Move to a different directory.
    fs::create_dir_all(d.join("other")).unwrap();
    let new_path = d.join("other/movable.txt");
    fs::rename(&f, &new_path).unwrap();

    let s2 = crawl(&d, &db);
    assert_eq!(s2.renamed, 1, "move should be detected as rename via fs_id");
    assert_eq!(s2.added, 0);

    let store2 = open_store(&db);
    let (new_id, path): (i64, String) = store2
        .conn()
        .query_row(
            "SELECT file_id, path FROM files WHERE deleted_at IS NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(new_id, old_id, "file_id must survive move");
    assert!(
        std::path::Path::new(&path).ends_with(std::path::Path::new("other").join("movable.txt"))
    );
}

// ── 4. Size+mtime fallback when fs_id is NULL ──────────────────────────────

#[test]
fn rename_match_via_size_mtime_fallback() {
    let (d, db) = tmp_dir("fallback");

    // We cannot easily make fs_id NULL on Linux, but we can test the fallback
    // directly by verifying that a file with the same size+mtime_ns is matched.
    // Write a file, crawl, then manually NULL the fs_id to simulate Windows.
    write_file(&d, "original.txt", "exact same content");

    let s1 = crawl(&d, &db);
    assert_eq!(s1.indexed, 1);
    assert_eq!(s1.added, 1);

    // Manually set fs_id to NULL to force the fallback path.
    let store = open_store(&db);
    store
        .conn()
        .execute("UPDATE files SET fs_id = NULL", [])
        .unwrap();

    // Rename without changing content/size/mtime.
    let old = d.join("original.txt");
    let new = d.join("renamed_fallback.txt");
    fs::rename(&old, &new).unwrap();

    let s2 = crawl(&d, &db);
    assert_eq!(s2.indexed, 1);
    assert_eq!(s2.renamed, 1, "should match via (size, mtime_ns) fallback");
    assert_eq!(s2.added, 0);

    let store2 = open_store(&db);
    let (path,): (String,) = store2
        .conn()
        .query_row(
            "SELECT path FROM files WHERE deleted_at IS NULL",
            [],
            |row| Ok((row.get(0)?,)),
        )
        .unwrap();
    assert!(path.contains("renamed_fallback.txt"));
}

// ── 5. Delete produces tombstone, not missing row ───────────────────────────

#[test]
fn delete_creates_tombstone_not_missing_row() {
    let (d, db) = tmp_dir("delete");
    let f = write_file(&d, "doomed.txt", "delete me");

    let s1 = crawl(&d, &db);
    assert_eq!(s1.indexed, 1);
    assert_eq!(s1.added, 1);

    let store1 = open_store(&db);
    let file_id: i64 = store1
        .conn()
        .query_row(
            "SELECT file_id FROM files WHERE path LIKE '%doomed.txt' AND deleted_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();

    // Delete the file from disk.
    fs::remove_file(&f).unwrap();

    let s2 = crawl(&d, &db);
    assert_eq!(s2.deleted, 1, "should detect deletion");
    assert_eq!(s2.indexed, 0);

    // Row still exists but with deleted_at set.
    let store2 = open_store(&db);
    let (deleted_at,): (Option<i64>,) = store2
        .conn()
        .query_row(
            "SELECT deleted_at FROM files WHERE file_id = ?1",
            [file_id],
            |row| Ok((row.get(0)?,)),
        )
        .unwrap();
    assert!(deleted_at.is_some(), "tombstone: deleted_at must be set");

    // Also verify there are no live rows.
    let live: i64 = store2
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM files WHERE deleted_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(live, 0);
}

// ── 6. Changed file keeps file_id, resets blake3 to NULL ──────────────────

#[test]
fn changed_file_keeps_id_and_resets_blake3() {
    let (d, db) = tmp_dir("change");
    let f = write_file(&d, "changing.txt", "version 1");

    // First crawl.
    let s1 = crawl(&d, &db);
    assert_eq!(s1.indexed, 1);
    assert_eq!(s1.added, 1);

    // Manually set blake3 + magic to simulate a previous hash pass.
    let store = open_store(&db);
    store
        .conn()
        .execute(
            "UPDATE files SET blake3 = X'0000000000000000000000000000000000000000000000000000000000000000',
                              magic = X'FF' WHERE deleted_at IS NULL",
            [],
        )
        .unwrap();

    let file_id: i64 = store
        .conn()
        .query_row(
            "SELECT file_id FROM files WHERE deleted_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();

    // Change the file content (and wait a tiny bit for mtime to change).
    std::thread::sleep(std::time::Duration::from_millis(10));
    fs::write(&f, "version 2").unwrap();

    // Second crawl.
    let s2 = crawl(&d, &db);
    assert_eq!(s2.indexed, 1);
    assert_eq!(s2.changed, 1, "should detect change");
    assert_eq!(s2.added, 0);

    // Same file_id, blake3 and magic reset to NULL.
    let store2 = open_store(&db);
    let (id2, blake3, magic): (i64, Option<Vec<u8>>, Option<Vec<u8>>) = store2
        .conn()
        .query_row(
            "SELECT file_id, blake3, magic FROM files WHERE deleted_at IS NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(id2, file_id, "file_id must not change");
    assert!(
        blake3.is_none(),
        "blake3 must be reset to NULL after change"
    );
    assert!(magic.is_none(), "magic must be reset to NULL after change");
}

// ── 7. Hash fills blake3; second hash run has nothing to do ────────────────

#[test]
fn hash_fills_blake3_and_second_run_is_idempotent() {
    let (d, db) = tmp_dir("hash");
    write_file(&d, "hashme.txt", "hello blake3");

    let s1 = crawl(&d, &db);
    assert_eq!(s1.indexed, 1);

    // Verify blake3 is NULL before hash.
    let store = open_store(&db);
    let before: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM files WHERE blake3 IS NULL AND deleted_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(before, 1);

    // Run hash.
    let h1 = sagasu_core::walk::hash_backfill(&db_path(&db), 4 * 1024 * 1024).unwrap();
    assert_eq!(h1.hashed, 1);
    assert_eq!(h1.skipped_too_large, 0);

    // Verify blake3 is now filled.
    let store2 = open_store(&db);
    let after: i64 = store2
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM files WHERE blake3 IS NOT NULL AND deleted_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(after, 1);

    // Also verify magic is filled.
    let magic: Option<Vec<u8>> = store2
        .conn()
        .query_row(
            "SELECT magic FROM files WHERE deleted_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(magic.is_some());
    assert!(!magic.unwrap().is_empty());

    // Second hash run: nothing to do.
    let h2 = sagasu_core::walk::hash_backfill(&db_path(&db), 4 * 1024 * 1024).unwrap();
    assert_eq!(h2.hashed, 0, "second hash run should find nothing to do");
}

// ── 8. hash --max-size skips large files ───────────────────────────────────

#[test]
fn hash_skips_large_files() {
    let (d, db) = tmp_dir("hash_skip");
    write_file(&d, "small.txt", "tiny");

    // Write a 2 KiB file.
    let big_content = vec![b'x'; 2048];
    fs::write(d.join("big.txt"), &big_content).unwrap();

    crawl(&d, &db);

    // max_size = 1024 bytes → big.txt (2048) should be skipped.
    let summary = sagasu_core::walk::hash_backfill(&db_path(&db), 1024).unwrap();
    assert_eq!(summary.hashed, 1, "small file should be hashed");
    assert_eq!(summary.skipped_too_large, 1, "big file should be skipped");

    // Verify small file has blake3, big file does not.
    let store = open_store(&db);
    let mut stmt = store
        .conn()
        .prepare("SELECT path, blake3 FROM files WHERE deleted_at IS NULL")
        .unwrap();
    let rows: Vec<(String, Option<Vec<u8>>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    for (path, blake3) in &rows {
        if path.contains("small.txt") {
            assert!(blake3.is_some(), "small file should have blake3");
        } else if path.contains("big.txt") {
            assert!(blake3.is_none(), "big file should still have NULL blake3");
        }
    }
}

// ── 9. Default excludes drop node_modules ──────────────────────────────────

#[test]
fn default_excludes_drop_node_modules() {
    let (d, db) = tmp_dir("exclude_nm");
    write_file(&d, "a.txt", "hello");
    write_file(&d, "node_modules/skip.js", "skip");

    let summary = crawl(&d, &db);
    assert_eq!(summary.indexed, 1, "only a.txt should be indexed");
    assert_eq!(summary.scanned, 2, "both files should be examined");

    let skips: HashMap<_, _> = summary.skipped;
    let nm_count = skips.get("node_modules").copied().unwrap_or(0);
    assert_eq!(
        nm_count, 1,
        "node_modules file should be counted as skipped"
    );
}

// ── 10. Default excludes keep dot-dirs like .opencode ──────────────────────

#[test]
fn default_excludes_keep_dot_dirs() {
    let (d, db) = tmp_dir("keep_dot");
    write_file(&d, ".opencode/config.json", "{}");
    write_file(&d, "visible.txt", "hi");

    let summary = crawl(&d, &db);
    assert_eq!(summary.indexed, 2, ".opencode should NOT be excluded");
    assert!(summary.skipped.is_empty(), "no files should be skipped");
}

// ── 11. .cargo/registry is excluded but .cargo itself is not ───────────────

#[test]
fn cargo_registry_excluded_but_cargo_not() {
    let (d, db) = tmp_dir("cargo");
    write_file(&d, ".cargo/config.toml", "[build]");
    write_file(&d, ".cargo/registry/cache/some-crate.tar.gz", "binary");

    let summary = crawl(&d, &db);
    // .cargo/config.toml should be indexed; .cargo/registry file should be skipped.
    assert_eq!(
        summary.indexed, 1,
        "only .cargo/config.toml should be indexed"
    );
    assert_eq!(summary.scanned, 2);

    let skips: HashMap<_, _> = summary.skipped;
    let cg_count = skips.get(".cargo").copied().unwrap_or(0);
    assert_eq!(
        cg_count, 1,
        ".cargo/registry file should be counted as skipped"
    );
}

// ── 12. --exclude adds custom excludes ─────────────────────────────────────

#[test]
fn custom_exclude_adds_to_defaults() {
    let (d, db) = tmp_dir("custom_excl");
    write_file(&d, "a.txt", "hi");
    write_file(&d, "build/output.o", "obj");

    let summary = crawl_exclude(&d, &db, vec!["build".to_string()], false);
    assert_eq!(summary.indexed, 1, "only a.txt should be indexed");
    let skips: HashMap<_, _> = summary.skipped;
    assert!(
        skips.contains_key("build"),
        "build should appear in skip reasons"
    );
}

// ── 13. --no-default-excludes drops the built-in list ──────────────────────

#[test]
fn no_default_excludes_indexes_node_modules() {
    let (d, db) = tmp_dir("no_default");
    write_file(&d, "node_modules/pkg/index.js", "js");
    write_file(&d, "src/main.rs", "rust");

    let summary = crawl_exclude(&d, &db, vec![], true);
    assert_eq!(
        summary.indexed, 2,
        "both files should be indexed with --no-default-excludes"
    );
    assert!(summary.skipped.is_empty());
}

// ── 14. Re-index with no changes reports 0 added/changed/renamed/deleted ──

#[test]
fn reindex_unchanged_reports_all_zeros() {
    let (d, db) = tmp_dir("unchanged");
    write_file(&d, "stable.txt", "unchanging");

    let s1 = crawl(&d, &db);
    assert_eq!(s1.indexed, 1);
    assert_eq!(s1.added, 1);

    // Second crawl with no changes.
    let s2 = crawl(&d, &db);
    assert_eq!(s2.indexed, 1);
    assert_eq!(s2.added, 0, "no files should be added");
    assert_eq!(s2.changed, 0, "no files should be changed");
    assert_eq!(s2.renamed, 0, "no files should be renamed");
    assert_eq!(s2.deleted, 0, "no files should be deleted");
}

// ── 15. Empty result (zero indexed) exits non-zero ────────────────────────

#[test]
fn empty_crawl_returns_zero_indexed() {
    let (d, db) = tmp_dir("empty");
    // Only create excluded directories.
    write_file(&d, "node_modules/pkg/index.js", "js");
    write_file(&d, "__pycache__/module.pyc", "pyc");

    let summary = crawl(&d, &db);
    assert_eq!(
        summary.indexed, 0,
        "only excluded files → indexed should be 0"
    );
    // The CLI is responsible for non-zero exit; the core returns the summary.
    assert!(!summary.skipped.is_empty(), "should have skip reasons");
}

// ── 16. Summary includes skip counts by reason ────────────────────────────

#[test]
fn summary_includes_skip_counts_by_reason() {
    let (d, db) = tmp_dir("skip_counts");
    write_file(&d, "nice.txt", "good");
    write_file(&d, "target/debug/some.o", "obj");
    write_file(&d, "node_modules/leftpad/index.js", "js");

    let summary = crawl(&d, &db);
    assert_eq!(summary.indexed, 1, "only nice.txt indexed");
    assert_eq!(summary.scanned, 3);

    let skips: HashMap<_, _> = summary.skipped;
    let target_count = skips.get("target").copied().unwrap_or(0);
    let nm_count = skips.get("node_modules").copied().unwrap_or(0);
    assert_eq!(target_count, 1, "target should have 1 skip");
    assert_eq!(nm_count, 1, "node_modules should have 1 skip");
}

// ── 17. Schema version is set ──────────────────────────────────────────────

#[test]
fn schema_version_is_set_after_crawl() {
    let (d, db) = tmp_dir("schema");
    write_file(&d, "f.txt", ".");

    crawl(&d, &db);
    let store = open_store(&db);
    let stats = store.get_stats().unwrap();
    assert_eq!(stats.schema_version, SCHEMA_VERSION);
}

// ── 18. scan_generation increments each time ───────────────────────────────

#[test]
fn scan_generation_increments() {
    let (d, db) = tmp_dir("scan_gen");
    write_file(&d, "f.txt", ".");

    crawl(&d, &db);
    let store1 = open_store(&db);
    let gen1 = store1.get_stats().unwrap().scan_generation;

    crawl(&d, &db);
    let store2 = open_store(&db);
    let gen2 = store2.get_stats().unwrap().scan_generation;

    assert!(gen2 > gen1, "scan generation must increment");
}

// ── 19. Index never opens files: magic/blake3 stay NULL ──────────────────

#[test]
fn index_never_opens_files() {
    let (d, db) = tmp_dir("no_open");
    write_file(&d, "secret.txt", "this content must not be hashed");

    crawl(&d, &db);

    let store = open_store(&db);
    let conn = store.conn();
    let (blake3, magic): (Option<Vec<u8>>, Option<Vec<u8>>) = conn
        .query_row(
            "SELECT blake3, magic FROM files WHERE deleted_at IS NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(
        blake3.is_none(),
        "blake3 must be NULL after index (crawl never opens files)"
    );
    assert!(
        magic.is_none(),
        "magic must be NULL after index (crawl never opens files)"
    );
}

// ── 20. access_history table exists and record_access works ───────────────

#[test]
fn access_history_recording() {
    let (d, db) = tmp_dir("access");
    write_file(&d, "f.txt", ".");

    crawl(&d, &db);

    let store = open_store(&db);
    // Get a file_id.
    let file_id: i64 = store
        .conn()
        .query_row(
            "SELECT file_id FROM files WHERE deleted_at IS NULL LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();

    let ts = 1000;
    store.record_access(file_id, ts, "open").unwrap();

    let (id, kind): (i64, String) = store
        .conn()
        .query_row(
            "SELECT file_id, kind FROM access_history WHERE file_id = ?1",
            [file_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(id, file_id);
    assert_eq!(kind, "open");
}

// ── 21. status reports correct counts ─────────────────────────────────────

#[test]
fn status_reports_correct_counts() {
    let (d, db) = tmp_dir("status");
    write_file(&d, "live.txt", "alive");
    write_file(&d, "also.txt", "here");

    crawl(&d, &db);

    let store = open_store(&db);
    let stats = store.get_stats().unwrap();

    assert_eq!(stats.live_count, 2);
    assert_eq!(stats.tombstone_count, 0);
    assert_eq!(stats.null_hash_count, 2);
    assert!(stats.root_path.is_some());
    assert_eq!(stats.schema_version, SCHEMA_VERSION);
}

// ── 22. root_path is stored and survives re-crawl ─────────────────────────

#[test]
fn root_path_persists() {
    let (d, db) = tmp_dir("root_path");
    write_file(&d, "f.txt", ".");

    crawl(&d, &db);
    let store = open_store(&db);
    let root1 = store.get_stats().unwrap().root_path;
    assert!(root1.is_some());

    // Re-crawl — root_path should still be set.
    crawl(&d, &db);
    let store2 = open_store(&db);
    let root2 = store2.get_stats().unwrap().root_path;
    assert_eq!(root1, root2);
}

// ── 23. hidden files are included by default ──────────────────────────────

#[test]
fn hidden_files_are_included() {
    let (d, db) = tmp_dir("hidden");
    write_file(&d, ".secret", "hidden content");
    write_file(&d, "visible.txt", "visible");

    let summary = crawl(&d, &db);
    assert_eq!(summary.indexed, 2, "hidden file must be included");
    assert_eq!(summary.scanned, 2);
}

// ── 24. Target directory is excluded ──────────────────────────────────────

#[test]
fn target_directory_excluded() {
    let (d, db) = tmp_dir("target_test");
    write_file(&d, "src/main.rs", "fn main() {}");
    write_file(&d, "target/debug/build/output", "build artifact");

    let summary = crawl(&d, &db);
    assert_eq!(summary.indexed, 1, "only src/main.rs indexed");

    let skips: HashMap<_, _> = summary.skipped;
    assert!(skips.contains_key("target"));
}

// ── 25. venv is excluded ──────────────────────────────────────────────────

#[test]
fn venv_excluded() {
    let (d, db) = tmp_dir("venv_test");
    write_file(&d, "venv/lib/python3/site.py", "python");
    write_file(&d, "app.py", "import os");

    let summary = crawl(&d, &db);
    assert_eq!(summary.indexed, 1);

    let skips: HashMap<_, _> = summary.skipped;
    assert!(skips.contains_key("venv"));
}

// ── 26. Rename + content modification → changed, hash nulled (fs_id path) ──

// fs_id-dependent: without fs_id (Windows M0) a rename+modify legitimately
// becomes add+delete — that path is covered by
// `rename_with_content_change_fs_id_null_is_new_file`.
#[cfg(unix)]
#[test]
fn rename_with_content_change_counts_as_changed_and_nulls_hash() {
    let (d, db) = tmp_dir("ren_mod");
    let f = write_file(&d, "orig.txt", "version one");

    let s1 = crawl(&d, &db);
    assert_eq!(s1.indexed, 1);
    assert_eq!(s1.added, 1);

    // Simulate a prior hash pass.
    let store = open_store(&db);
    store
        .conn()
        .execute(
            "UPDATE files SET blake3 = X'0000000000000000000000000000000000000000000000000000000000000000',
                              magic = X'FF' WHERE deleted_at IS NULL",
            [],
        )
        .unwrap();
    let file_id: i64 = store
        .conn()
        .query_row(
            "SELECT file_id FROM files WHERE deleted_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();

    // Rename AND change content (different size) between scans.
    let new_path = d.join("renamed.txt");
    fs::rename(&f, &new_path).unwrap();
    fs::write(&new_path, "version two, longer content").unwrap();

    let s2 = crawl(&d, &db);
    assert_eq!(
        s2.changed, 1,
        "rename+modify must count as changed (stale hash), not renamed"
    );
    assert_eq!(s2.renamed, 0);
    assert_eq!(s2.added, 0);
    assert_eq!(s2.deleted, 0);
    assert_eq!(s2.indexed, 1);

    let store2 = open_store(&db);
    let (id2, path2, blake3, magic): (i64, String, Option<Vec<u8>>, Option<Vec<u8>>) = store2
        .conn()
        .query_row(
            "SELECT file_id, path, blake3, magic FROM files WHERE deleted_at IS NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(id2, file_id, "file_id must survive rename+modify");
    assert!(path2.contains("renamed.txt"), "path must be updated");
    assert!(blake3.is_none(), "blake3 must be NULL after rename+modify");
    assert!(magic.is_none(), "magic must be NULL after rename+modify");
}

// ── 27. Rename+modify with fs_id NULL → new file (fallback limitation) ─────
// Without fs_id the (size, mtime) fallback cannot match a file whose content
// changed (size/mtime differ by definition), so it becomes a new file and the
// old row is tombstoned. The new row has NULL blake3 — no stale hash possible.

#[test]
fn rename_with_content_change_fs_id_null_is_new_file() {
    let (d, db) = tmp_dir("ren_mod_fb");
    let f = write_file(&d, "orig.txt", "version one");

    crawl(&d, &db);

    // Force fs_id NULL to simulate Windows (no fs_id in M0).
    let store = open_store(&db);
    store
        .conn()
        .execute("UPDATE files SET fs_id = NULL", [])
        .unwrap();
    let old_id: i64 = store
        .conn()
        .query_row(
            "SELECT file_id FROM files WHERE deleted_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();

    // Rename AND modify.
    let new_path = d.join("renamed.txt");
    fs::rename(&f, &new_path).unwrap();
    fs::write(&new_path, "version two, longer content").unwrap();

    let s2 = crawl(&d, &db);
    assert_eq!(s2.added, 1, "no fs_id → rename+modify cannot be matched");
    assert_eq!(s2.renamed, 0);
    assert_eq!(s2.deleted, 1, "old row must be tombstoned");

    // Old row tombstoned, new row live with NULL hash.
    let store2 = open_store(&db);
    let (live_id, blake3): (i64, Option<Vec<u8>>) = store2
        .conn()
        .query_row(
            "SELECT file_id, blake3 FROM files WHERE deleted_at IS NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_ne!(live_id, old_id, "new file gets a fresh file_id");
    assert!(blake3.is_none(), "new row must have NULL blake3");
    let tombstones: i64 = store2
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM files WHERE deleted_at IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tombstones, 1);
}

// ── 28. Database inside crawl root is never indexed ───────────────────────

#[test]
fn db_inside_crawl_root_is_not_indexed() {
    // Deliberately place the DB inside the crawled tree (user misconfiguration).
    let base = std::env::temp_dir().join(format!("sagasu_test_dbinside_{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join("real.txt"), "real file").unwrap();

    let db_file = base.join("index.db");
    let summary = walk::crawl(CrawlConfig {
        root: base.clone(),
        db_path: db_file.clone(),
        exclude: vec![],
        no_default_excludes: false,
        hidden: Default::default(),
        use_gitignore: false,
        threads: 1,
    })
    .unwrap();

    // Only the real file is indexed; the db (+ wal/shm) is skipped.
    assert_eq!(summary.indexed, 1, "db file must not be indexed");
    assert_eq!(summary.scanned, 1, "db file must not count as scanned");

    let store = Store::open(&db_file).unwrap();
    let rows: Vec<String> = store
        .conn()
        .prepare("SELECT path FROM files WHERE deleted_at IS NULL")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].ends_with("real.txt"),
        "only real.txt indexed: {rows:?}"
    );

    // A second crawl must not see the db as 'changed' (no self-referential loop).
    let s2 = walk::crawl(CrawlConfig {
        root: base.clone(),
        db_path: db_file.clone(),
        exclude: vec![],
        no_default_excludes: false,
        hidden: Default::default(),
        use_gitignore: false,
        threads: 1,
    })
    .unwrap();
    assert_eq!(s2.indexed, 1);
    assert_eq!(s2.changed, 0, "db writes must not register as file changes");
    assert_eq!(s2.added, 0);
}

// ── 29. Failed crawl leaves meta intact (transactional meta) ──────────────

#[test]
fn failed_crawl_leaves_meta_intact() {
    let (d, db) = tmp_dir("fail_meta");
    write_file(&d, "f.txt", ".");

    // Successful crawl establishes baseline meta.
    crawl(&d, &db);
    let store = open_store(&db);
    let before = store.get_stats().unwrap();
    assert_eq!(before.scan_generation, 1);

    // Failed crawl (root does not exist) must not touch meta.
    let err = walk::crawl(CrawlConfig {
        root: d.join("does_not_exist"),
        db_path: db_path(&db),
        exclude: vec![],
        no_default_excludes: false,
        hidden: Default::default(),
        use_gitignore: false,
        threads: 1,
    });
    assert!(err.is_err(), "crawl of a missing root must fail");

    let store2 = open_store(&db);
    let after = store2.get_stats().unwrap();
    assert_eq!(
        after.scan_generation, before.scan_generation,
        "failed crawl must not bump the generation counter"
    );
    assert_eq!(
        after.scan_marker_ns, before.scan_marker_ns,
        "failed crawl must not move the scan marker"
    );
    assert_eq!(after.live_count, before.live_count);
}

// ── 30. Meta writes are transactional at the store level ──────────────────

#[test]
fn meta_writes_are_transactional() {
    let (d, db) = tmp_dir("meta_tx");
    write_file(&d, "f.txt", ".");

    crawl(&d, &db);
    let store = open_store(&db);

    // Rolled-back tx: nothing persists.
    {
        let tx = store.begin_tx().unwrap();
        Store::meta_set_tx(&tx, "test_key", "rolled-back").unwrap();
        let gen = Store::next_scan_generation_tx(&tx).unwrap();
        assert!(gen >= 1);
        drop(tx); // rollback
    }
    assert_eq!(
        store.meta_get("test_key").unwrap(),
        None,
        "rolled-back meta write must not persist"
    );
    let gen_after_rollback = store.get_stats().unwrap().scan_generation;

    // Committed tx: persists.
    {
        let tx = store.begin_tx().unwrap();
        Store::meta_set_tx(&tx, "test_key", "committed").unwrap();
        Store::next_scan_generation_tx(&tx).unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(
        store.meta_get("test_key").unwrap().as_deref(),
        Some("committed")
    );
    let gen_after_commit = store.get_stats().unwrap().scan_generation;
    assert_eq!(
        gen_after_commit,
        gen_after_rollback + 1,
        "committed generation bump must persist"
    );
}

// ── 31. A .gitignore is not consulted unless it is asked for (#14) ─────────

/// Crawl with the exclusion knobs #14 introduced.
fn crawl_scoped(
    dir: &Path,
    db_dir: &Path,
    hidden: walk::HiddenPolicy,
    use_gitignore: bool,
) -> walk::CrawlSummary {
    walk::crawl(CrawlConfig {
        root: dir.to_path_buf(),
        db_path: db_path(db_dir),
        exclude: vec![],
        no_default_excludes: false,
        hidden,
        use_gitignore,
        threads: 1,
    })
    .unwrap()
}

#[test]
fn a_gitignore_is_ignored_by_default_and_only_prunes_directories_when_asked() {
    let (d, db) = tmp_dir("gitignore");
    // A realistic root .gitignore: one generated directory, one class of file
    // people do not commit, one secret.
    write_file(&d, ".gitignore", "dist/\n*.log\n.env\n");
    write_file(&d, "dist/bundle.js", "generated");
    write_file(&d, "app.log", "line one\n");
    write_file(&d, ".env", "TOKEN=1\n");
    write_file(&d, "README.md", "# hi\n");

    // Default: the version-control rules say nothing about what is findable.
    let s = crawl_scoped(&d, &db, walk::HiddenPolicy::Include, false);
    assert_eq!(
        s.indexed, 5,
        "nothing is dropped by a .gitignore by default"
    );
    assert_eq!(s.skipped_gitignore, 0);

    // Opt in: the generated *directory* goes, the files stay. "Do not commit"
    // is not "do not find" — a log and a .env are things people search for.
    let (d2, db2) = tmp_dir("gitignore_on");
    write_file(&d2, ".gitignore", "dist/\n*.log\n.env\n");
    write_file(&d2, "dist/bundle.js", "generated");
    write_file(&d2, "app.log", "line one\n");
    write_file(&d2, ".env", "TOKEN=1\n");
    write_file(&d2, "README.md", "# hi\n");

    let s2 = crawl_scoped(&d2, &db2, walk::HiddenPolicy::Include, true);
    assert_eq!(s2.indexed, 4, "only dist/ is pruned");
    assert_eq!(s2.skipped_gitignore, 1);
    assert_eq!(s2.skipped_total(), 1);

    let store = open_store(&db2);
    let paths: Vec<String> = store
        .find_paths_like("", 100)
        .unwrap()
        .into_iter()
        .map(|r| r.path)
        .collect();
    assert!(paths.iter().any(|p| p.ends_with("app.log")), "{paths:?}");
    assert!(paths.iter().any(|p| p.ends_with(".env")), "{paths:?}");
    assert!(!paths.iter().any(|p| p.contains("bundle.js")), "{paths:?}");
}

// ── 32. Dot directories are content, on every platform (#14) ──────────────

#[test]
fn dot_directories_are_indexed_even_when_hidden_entries_are_skipped() {
    let (d, db) = tmp_dir("dotdirs");
    write_file(&d, ".github/workflows/ci.yml", "on: push\n");
    write_file(&d, ".config/app.toml", "[a]\n");
    write_file(&d, ".opencode/notes.md", "# rule\n");
    write_file(&d, "src/main.rs", "fn main() {}\n");

    // Even with `--skip-hidden`, a leading dot is not a hidden attribute: this
    // is the distinction issue #14 is about, and it must hold on Windows too,
    // where the walker would otherwise inherit the `ignore` crate's default.
    let s = crawl_scoped(&d, &db, walk::HiddenPolicy::SkipOsHidden, false);
    assert_eq!(s.indexed, 4, "dot directories are content");
    assert_eq!(s.skipped_hidden, 0);
    assert_eq!(s.skipped_total(), 0);

    let store = open_store(&db);
    let hits = store.find_paths_like(".github", 10).unwrap();
    assert_eq!(hits.len(), 1, "a file under .github must be findable");
}

// ── 33. The crawl's exclusion policy is what queries replay (#14) ─────────

#[test]
fn the_exclusion_policy_is_recorded_and_replayed_by_the_delta_path() {
    use sagasu_core::delta::DeltaConfig;

    let (d, db) = tmp_dir("policy_replay");
    write_file(&d, "a.txt", "hi");
    write_file(&d, "scratch/b.txt", "hi");
    write_file(&d, ".gitignore", "dist/\n");

    let summary = walk::crawl(CrawlConfig {
        root: d.clone(),
        db_path: db_path(&db),
        exclude: vec!["scratch".to_string()],
        no_default_excludes: false,
        hidden: walk::HiddenPolicy::SkipOsHidden,
        use_gitignore: true,
        threads: 1,
    })
    .unwrap();
    assert_eq!(summary.indexed, 2, "a.txt and .gitignore");

    // The delta side must reconstruct the *same* set. Rebuilding "the defaults"
    // instead is how `--exclude` used to disagree with every later query: the
    // crawl never saw scratch/, the delta scan did, and a live hit appeared for
    // a path with no index row behind it.
    let store = open_store(&db);
    let config = DeltaConfig::from_index(&store, &db_path(&db))
        .unwrap()
        .expect("the crawl recorded a root");
    assert!(config.excludes.contains("scratch"));
    assert_eq!(
        config.excludes.hidden_policy(),
        walk::HiddenPolicy::SkipOsHidden
    );
    assert!(config.excludes.uses_gitignore());
    assert_eq!(
        config
            .excludes
            .reason_for_path(&d.join("scratch/b.txt"), &d),
        Some(walk::ExcludeReason::Name("scratch".to_string()))
    );
    assert_eq!(
        config.excludes.reason_for_path(&d.join("dist/x.js"), &d),
        Some(walk::ExcludeReason::Gitignore)
    );
    assert_eq!(config.excludes.reason_for_path(&d.join("a.txt"), &d), None);
}

// ── 34. An index written before policies existed still answers (#14) ──────

#[test]
fn an_index_without_a_recorded_policy_falls_back_to_the_defaults() {
    use sagasu_core::delta::DeltaConfig;

    let (d, db) = tmp_dir("policy_absent");
    write_file(&d, "a.txt", "hi");
    crawl(&d, &db);

    // Simulate a database from before the policy row existed.
    let store = open_store(&db);
    store
        .conn()
        .execute("DELETE FROM meta WHERE key = 'exclude_policy'", [])
        .unwrap();

    let config = DeltaConfig::from_index(&store, &db_path(&db))
        .unwrap()
        .expect("root is still recorded");
    assert!(config.excludes.contains("node_modules"));
    assert_eq!(config.excludes.hidden_policy(), walk::HiddenPolicy::Include);
    assert!(!config.excludes.uses_gitignore());
}
