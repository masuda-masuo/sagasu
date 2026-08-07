//! Integration tests for the freshness layer: delta sources, the search-time
//! merge, and the staleness reporting around them (design.md §5, issues #3/#16).
//!
//! The shape of every acceptance test here is the same, and it is the one the
//! milestone is about:
//!
//! 1. build a corpus and index it,
//! 2. change the filesystem — add, edit, rename, delete,
//! 3. search **without re-indexing**,
//! 4. assert the answer reflects step 2.
//!
//! Temporary directories live under `std::env::temp_dir()` and are keyed by test
//! name so the tests can run in parallel.

use std::fs;
use std::path::{Path, PathBuf};

use sagasu_core::delta::{
    self, DeltaCache, DeltaSource, DeltaSourceKind, DeltaStatus, MtimeDeltaSource, RescanReason,
    ScanMarker,
};
use sagasu_core::fresh::{self, FreshConfig, FreshOutcome, HitOrigin, StaleKind};
use sagasu_core::fulltext::{self, FulltextConfig};
use sagasu_core::store::Store;
use sagasu_core::walk::{self, CrawlConfig, ExcludeSet};

// ── helpers ─────────────────────────────────────────────────────────────────

/// Create a temporary working area. Returns (data_dir, db_dir, index_dir).
/// The database and the tantivy index live outside the crawled tree so the
/// walker — and the delta walk — never see their files.
fn tmp_dirs(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("sagasu_fresh_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let (data, db, index) = (base.join("data"), base.join("db"), base.join("ft"));
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&db).unwrap();
    (data, db, index)
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

fn crawl(data: &Path, db_dir: &Path) {
    walk::crawl(CrawlConfig {
        root: data.to_path_buf(),
        db_path: db_path(db_dir),
        exclude: vec![],
        no_default_excludes: false,
        hidden: Default::default(),
        use_gitignore: false,
        threads: 1,
    })
    .unwrap();
}

fn build_ft(db_dir: &Path, index_dir: &Path) {
    let mut config = FulltextConfig::new(db_path(db_dir), index_dir);
    // Small writer budget so many tests can run in parallel (tantivy wants
    // ≥15 MB per writer thread).
    config.heap_bytes = 16 * 1024 * 1024;
    config.threads = 2;
    fulltext::build(&config).unwrap();
}

/// Give the filesystem clock a moment to move past the crawl marker.
///
/// Sized for Windows, where this is not cosmetic: `SystemTime::now()` (the
/// marker) comes from `GetSystemTimePreciseAsFileTime`, while a file's
/// LastWriteTime comes from the interrupt-driven system clock at roughly 15.6 ms
/// granularity. A file written a few milliseconds after the marker can therefore
/// carry a timestamp *older* than it. 30 ms clears that window with room to
/// spare; on the nanosecond-resolution filesystems elsewhere it is free.
fn tick() {
    std::thread::sleep(std::time::Duration::from_millis(30));
}

/// Whether the delta source that answered this query can observe a rename.
///
/// Branching on the reported capability rather than on `cfg!(windows)` keeps the
/// assertions tied to what actually ran: the `stat` fallback infers renames from
/// `st_ctime` and so cannot see them on Windows, while the USN source reports
/// them everywhere it is enabled.
fn detects_renames(outcome: &FreshOutcome) -> bool {
    outcome
        .delta
        .as_ref()
        .map(|d| d.detects_renames)
        .unwrap_or(false)
}

/// Describe the delta source in an assertion message, so a failure on a platform
/// we cannot run locally says which path produced it.
fn delta_note(outcome: &FreshOutcome) -> String {
    match &outcome.delta {
        Some(d) => format!(
            "source={} renames={} entries={} scanned={} excluded={} status={:?}",
            d.kind.as_str(),
            d.detects_renames,
            d.entries,
            d.scanned,
            d.excluded,
            d.status
        ),
        None => "no delta taken".to_string(),
    }
}

fn find_config(db_dir: &Path, query: &str) -> FreshConfig {
    FreshConfig::new(db_path(db_dir), query)
}

fn search_config(db_dir: &Path, index_dir: &Path, query: &str) -> FreshConfig {
    let mut c = FreshConfig::new(db_path(db_dir), query);
    c.index_dir = Some(index_dir.to_path_buf());
    c
}

/// Basenames of the merged hits, in result order.
fn names(outcome: &FreshOutcome) -> Vec<String> {
    outcome
        .hits
        .iter()
        .map(|h| {
            Path::new(&h.path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect()
}

fn origin_of(outcome: &FreshOutcome, basename: &str) -> Option<HitOrigin> {
    outcome
        .hits
        .iter()
        .find(|h| h.path.ends_with(basename))
        .map(|h| h.origin)
}

// ── 1. Acceptance: metadata search after add / change / delete ──────────────

#[test]
fn metadata_search_sees_a_file_added_after_indexing() {
    let (d, db, _) = tmp_dirs("meta_add");
    write_file(&d, "old_report.md", "before");
    crawl(&d, &db);
    tick();

    write_file(&d, "new_report.md", "after");

    let outcome = fresh::find(&find_config(&db, "report"), None).unwrap();
    let n = names(&outcome);
    assert!(n.contains(&"new_report.md".to_string()), "{n:?}");
    assert!(n.contains(&"old_report.md".to_string()), "{n:?}");
    assert_eq!(origin_of(&outcome, "new_report.md"), Some(HitOrigin::Live));
    assert_eq!(origin_of(&outcome, "old_report.md"), Some(HitOrigin::Index));
    assert!(outcome.stale.is_none(), "{:?}", outcome.stale);
}

#[test]
fn metadata_search_drops_a_file_deleted_after_indexing() {
    let (d, db, _) = tmp_dirs("meta_delete");
    write_file(&d, "keep_report.md", "a");
    let gone = write_file(&d, "gone_report.md", "b");
    crawl(&d, &db);
    tick();

    fs::remove_file(&gone).unwrap();

    let outcome = fresh::find(&find_config(&db, "report"), None).unwrap();
    assert_eq!(names(&outcome), vec!["keep_report.md".to_string()]);
    assert_eq!(outcome.dropped_deleted, 1);
    // Deletion is caught by the merge's existence check, not by the delta walk:
    // an mtime walk cannot see a file that is no longer there.
    assert_eq!(outcome.delta.as_ref().unwrap().entries, 0);
}

#[test]
fn metadata_search_returns_fresh_size_for_a_file_changed_after_indexing() {
    let (d, db, _) = tmp_dirs("meta_change");
    write_file(&d, "notes_report.md", "short");
    crawl(&d, &db);
    tick();

    write_file(&d, "notes_report.md", &"x".repeat(4096));

    let outcome = fresh::find(&find_config(&db, "report"), None).unwrap();
    assert_eq!(outcome.hits.len(), 1);
    let hit = &outcome.hits[0];
    assert_eq!(hit.origin, HitOrigin::Live);
    assert_eq!(hit.size, Some(4096), "live hits carry re-read metadata");
    // The index row for the same path was dropped rather than returned twice.
    assert_eq!(outcome.dropped_changed, 1);
    // ...and the stable file ID survived the swap.
    assert!(hit.file_id.is_some());
}

#[test]
fn metadata_search_follows_a_rename_made_after_indexing() {
    let (d, db, _) = tmp_dirs("meta_rename");
    write_file(&d, "draft_report.md", "content");
    crawl(&d, &db);
    tick();

    fs::rename(d.join("draft_report.md"), d.join("final_report.md")).unwrap();

    let outcome = fresh::find(&find_config(&db, "report"), None).unwrap();
    let n = names(&outcome);

    // Either way the stale path must go: returning a file that is no longer
    // there is the one answer that is always wrong.
    assert_eq!(
        outcome.dropped_deleted,
        1,
        "the old path no longer exists ({})",
        delta_note(&outcome)
    );

    if detects_renames(&outcome) {
        assert_eq!(
            n,
            vec!["final_report.md".to_string()],
            "{n:?} ({})",
            delta_note(&outcome)
        );
    } else {
        // A `stat` walk on Windows cannot see this: NTFS leaves LastWriteTime
        // untouched across a rename and std::fs exposes no ChangeTime, so the
        // new path is invisible until the next crawl. Documented in
        // docs/design.md §5-2; the CLI prints the caveat with every result.
        assert!(n.is_empty(), "{n:?} ({})", delta_note(&outcome));
    }
}

#[test]
fn a_rename_a_source_cannot_see_drops_the_old_path_rather_than_returning_it() {
    // The Windows `stat` fallback cannot observe a rename, so its delta set is
    // empty in exactly this situation. Reproducing that precondition here (with
    // the merge switched off, which empties the delta set the same way) checks
    // the resulting behaviour on a platform the suite actually runs on, instead
    // of leaving the Windows branch of the rename tests asserted blind.
    let (d, db, _) = tmp_dirs("blind_rename");
    write_file(&d, "draft_report.md", "content");
    crawl(&d, &db);
    tick();

    fs::rename(d.join("draft_report.md"), d.join("final_report.md")).unwrap();

    let mut config = find_config(&db, "report");
    config.no_delta = true;
    let outcome = fresh::find(&config, None).unwrap();

    // The vanished path must not be returned...
    assert!(names(&outcome).is_empty(), "{:?}", names(&outcome));
    assert_eq!(outcome.dropped_deleted, 1);
    // ...and the answer must admit it is not merged.
    assert!(outcome.stale.is_some());
}

// ── 2. Acceptance: full-text search after add / change / delete ─────────────

#[test]
fn fulltext_search_sees_a_file_added_after_indexing() {
    let (d, db, ft) = tmp_dirs("ft_add");
    write_file(&d, "a.md", "既存の文書には索引という語が入っている。\n");
    crawl(&d, &db);
    build_ft(&db, &ft);
    tick();

    write_file(&d, "b.md", "新しい文書にも索引という語を書いた。\n");

    let outcome = fresh::search(&search_config(&db, &ft, "索引"), None).unwrap();
    let n = names(&outcome);
    assert!(n.contains(&"b.md".to_string()), "{n:?}");
    assert!(n.contains(&"a.md".to_string()), "{n:?}");
    assert_eq!(origin_of(&outcome, "b.md"), Some(HitOrigin::Live));
    // Live hits come first so `--limit` can never cut the fresh ones off.
    assert_eq!(n[0], "b.md", "{n:?}");
}

#[test]
fn fulltext_search_reflects_an_edit_made_after_indexing() {
    let (d, db, ft) = tmp_dirs("ft_edit");
    write_file(&d, "doc.md", "この文書には索引という語が入っている。\n");
    write_file(&d, "other.md", "こちらの文書にも索引がある。\n");
    crawl(&d, &db);
    build_ft(&db, &ft);
    tick();

    // The term the index knows about is removed; a new one takes its place.
    write_file(
        &d,
        "doc.md",
        "この文書は書き換えられ、鮮度という語になった。\n",
    );

    // The removed term must no longer match the edited file.
    let stale_term = fresh::search(&search_config(&db, &ft, "索引"), None).unwrap();
    let n = names(&stale_term);
    assert!(!n.contains(&"doc.md".to_string()), "edited away, but {n:?}");
    assert!(n.contains(&"other.md".to_string()), "{n:?}");
    assert_eq!(stale_term.dropped_changed, 1);

    // ...and the new term must match it, even though the index has never seen it.
    let new_term = fresh::search(&search_config(&db, &ft, "鮮度"), None).unwrap();
    let n = names(&new_term);
    assert_eq!(n, vec!["doc.md".to_string()], "{n:?}");
    assert_eq!(new_term.hits[0].origin, HitOrigin::Live);
    assert!(
        !new_term.hits[0].snippet.is_empty(),
        "live hits get snippets"
    );
}

#[test]
fn fulltext_search_drops_a_file_deleted_after_indexing() {
    let (d, db, ft) = tmp_dirs("ft_delete");
    write_file(&d, "keep.md", "索引の話。\n");
    write_file(&d, "gone.md", "索引の話その二。\n");
    crawl(&d, &db);
    build_ft(&db, &ft);
    tick();

    fs::remove_file(d.join("gone.md")).unwrap();

    let outcome = fresh::search(&search_config(&db, &ft, "索引"), None).unwrap();
    assert_eq!(names(&outcome), vec!["keep.md".to_string()]);
    assert_eq!(outcome.dropped_deleted, 1);
}

#[test]
fn fulltext_search_follows_a_rename_made_after_indexing() {
    let (d, db, ft) = tmp_dirs("ft_rename");
    write_file(&d, "before.md", "移動しても索引から辿れるべき本文。\n");
    crawl(&d, &db);
    build_ft(&db, &ft);
    tick();

    fs::rename(d.join("before.md"), d.join("after.md")).unwrap();

    let outcome = fresh::search(&search_config(&db, &ft, "索引"), None).unwrap();
    let n = names(&outcome);

    assert_eq!(
        outcome.dropped_deleted,
        1,
        "the indexed path no longer exists ({})",
        delta_note(&outcome)
    );

    if detects_renames(&outcome) {
        assert_eq!(
            n,
            vec!["after.md".to_string()],
            "({})",
            delta_note(&outcome)
        );
        assert_eq!(outcome.hits[0].origin, HitOrigin::Live);
    } else {
        // See the metadata rename test: invisible to a `stat` walk on Windows.
        assert!(n.is_empty(), "{n:?} ({})", delta_note(&outcome));
    }
}

#[test]
fn fulltext_live_side_honours_query_negation() {
    let (d, db, ft) = tmp_dirs("ft_negation");
    write_file(&d, "seed.md", "索引の説明。\n");
    crawl(&d, &db);
    build_ft(&db, &ft);
    tick();

    write_file(&d, "wanted.md", "索引についての新しいメモ。\n");
    write_file(&d, "unwanted.md", "索引と鮮度の両方に触れたメモ。\n");

    // Both new files contain 索引; only one also contains 鮮度.
    let outcome = fresh::search(&search_config(&db, &ft, "索引 -鮮度"), None).unwrap();
    let n = names(&outcome);
    assert!(n.contains(&"wanted.md".to_string()), "{n:?}");
    assert!(
        !n.contains(&"unwanted.md".to_string()),
        "a negated term must exclude a live hit too: {n:?}"
    );
}

#[test]
fn live_hits_lead_the_page_without_taking_all_of_it() {
    let (d, db, _) = tmp_dirs("live_share");
    for i in 0..6 {
        write_file(&d, &format!("indexed_{i}_report.md"), "x");
    }
    crawl(&d, &db);
    tick();

    // More changed files than the whole page: without a reservation the ranked
    // index result would vanish behind them.
    for i in 0..6 {
        write_file(&d, &format!("changed_{i}_report.md"), "y");
    }

    let mut config = find_config(&db, "report");
    config.limit = 4;
    let outcome = fresh::find(&config, None).unwrap();

    assert_eq!(outcome.hits.len(), 4);
    assert_eq!(
        outcome.live_hits, 2,
        "live hits are capped at half the page"
    );
    // ...and the ones that are there come first.
    assert_eq!(outcome.hits[0].origin, HitOrigin::Live);
    assert_eq!(outcome.hits[1].origin, HitOrigin::Live);
    assert_eq!(outcome.hits[2].origin, HitOrigin::Index);
}

#[test]
fn live_hits_take_the_free_slots_when_the_index_has_none() {
    let (d, db, _) = tmp_dirs("live_share_free");
    write_file(&d, "seed.txt", "x");
    crawl(&d, &db);
    tick();

    for i in 0..4 {
        write_file(&d, &format!("changed_{i}_report.md"), "y");
    }

    let mut config = find_config(&db, "report");
    config.limit = 4;
    let outcome = fresh::find(&config, None).unwrap();

    assert_eq!(outcome.hits.len(), 4, "no index hits to reserve slots for");
    assert_eq!(outcome.live_hits, 4);
}

#[test]
fn the_live_grep_uses_the_same_extension_policy_as_the_index_build() {
    let (d, db, ft) = tmp_dirs("ft_ext");
    write_file(&d, "custom.obj", "索引を含む拡張子のファイル。\n");
    crawl(&d, &db);

    // The index was told to accept `.obj` (which is on the built-in binary denylist)...
    let mut build = FulltextConfig::new(db_path(&db), &ft);
    build.heap_bytes = 16 * 1024 * 1024;
    build.threads = 2;
    build.extra_exts = vec!["obj".to_string()];
    fulltext::build(&build).unwrap();
    tick();

    write_file(&d, "custom.obj", "書き換えたが索引という語は残す。\n");

    // ...so a search that does not know about `.obj` drops the changed index hit
    // and has nothing to put in its place.
    let unaware = fresh::search(&search_config(&db, &ft, "索引"), None).unwrap();
    assert!(unaware.hits.is_empty(), "{:?}", names(&unaware));
    assert_eq!(unaware.dropped_changed, 1);

    // Told the same thing the build was told, the live side refreshes it.
    let mut aware = search_config(&db, &ft, "索引");
    aware.extra_exts = vec!["obj".to_string()];
    let aware = fresh::search(&aware, None).unwrap();
    assert_eq!(names(&aware), vec!["custom.obj".to_string()]);
    assert_eq!(aware.hits[0].origin, HitOrigin::Live);
}

// ── 3. Threshold: truncation is reported, not hidden ────────────────────────

#[test]
fn a_delta_set_over_the_cap_is_truncated_and_reported_stale() {
    let (d, db, _) = tmp_dirs("truncate");
    write_file(&d, "seed_report.md", "seed");
    crawl(&d, &db);
    tick();

    for i in 0..5 {
        write_file(&d, &format!("changed_{i}_report.md"), "new");
    }

    let mut config = find_config(&db, "report");
    config.delta_limit = 2;
    let outcome = fresh::find(&config, None).unwrap();

    let delta = outcome.delta.as_ref().unwrap();
    assert!(
        matches!(delta.status, DeltaStatus::Truncated { limit: 2 }),
        "{:?}",
        delta.status
    );
    assert!(delta.entries <= 2, "{}", delta.entries);

    let notice = outcome.stale.expect("truncation must be reported");
    assert_eq!(notice.kind, StaleKind::DeltaTruncated);
    assert!(
        notice.message.contains("index is stale"),
        "{}",
        notice.message
    );
}

#[test]
fn truncation_is_a_different_branch_from_a_missing_marker() {
    let (d, db, _) = tmp_dirs("no_marker");
    write_file(&d, "a_report.md", "x");
    crawl(&d, &db);

    // Simulate an index that predates the marker (or a corrupted one).
    let store = Store::open(db_path(&db)).unwrap();
    store.meta_delete("delta_marker").unwrap();
    drop(store);

    let outcome = fresh::find(&find_config(&db, "report"), None).unwrap();
    let notice = outcome
        .stale
        .clone()
        .expect("a missing marker must be reported");
    assert_eq!(
        notice.kind,
        StaleKind::RescanRequired(RescanReason::MarkerMissing)
    );
    // No delta could be taken at all — this is not "the set was too big".
    assert!(outcome.delta.is_none());
    // The index answer is still returned; it is labelled, not withheld.
    assert_eq!(names(&outcome), vec!["a_report.md".to_string()]);
}

#[test]
fn disabling_the_merge_still_says_the_answer_is_unmerged() {
    let (d, db, _) = tmp_dirs("no_delta");
    write_file(&d, "a_report.md", "x");
    crawl(&d, &db);
    tick();
    write_file(&d, "b_report.md", "y");

    let mut config = find_config(&db, "report");
    config.no_delta = true;
    let outcome = fresh::find(&config, None).unwrap();

    assert_eq!(names(&outcome), vec!["a_report.md".to_string()]);
    assert_eq!(outcome.stale.unwrap().kind, StaleKind::MergeDisabled);
    assert_eq!(outcome.timing.delta_ms, 0.0);
}

// ── 4. The delta source itself ──────────────────────────────────────────────

/// A delta source over `root`, configured exactly the way `fresh::find` does it.
///
/// The root is canonicalized because that is what `sagasu index` stores in
/// `meta.root_path` and therefore what production always passes. Handing a
/// source a raw path instead used to exercise a code path the product never
/// reaches — on Windows `canonicalize` yields a `\\?\` verbatim path, and the
/// two shapes selected two different sources.
fn source_for_root(root: &Path, db_dir: &Path) -> Box<dyn DeltaSource> {
    delta::source_for(&delta::DeltaConfig {
        root: root.canonicalize().unwrap(),
        excludes: ExcludeSet::new(&[], false),
        skip_paths: walk::db_sibling_paths(&db_path(db_dir)),
        threads: 1,
    })
}

#[test]
fn the_delta_set_applies_the_crawls_exclusion_set() {
    let (d, db, _) = tmp_dirs("delta_excludes");
    write_file(&d, "src/main.rs", "fn main() {}");
    crawl(&d, &db);
    tick();

    write_file(&d, "src/lib.rs", "pub fn a() {}");
    write_file(&d, "node_modules/pkg/index.js", "module.exports = 1;");
    write_file(&d, "target/debug/build.log", "noise");

    let marker = Store::open(db_path(&db))
        .unwrap()
        .delta_marker()
        .unwrap()
        .unwrap();
    let set = source_for_root(&d, &db)
        .changes_since(&marker, delta::DEFAULT_DELTA_LIMIT)
        .unwrap();

    let note = format!(
        "source={} scanned={} excluded={} status={:?}",
        set.kind.as_str(),
        set.scanned,
        set.excluded,
        set.status
    );
    let paths: Vec<&str> = set.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths.len(), 1, "{paths:?} ({note})");
    assert!(paths[0].ends_with("lib.rs"), "{paths:?} ({note})");
    assert!(
        set.excluded >= 2,
        "excluded files must be counted, not just dropped ({note})"
    );
    assert_eq!(set.status, DeltaStatus::Complete, "({note})");
}

#[test]
fn the_delta_set_never_contains_our_own_database() {
    let (d, _db, _) = tmp_dirs("delta_db_inside");
    // Deliberately place the database inside the crawl root — the configuration
    // the CLI warns about, but which must still not poison the delta set.
    let inner_db = d.join("index.db");
    write_file(&d, "a.txt", "x");
    walk::crawl(CrawlConfig {
        root: d.clone(),
        db_path: inner_db.clone(),
        exclude: vec![],
        no_default_excludes: false,
        hidden: Default::default(),
        use_gitignore: false,
        threads: 1,
    })
    .unwrap();
    tick();

    // Touch the database the way any later query would.
    let store = Store::open(&inner_db).unwrap();
    store.meta_set("probe", "1").unwrap();
    let marker = store.delta_marker().unwrap().unwrap();
    drop(store);

    let source = delta::source_for(&delta::DeltaConfig {
        root: d.canonicalize().unwrap(),
        excludes: ExcludeSet::new(&[], false),
        skip_paths: walk::db_sibling_paths(&inner_db),
        threads: 1,
    });
    let set = source
        .changes_since(&marker, delta::DEFAULT_DELTA_LIMIT)
        .unwrap();

    assert!(
        set.entries.iter().all(|e| !e.path.contains("index.db")),
        "{:?}",
        set.entries.iter().map(|e| &e.path).collect::<Vec<_>>()
    );
}

#[test]
fn the_mtime_source_accepts_a_usn_marker_by_falling_back_to_its_timestamp() {
    let (d, db, _) = tmp_dirs("marker_degrade");
    write_file(&d, "a.txt", "x");
    crawl(&d, &db);
    tick();
    write_file(&d, "b.txt", "y");

    // A Windows index opened where the journal is unavailable: the USN marker is
    // all we have, and its wall clock still makes the fallback usable.
    let recorded_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
        - 60 * 1_000_000_000;
    let marker = ScanMarker::Usn {
        volume: "C:".into(),
        journal_id: 1,
        next_usn: 0,
        maximum_size: 1,
        recorded_ns,
    };

    let source = MtimeDeltaSource::new(delta::DeltaConfig::new(&d));
    let set = source
        .changes_since(&marker, delta::DEFAULT_DELTA_LIMIT)
        .unwrap();

    assert_eq!(source.kind(), DeltaSourceKind::Mtime);
    assert_eq!(set.status, DeltaStatus::Complete);
    assert_eq!(
        set.entries.len(),
        2,
        "a marker a minute old covers both files"
    );
}

// ── 5. The cache ────────────────────────────────────────────────────────────

#[test]
fn the_cache_serves_a_second_query_without_re_walking() {
    let (d, db, _) = tmp_dirs("cache_hit");
    write_file(&d, "a_report.md", "x");
    crawl(&d, &db);
    tick();
    write_file(&d, "b_report.md", "y");

    let cache = DeltaCache::new();
    let first = fresh::find(&find_config(&db, "report"), Some(&cache)).unwrap();
    let second = fresh::find(&find_config(&db, "repo"), Some(&cache)).unwrap();

    assert!(!first.delta.as_ref().unwrap().cached);
    assert!(second.delta.as_ref().unwrap().cached);
    assert_eq!(cache.stats().hits, 1);
    assert_eq!(cache.stats().misses, 1);
    // Both queries saw the same delta set, so both saw the new file.
    assert!(names(&first).contains(&"b_report.md".to_string()));
    assert!(names(&second).contains(&"b_report.md".to_string()));
}

#[test]
fn the_cache_is_invalidated_by_a_re_crawl_and_by_hand() {
    let (d, db, _) = tmp_dirs("cache_invalidate");
    write_file(&d, "a_report.md", "x");
    crawl(&d, &db);
    tick();
    write_file(&d, "b_report.md", "y");

    let cache = DeltaCache::new();
    fresh::find(&find_config(&db, "report"), Some(&cache)).unwrap();
    assert_eq!(cache.stats().misses, 1);

    // An explicit drop forces the next query to ask the source again.
    cache.invalidate();
    let after_invalidate = fresh::find(&find_config(&db, "report"), Some(&cache)).unwrap();
    assert!(!after_invalidate.delta.as_ref().unwrap().cached);
    assert_eq!(cache.stats().misses, 2);

    // A re-crawl moves the marker, which makes every cached entry meaningless
    // even inside the TTL.
    crawl(&d, &db);
    let after_recrawl = fresh::find(&find_config(&db, "report"), Some(&cache)).unwrap();
    assert!(!after_recrawl.delta.as_ref().unwrap().cached);
    assert_eq!(cache.stats().marker_invalidations, 1);
    // Both files are in the index now, so nothing is left on the live side.
    assert_eq!(after_recrawl.delta.as_ref().unwrap().entries, 0);
    assert_eq!(names(&after_recrawl).len(), 2);
}

#[test]
fn a_zero_ttl_cache_never_reuses_a_set() {
    let (d, db, _) = tmp_dirs("cache_zero_ttl");
    write_file(&d, "a_report.md", "x");
    crawl(&d, &db);

    let cache = DeltaCache::with_ttl_ms(0);
    fresh::find(&find_config(&db, "report"), Some(&cache)).unwrap();
    let second = fresh::find(&find_config(&db, "report"), Some(&cache)).unwrap();

    assert!(!second.delta.as_ref().unwrap().cached);
    assert_eq!(cache.stats().hits, 0);
    assert_eq!(cache.stats().misses, 2);
}

// ── 6. Marker plumbing ──────────────────────────────────────────────────────

#[test]
fn a_crawl_records_a_delta_marker_and_bumps_it_on_re_crawl() {
    let (d, db, _) = tmp_dirs("marker_recorded");
    write_file(&d, "a.txt", "x");
    crawl(&d, &db);

    let store = Store::open(db_path(&db)).unwrap();
    let first = store.delta_marker().unwrap().expect("marker recorded");
    assert_eq!(store.get_stats().unwrap().delta_marker, Some(first.clone()));
    drop(store);

    tick();
    crawl(&d, &db);

    let store = Store::open(db_path(&db)).unwrap();
    let second = store.delta_marker().unwrap().unwrap();
    assert_ne!(first, second, "a re-crawl must move the marker forward");
    assert!(second.wall_clock_ns() > first.wall_clock_ns());
}

#[test]
fn an_empty_query_is_rejected_rather_than_matching_everything() {
    let (d, db, _) = tmp_dirs("empty_query");
    write_file(&d, "a.txt", "x");
    crawl(&d, &db);

    assert!(fresh::find(&find_config(&db, "   "), None).is_err());
}

#[test]
fn a_literal_percent_in_a_query_is_not_a_wildcard() {
    let (d, db, _) = tmp_dirs("like_escape");
    write_file(&d, "100%_done.txt", "x");
    write_file(&d, "unrelated.txt", "y");
    crawl(&d, &db);

    let outcome = fresh::find(&find_config(&db, "%_"), None).unwrap();
    assert_eq!(names(&outcome), vec!["100%_done.txt".to_string()]);
}
