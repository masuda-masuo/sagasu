//! Writer guard + SQLite busy timeout (issue #77).
//!
//! Concurrent `index` / `hash` / `fulltext` / `tag` against the same database
//! is not supported. The marker lives in `meta`; a second writer is refused
//! before any crawl, tag or extraction. Reading commands never consult it.

use std::fs;
use std::path::{Path, PathBuf};

use sagasu_core::store::{Store, WriterGuard, BUSY_TIMEOUT_MS, WRITER_LOCK_KEY};

fn tmp_db(prefix: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!(
        "sagasu_writer_guard_{}_{}",
        prefix,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    (base.join("test.db"), base)
}

fn cleanup(base: &Path) {
    let _ = fs::remove_dir_all(base);
}

fn busy_timeout_ms(store: &Store) -> i64 {
    store
        .conn()
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn open_sets_busy_timeout_on_every_connection() {
    let (db, base) = tmp_db("busy");
    let store = Store::open(&db).unwrap();
    assert_eq!(busy_timeout_ms(&store), BUSY_TIMEOUT_MS as i64);
    drop(store);
    let again = Store::open(&db).unwrap();
    assert_eq!(busy_timeout_ms(&again), BUSY_TIMEOUT_MS as i64);
    cleanup(&base);
}

#[test]
fn second_writer_is_refused_with_command_pid_and_start_time() {
    let (db, base) = tmp_db("refuse");
    let first = WriterGuard::acquire(&db, "tag", false).unwrap();
    let occupied = Store::open(&db).unwrap().writer_lock().unwrap().unwrap();
    assert_eq!(occupied.command, "tag");
    assert_eq!(occupied.pid, std::process::id());
    assert!(
        occupied.started.ends_with('Z') && occupied.started.contains('T'),
        "start time should be UTC, got {}",
        occupied.started
    );

    let err = match WriterGuard::acquire(&db, "fulltext", false) {
        Ok(_) => panic!("second writer should have been refused"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("`tag`"), "{msg}");
    assert!(msg.contains(&occupied.pid.to_string()), "{msg}");
    assert!(msg.contains(&occupied.started), "{msg}");
    assert!(msg.contains("--force"), "{msg}");
    assert!(msg.contains("Wait"), "{msg}");

    drop(first);
    cleanup(&base);
}

#[test]
fn force_takes_the_marker_over() {
    let (db, base) = tmp_db("force");
    let first = WriterGuard::acquire(&db, "tag", false).unwrap();
    let second = WriterGuard::acquire(&db, "fulltext", true).unwrap();
    let occupied = Store::open(&db).unwrap().writer_lock().unwrap().unwrap();
    assert_eq!(occupied.command, "fulltext");
    assert_eq!(occupied.pid, std::process::id());

    // The original guard must not clear the takeover marker on drop.
    drop(first);
    let still = Store::open(&db).unwrap().writer_lock().unwrap().unwrap();
    assert_eq!(still.command, "fulltext");

    drop(second);
    assert!(Store::open(&db).unwrap().writer_lock().unwrap().is_none());
    cleanup(&base);
}

#[test]
fn successful_writer_leaves_no_marker() {
    let (db, base) = tmp_db("success");
    {
        let _guard = WriterGuard::acquire(&db, "index", false).unwrap();
        assert!(Store::open(&db).unwrap().writer_lock().unwrap().is_some());
    }
    assert!(Store::open(&db).unwrap().writer_lock().unwrap().is_none());
    drop(WriterGuard::acquire(&db, "hash", false).unwrap());
    cleanup(&base);
}

#[test]
fn failed_writer_leaves_no_marker() {
    let (db, base) = tmp_db("fail");
    let result: Result<(), String> = (|| {
        let _guard = WriterGuard::acquire(&db, "fulltext", false).map_err(|e| e.to_string())?;
        Err("boom midway".to_string())
    })();
    assert!(result.is_err());
    assert!(Store::open(&db).unwrap().writer_lock().unwrap().is_none());
    assert!(Store::open(&db)
        .unwrap()
        .meta_get(WRITER_LOCK_KEY)
        .unwrap()
        .is_none());
    cleanup(&base);
}

#[test]
fn readers_work_the_same_while_a_marker_is_set() {
    let (db, base) = tmp_db("readers");

    // The database must hold rows for this test to mean anything: on an empty
    // one every "the answer is unchanged" assertion is empty == empty, which a
    // reader that wrongly consulted the marker would satisfy too.
    let corpus = base.join("corpus");
    fs::create_dir_all(&corpus).unwrap();
    for i in 0..5 {
        fs::write(corpus.join(format!("doc_{i}_report.txt")), "body\n").unwrap();
    }
    sagasu_core::walk::crawl(sagasu_core::walk::CrawlConfig {
        root: corpus.clone(),
        db_path: db.clone(),
        exclude: vec![],
        no_default_excludes: false,
        hidden: Default::default(),
        use_gitignore: false,
        threads: 1,
    })
    .unwrap();

    let before = {
        let store = Store::open(&db).unwrap();
        store.get_stats().unwrap()
    };
    assert_eq!(before.live_count, 5, "fixture must not be empty");
    let matched_before = Store::open(&db)
        .unwrap()
        .find_paths_like("report", 10)
        .unwrap();
    assert_eq!(matched_before.len(), 5, "fixture must not be empty");

    let _guard = WriterGuard::acquire(&db, "tag", false).unwrap();
    let store = Store::open(&db).unwrap();
    assert_eq!(busy_timeout_ms(&store), BUSY_TIMEOUT_MS as i64);
    let after = store.get_stats().unwrap();
    assert_eq!(after.live_count, before.live_count);
    assert_eq!(after.root_path, before.root_path);
    assert_eq!(after.schema_version, before.schema_version);
    assert_eq!(after.tombstone_count, before.tombstone_count);

    let matched_after = store.find_paths_like("report", 10).unwrap();
    let paths_before: Vec<&str> = matched_before.iter().map(|r| r.path.as_str()).collect();
    let paths_after: Vec<&str> = matched_after.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths_after, paths_before);

    // Reading must not consume or clear the marker.
    assert_eq!(store.writer_lock().unwrap().unwrap().command, "tag");
    cleanup(&base);
}
