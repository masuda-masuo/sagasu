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
//!
//! ## Timestamp discipline
//!
//! The delta walk decides "changed since the marker" by comparing kernel
//! timestamps against the marker the crawl recorded, and kernel-assigned
//! timestamps come from a coarse clock that can lag the marker's precise
//! `SystemTime::now()` — so no test here *assumes* a freshly written file lands
//! after the marker. Every file whose presence in the delta set is asserted is
//! stamped explicitly past the recorded marker (`stamp_after_marker`); renames
//! ride on `st_ctime`, which no API can set, so their precondition is *verified*
//! with a bounded retry (`ensure_ctime_after_marker`). Files whose *absence*
//! from the delta set is asserted are never metadata-touched after the crawl —
//! any utimensat/chmod bumps ctime, and the unix source treats `ctime > marker`
//! as changed (the "ctime trap").

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

/// Margin past the recorded marker at which changed files are stamped.
///
/// An explicitly-set mtime is stored as given, so ten seconds is effectively
/// infinite margin: the coarse-clock lag that made the old 30 ms sleep flaky
/// only affects *kernel-assigned* timestamps, never a value written by
/// `utimensat`.
const TIMESTAMP_MARGIN_NS: i64 = 10 * 1_000_000_000;

/// The wall-clock marker (unix ns) the last crawl recorded.
fn recorded_marker_ns(db_dir: &Path) -> i64 {
    Store::open(db_path(db_dir))
        .unwrap()
        .delta_marker()
        .unwrap()
        .expect("a crawl must record a delta marker")
        .wall_clock_ns()
}

/// Set `path`'s mtime to exactly `ns` (unix ns), leaving atime alone.
fn set_mtime_ns(path: &Path, ns: i64) {
    filetime::set_file_mtime(
        path,
        filetime::FileTime::from_unix_time(
            ns.div_euclid(1_000_000_000),
            ns.rem_euclid(1_000_000_000) as u32,
        ),
    )
    .unwrap();
}

/// Make a file that must appear in the delta set compare as changed since the
/// recorded marker — as an explicit fact, not a race.
///
/// Write the file, then stamp its mtime to `marker_ns + TIMESTAMP_MARGIN_NS`.
/// This replaces the old `tick()` ("sleep 30 ms and hope the kernel clock moved
/// past the marker") with a value that is guaranteed newer. It also bumps
/// ctime, which is consistent: the file is supposed to look touched. It is only
/// ever called on files whose *presence* in the delta set the test asserts;
/// files whose absence is asserted are never metadata-touched after the crawl
/// (the ctime trap — see the module docs).
fn stamp_after_marker(path: &Path, marker_ns: i64) {
    set_mtime_ns(path, marker_ns + TIMESTAMP_MARGIN_NS);
}

/// Make a rename's detectability a *verified* precondition instead of an
/// assumed one.
///
/// On unix the mtime source sees a rename through `st_ctime`, which no API can
/// set explicitly. The rename itself bumps ctime, but the kernel assigns it
/// from the coarse clock, which can lag the marker — so after the rename,
/// verify ctime is past the marker; if not, bump it again with a metadata no-op
/// (re-applying the same mode is a `chmod`, which updates ctime and never
/// touches mtime — keeping the "renamed, not modified" shape the source keys
/// on) and re-check, bounded.
///
/// A same-mode `chmod` only advances ctime when it lands in a *new* coarse
/// clock tick: issued back-to-back with the rename it can be a no-op, which is
/// why each retry sleeps 5 ms before re-checking — every attempt lands in a
/// fresh tick and is therefore effective. The 100-attempt bound (~0.5 s of
/// ticks) covers any realistic lag; exhaustion panics loudly rather than
/// answering with an unverifiable delta set.
///
/// Unix-only: Windows exposes no ctime through `std::fs`, and the rename tests
/// take the `detects_renames == false` branch there (or the USN source, which
/// reports renames without ctime).
#[cfg(unix)]
fn ensure_ctime_after_marker(path: &Path, marker_ns: i64) {
    use std::os::unix::fs::MetadataExt;

    let mut attempts: u32 = 0;
    loop {
        let meta = fs::metadata(path).unwrap();
        let ctime_ns = meta
            .ctime()
            .saturating_mul(1_000_000_000)
            .saturating_add(meta.ctime_nsec());
        if ctime_ns > marker_ns {
            return;
        }
        attempts += 1;
        assert!(
            attempts < 100,
            "rename at {} never moved ctime past the crawl marker \
             (ctime={ctime_ns}, marker={marker_ns})",
            path.display()
        );
        let mode = meta.permissions();
        fs::set_permissions(path, mode).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
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
    let marker_ns = recorded_marker_ns(&db);

    let new = write_file(&d, "new_report.md", "after");
    // The new file must land on the live side: stamp its mtime past the marker
    // explicitly instead of sleeping and hoping the kernel clock moved.
    stamp_after_marker(&new, marker_ns);

    let outcome = fresh::find(&find_config(&db, "report"), None).unwrap();
    let n = names(&outcome);
    assert!(n.contains(&"new_report.md".to_string()), "{n:?}");
    assert!(n.contains(&"old_report.md".to_string()), "{n:?}");
    assert_eq!(origin_of(&outcome, "new_report.md"), Some(HitOrigin::Live));
    // old_report.md must stay on the index side, and it will: it was written
    // before the crawl and is never metadata-touched afterwards (ctime trap),
    // so it cannot enter the delta set.
    assert_eq!(origin_of(&outcome, "old_report.md"), Some(HitOrigin::Index));
    assert!(outcome.stale.is_none(), "{:?}", outcome.stale);
}

#[test]
fn metadata_search_drops_a_file_deleted_after_indexing() {
    let (d, db, _) = tmp_dirs("meta_delete");
    write_file(&d, "keep_report.md", "a");
    let gone = write_file(&d, "gone_report.md", "b");
    crawl(&d, &db);

    // keep_report.md was written before the crawl and is never metadata-touched
    // afterwards (ctime trap: any utimensat/chmod bumps ctime, and the unix
    // source reads `ctime > marker` as changed). Its pre-crawl timestamps are
    // already safely below the marker — the marker is recorded after those
    // writes complete — so on the mtime path `delta.entries == 0` below stays
    // strict.
    fs::remove_file(&gone).unwrap();

    let outcome = fresh::find(&find_config(&db, "report"), None).unwrap();
    assert_eq!(names(&outcome), vec!["keep_report.md".to_string()]);
    assert_eq!(outcome.dropped_deleted, 1);
    // The deletion must reach the answer whichever source ran. An mtime walk
    // cannot see a file that is no longer there, so `dropped_deleted` comes
    // from the merge's existence check and the delta set is empty. The USN
    // journal *does* record the deletion, so its delta set legitimately holds
    // one entry and `dropped_deleted` comes from that entry instead — the
    // strict empty-set assertion therefore applies to the mtime kind only,
    // mirroring how `usn_is_the_default_on_windows_with_mtime_fallback` in
    // delta.rs tolerates both sources.
    match &outcome.delta {
        Some(d) if d.kind == DeltaSourceKind::Mtime => assert_eq!(d.entries, 0),
        Some(d) => assert_eq!(d.entries, 1, "{}", delta_note(&outcome)),
        None => {}
    }
}

#[test]
fn metadata_search_returns_fresh_size_for_a_file_changed_after_indexing() {
    let (d, db, _) = tmp_dirs("meta_change");
    write_file(&d, "notes_report.md", "short");
    crawl(&d, &db);
    let marker_ns = recorded_marker_ns(&db);

    let p = write_file(&d, "notes_report.md", &"x".repeat(4096));
    // The file must look *changed* since the marker: overwriting alone leaves
    // its timestamp to the kernel's coarse clock, so stamp it explicitly.
    stamp_after_marker(&p, marker_ns);

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
    // Rename detection on unix rides on st_ctime, which no API can set
    // explicitly, so make the precondition *verified* instead of assumed:
    // after the rename, ensure the observed ctime is past the marker (bounded
    // retry — see `ensure_ctime_after_marker`). On Windows the `stat` source
    // cannot see the rename at all (the `detects_renames` branch below), so
    // there is nothing to verify there.
    #[cfg(unix)]
    let marker_ns = recorded_marker_ns(&db);

    fs::rename(d.join("draft_report.md"), d.join("final_report.md")).unwrap();
    #[cfg(unix)]
    ensure_ctime_after_marker(&d.join("final_report.md"), marker_ns);

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
    // With the merge disabled below, no delta walk runs at all, so the rename's
    // timestamps never enter the picture — the dropped old path is the merge's
    // existence check, not a timestamp comparison.
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
    let marker_ns = recorded_marker_ns(&db);

    let b = write_file(&d, "b.md", "新しい文書にも索引という語を書いた。\n");
    stamp_after_marker(&b, marker_ns);

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
    let marker_ns = recorded_marker_ns(&db);

    // The term the index knows about is removed; a new one takes its place.
    let doc = write_file(
        &d,
        "doc.md",
        "この文書は書き換えられ、鮮度という語になった。\n",
    );
    // doc.md must enter the delta set as *changed*; other.md must not (its
    // absence is what makes `dropped_changed == 1` below), so other.md is never
    // metadata-touched after the crawl (ctime trap) and doc.md is stamped
    // explicitly.
    stamp_after_marker(&doc, marker_ns);

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

    // keep.md stays untouched after the crawl (ctime trap — see the metadata
    // delete test); the deletion is caught by the merge's existence check, and
    // the delta walk cannot see a file that no longer exists.
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
    // Same verified precondition as the metadata rename test: on unix the new
    // path must be seen via ctime, which is verified (bounded retry) rather
    // than assumed; on Windows the `detects_renames` branch below decides.
    #[cfg(unix)]
    let marker_ns = recorded_marker_ns(&db);

    fs::rename(d.join("before.md"), d.join("after.md")).unwrap();
    #[cfg(unix)]
    ensure_ctime_after_marker(&d.join("after.md"), marker_ns);

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
    let marker_ns = recorded_marker_ns(&db);

    let wanted = write_file(&d, "wanted.md", "索引についての新しいメモ。\n");
    let unwanted = write_file(&d, "unwanted.md", "索引と鮮度の両方に触れたメモ。\n");
    // Both new files must reach the live side: wanted.md to be returned, and
    // unwanted.md to be *excluded by the negation* — a file that never made it
    // into the delta set would pass the `!contains` assertion vacuously.
    stamp_after_marker(&wanted, marker_ns);
    stamp_after_marker(&unwanted, marker_ns);

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
    let marker_ns = recorded_marker_ns(&db);

    // More changed files than the whole page: without a reservation the ranked
    // index result would vanish behind them. All six must be in the delta set
    // for the live half of the page to fill, so each is stamped explicitly.
    for i in 0..6 {
        let p = write_file(&d, &format!("changed_{i}_report.md"), "y");
        stamp_after_marker(&p, marker_ns);
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
    let marker_ns = recorded_marker_ns(&db);

    for i in 0..4 {
        let p = write_file(&d, &format!("changed_{i}_report.md"), "y");
        stamp_after_marker(&p, marker_ns);
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

    // The index was told to accept `.obj`, which is on the built-in binary
    // denylist. That instruction is recorded in the index (issue #15).
    let mut build = FulltextConfig::new(db_path(&db), &ft);
    build.heap_bytes = 16 * 1024 * 1024;
    build.threads = 2;
    build.text_policy.add_text_exts(&["obj".to_string()]);
    fulltext::build(&build).unwrap();
    let marker_ns = recorded_marker_ns(&db);

    let custom = write_file(&d, "custom.obj", "書き換えたが索引という語は残す。\n");
    stamp_after_marker(&custom, marker_ns);

    // A search told nothing inherits it, and the live side refreshes the file.
    //
    // This is the whole point of recording the policy. Before it was recorded,
    // a search that had not been handed `--ext obj` judged the changed file as
    // binary, dropped the index hit as stale and put nothing in its place — so
    // editing a file **removed it from the answer**. The earlier version of
    // this test asserted that disappearance as if it were the specification.
    let inherited = fresh::search(&search_config(&db, &ft, "索引"), None).unwrap();
    // Asserted before the hits: these hold whatever the query matches, so a
    // tokenizer that cannot analyze the query (the offline-dictionary case)
    // fails below on the hit list rather than hiding which half broke.
    assert_eq!(inherited.text_policy.text_exts(), ["obj"]);
    assert!(
        inherited.text_policy_notice.is_none(),
        "inheriting the index's own rule is not something to warn about"
    );
    assert_eq!(
        inherited.live_read, 1,
        "the live grep must judge `.obj` as text and read its body"
    );
    assert_eq!(names(&inherited), vec!["custom.obj".to_string()]);
    assert_eq!(inherited.hits[0].origin, HitOrigin::Live);
    assert_eq!(inherited.dropped_changed, 1, "replaced, not merely dropped");

    // Saying the same thing again changes nothing and warns about nothing.
    let mut same = search_config(&db, &ft, "索引");
    same.text_policy.add_text_exts(&["obj".to_string()]);
    let same = fresh::search(&same, None).unwrap();
    assert_eq!(names(&same), vec!["custom.obj".to_string()]);
    assert!(same.text_policy_notice.is_none());

    // An explicit *disagreeing* policy still wins — `--ext` has to stay an
    // escape hatch for an index that is already built. `resolve_text_policy`
    // treats any non-empty policy as explicit, so putting `.obj` on the
    // denylist is how a caller says "not for this search". The file then
    // behaves the way it did before the policy was recorded: dropped as
    // changed, with nothing to replace it. The difference is that this is now
    // something the caller asked for, and it is reported.
    let mut overridden = search_config(&db, &ft, "索引");
    overridden.text_policy.add_binary_exts(&["obj".to_string()]);
    let overridden = fresh::search(&overridden, None).unwrap();
    assert!(overridden.hits.is_empty(), "{:?}", names(&overridden));
    assert_eq!(overridden.dropped_changed, 1);
    assert!(
        overridden
            .text_policy_notice
            .as_deref()
            .is_some_and(|n| n.contains("obj")),
        "a live scan judging files differently from the build must say so: {:?}",
        overridden.text_policy_notice
    );
}

// ── 3. Threshold: truncation is reported, not hidden ────────────────────────

#[test]
fn a_delta_set_over_the_cap_is_truncated_and_reported_stale() {
    let (d, db, _) = tmp_dirs("truncate");
    write_file(&d, "seed_report.md", "seed");
    crawl(&d, &db);
    let marker_ns = recorded_marker_ns(&db);

    // Truncation needs at least `delta_limit + 1` files past the marker, so
    // every changed file is stamped explicitly.
    for i in 0..5 {
        let p = write_file(&d, &format!("changed_{i}_report.md"), "new");
        stamp_after_marker(&p, marker_ns);
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
    write_file(&d, "b_report.md", "y");
    // The merge is switched off below, so no delta walk runs and b_report.md's
    // timestamp never enters the picture — there is no marker relation to
    // establish or verify here.

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
    let marker = Store::open(db_path(&db))
        .unwrap()
        .delta_marker()
        .unwrap()
        .unwrap();
    let marker_ns = marker.wall_clock_ns();

    let lib = write_file(&d, "src/lib.rs", "pub fn a() {}");
    write_file(&d, "node_modules/pkg/index.js", "module.exports = 1;");
    write_file(&d, "target/debug/build.log", "noise");
    // Only lib.rs must enter the delta set, so its mtime is stamped past the
    // marker. The two noise files are dropped by the exclusion rules *before*
    // any timestamp comparison, and the `excluded >= 2` counter asserts those
    // rules fired — their mtimes never matter. src/main.rs is pre-crawl and
    // never touched (ctime trap).
    stamp_after_marker(&lib, marker_ns);

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
    // The database is rejected by `skip_paths` *before* any timestamp
    // comparison, so touching it afterwards (exactly what any query would do,
    // and what the lines below do) cannot put it in the delta set — there is no
    // marker relation to establish or verify here.

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
    write_file(&d, "b.txt", "y");

    // The marker below is a USN marker whose fallback timestamp is constructed
    // a full minute in the past: both files compare newer than it by
    // construction, a margin that dwarfs any coarse-clock lag, so neither
    // stamping nor a sleep is needed here.
    //
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
    let marker_ns = recorded_marker_ns(&db);

    let b = write_file(&d, "b_report.md", "y");
    // Both queries must see the new file, so it must be in the delta set for
    // the shared marker: stamp it explicitly.
    stamp_after_marker(&b, marker_ns);

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
    let marker_ns = recorded_marker_ns(&db);

    let b = write_file(&d, "b_report.md", "y");
    // For the pre-recrawl queries the file must be in the delta set (changed
    // since marker1), so its mtime is stamped explicitly.
    stamp_after_marker(&b, marker_ns);

    let cache = DeltaCache::new();
    fresh::find(&find_config(&db, "report"), Some(&cache)).unwrap();
    assert_eq!(cache.stats().misses, 1);

    // An explicit drop forces the next query to ask the source again.
    cache.invalidate();
    let after_invalidate = fresh::find(&find_config(&db, "report"), Some(&cache)).unwrap();
    assert!(!after_invalidate.delta.as_ref().unwrap().cached);
    assert_eq!(cache.stats().misses, 2);

    // The re-crawl records a fresh marker; the post-recrawl query must see an
    // *empty* delta set, so b_report.md has to compare older than that new
    // marker on every axis the source checks (mtime, and ctime on unix). Its
    // stamped mtime (marker1 + 10 s) is ahead of any marker recorded a moment
    // later, so first re-stamp it just below the current wall clock. This
    // metadata touch happens *before* the re-crawl, which is exactly what the
    // ctime trap allows. Then make the precondition *verified* instead of
    // assumed, with the same bounded-retry discipline as the rename tests:
    // re-crawl until the recorded marker is past b_report.md's mtime and (on
    // unix) its ctime is not past it. Each iteration is a real crawl over the
    // same two files; the cache was populated before the crawls, so
    // marker_invalidations stays 1 however many land.
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    let b_mtime_ns = now_ns - 1_000_000_000;
    set_mtime_ns(&b, b_mtime_ns);

    let mut marker2_ns = None;
    for _ in 0..100 {
        crawl(&d, &db);
        let m2 = recorded_marker_ns(&db);
        // mtime must not be past the marker (b_mtime_ns < m2, i.e. mtime ≤
        // marker2 — the source's `>` comparison then says "unchanged").
        let mtime_ok = m2 > b_mtime_ns;
        // ctime (unix) is kernel-assigned at the re-stamp and must not be past
        // the marker either; each retry records a later marker, so a lagging
        // coarse clock clears on the next iteration. Windows exposes no ctime
        // through `std::fs` and the stat source never consults it.
        #[cfg(unix)]
        let ctime_ok = {
            use std::os::unix::fs::MetadataExt;
            let meta = fs::metadata(&b).unwrap();
            let ctime_ns = meta
                .ctime()
                .saturating_mul(1_000_000_000)
                .saturating_add(meta.ctime_nsec());
            ctime_ns <= m2
        };
        #[cfg(not(unix))]
        let ctime_ok = true;
        if mtime_ok && ctime_ok {
            marker2_ns = Some(m2);
            break;
        }
    }
    marker2_ns.expect(
        "100 re-crawls never recorded a marker after b_report.md's re-stamped \
         mtime/ctime; the post-recrawl empty-delta precondition cannot be established",
    );

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

    // This is a marker-vs-marker relation: no file timestamp is involved, so
    // nothing needs stamping — but the wall clock can step, so "the re-crawl
    // records a strictly later marker" is *verified* rather than assumed.
    // Re-crawl until the recorded marker is past the first (bounded); each
    // iteration is a real crawl over the same single file.
    let mut second = None;
    for _ in 0..50 {
        crawl(&d, &db);
        let store = Store::open(db_path(&db)).unwrap();
        let m = store.delta_marker().unwrap().unwrap();
        if m.wall_clock_ns() > first.wall_clock_ns() {
            second = Some(m);
            break;
        }
    }
    let second = second.expect("50 re-crawls never moved the delta marker past the first");
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
