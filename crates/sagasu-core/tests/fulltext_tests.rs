//! Integration tests for the full-text stage: body extraction, tantivy +
//! Lindera indexing, and search.
//!
//! Every test builds a small corpus in a temporary directory, crawls it into a
//! SQLite metadata index, builds a full-text index from that, and searches.
//! Temporary directories live under `std::env::temp_dir()` and are keyed by
//! test name so the tests can run in parallel.

use std::fs;
use std::path::{Path, PathBuf};

use sagasu_core::fulltext::{
    self, FulltextConfig, FulltextSummary, SearchConfig, SearchOutcome, SkipReason,
};
use sagasu_core::store::Store;
use sagasu_core::walk::{self, CrawlConfig};

// ── helpers ─────────────────────────────────────────────────────────────────

/// Create a temporary working area. Returns (data_dir, db_dir, index_dir).
/// The database and the tantivy index both live outside the crawled tree so the
/// walker never sees their files.
fn tmp_dirs(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("sagasu_ft_{}_{}", name, std::process::id()));
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

fn write_bytes(dir: &Path, rel: &str, content: &[u8]) -> PathBuf {
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

/// Full-text config with a writer budget small enough that many tests can run
/// in parallel (tantivy needs ≥15 MB per writer thread, so this ends up with a
/// single writer thread).
fn ft_config(db_dir: &Path, index_dir: &Path) -> FulltextConfig {
    FulltextConfig {
        db_path: db_path(db_dir),
        index_dir: index_dir.to_path_buf(),
        max_size: fulltext::DEFAULT_MAX_SIZE,
        text_policy: Default::default(),
        no_sniff: false,
        threads: 2,
        heap_bytes: 16 * 1024 * 1024,
    }
}

fn build_ft(db_dir: &Path, index_dir: &Path) -> FulltextSummary {
    fulltext::build(&ft_config(db_dir, index_dir)).unwrap()
}

/// Crawl + build in one step (the common setup).
fn index_all(data: &Path, db_dir: &Path, index_dir: &Path) -> FulltextSummary {
    crawl(data, db_dir);
    build_ft(db_dir, index_dir)
}

fn search(index_dir: &Path, query: &str) -> SearchOutcome {
    fulltext::search(&SearchConfig::new(index_dir, query)).unwrap()
}

fn search_with_db(index_dir: &Path, db_dir: &Path, query: &str) -> SearchOutcome {
    let mut config = SearchConfig::new(index_dir, query);
    config.db_path = Some(db_path(db_dir));
    fulltext::search(&config).unwrap()
}

/// Basenames of the hits, for readable assertions.
fn hit_names(outcome: &SearchOutcome) -> Vec<String> {
    outcome
        .hits
        .iter()
        .map(|h| {
            Path::new(h.display_path())
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect()
}

/// A corpus split so that exactly one document contains both the Japanese and
/// the English term. That makes AND queries deterministic.
fn write_mixed_corpus(data: &Path) {
    write_file(
        data,
        "ja_only.md",
        "# 設計メモ\n\nこのツールは日本語の文章を形態素解析してから索引する。\
         全文検索エンジンとしての精度はここで決まる。\n",
    );
    write_file(
        data,
        "en_only.md",
        "# Design note\n\nThe indexing layer is built on tantivy, a Lucene-class \
         library written in Rust.\n",
    );
    write_file(
        data,
        "mixed.md",
        "# 混在メモ\n\ntantivy に日本語を食わせる。\n",
    );
    write_file(
        data,
        "unrelated.txt",
        "curry recipe: onion, carrot, potato, garam masala\n",
    );
}

// ── 1. Mixed Japanese/English query ────────────────────────────────────────

#[test]
fn mixed_japanese_english_query_returns_expected_hits() {
    let (data, db, index) = tmp_dirs("mixed_query");
    write_mixed_corpus(&data);

    let summary = index_all(&data, &db, &index);
    assert_eq!(summary.indexed, 4, "all four text files should be indexed");

    // OR semantics: every document carrying either term comes back, and the one
    // carrying both ranks first.
    let outcome = search(&index, "日本語 tantivy");
    let names = hit_names(&outcome);
    assert_eq!(
        names.first().map(String::as_str),
        Some("mixed.md"),
        "the document with both terms must rank first: {names:?}"
    );
    assert!(names.contains(&"ja_only.md".to_string()), "{names:?}");
    assert!(names.contains(&"en_only.md".to_string()), "{names:?}");
    assert!(
        !names.contains(&"unrelated.txt".to_string()),
        "a document with neither term must not match: {names:?}"
    );

    // AND semantics: exactly the one document.
    let strict = search(&index, "日本語 AND tantivy");
    assert_eq!(hit_names(&strict), vec!["mixed.md".to_string()]);
}

// ── 2. Lindera actually segments Japanese ──────────────────────────────────

#[test]
fn japanese_is_segmented_by_lindera_not_treated_as_one_token() {
    let (data, db, index) = tmp_dirs("lindera_seg");
    write_file(
        data.as_path(),
        "doc.md",
        "本ツールは全文検索エンジンである。形態素解析器には Lindera を採用した。\n",
    );

    index_all(&data, &db, &index);

    // "検索" appears only inside the compound 全文検索エンジン. A whitespace or
    // alphanumeric-run tokenizer would index the whole sentence as one token and
    // return nothing here; Lindera splits it, so this must hit.
    let outcome = search(&index, "検索");
    assert_eq!(hit_names(&outcome), vec!["doc.md".to_string()]);

    // Same for a compound the query splits differently from the document.
    let outcome = search(&index, "検索エンジン");
    assert_eq!(hit_names(&outcome), vec!["doc.md".to_string()]);

    // A word that is not in the document must not match.
    let outcome = search(&index, "冷蔵庫");
    assert!(outcome.hits.is_empty(), "{:?}", hit_names(&outcome));
}

// ── 3. English matching is case-insensitive ────────────────────────────────

#[test]
fn english_matching_is_case_insensitive() {
    let (data, db, index) = tmp_dirs("case_fold");
    write_file(
        &data,
        "code.rs",
        "use std::collections::BTreeMap;\n// 索引には BTreeMap を使う\n",
    );

    index_all(&data, &db, &index);

    for query in ["BTreeMap", "btreemap", "BTREEMAP", "btreemap AND 索引"] {
        let outcome = search(&index, query);
        assert_eq!(
            hit_names(&outcome),
            vec!["code.rs".to_string()],
            "query {query:?} should match regardless of case"
        );
    }
}

// ── 4. The ESM/TSX extension family is indexed (issue #15 regression) ──────

#[test]
fn esm_and_tsx_extensions_are_indexed() {
    let (data, db, index) = tmp_dirs("esm_exts");
    for (i, ext) in ["mjs", "cjs", "jsx", "tsx", "mts", "cts"]
        .iter()
        .enumerate()
    {
        write_file(
            &data,
            &format!("mod{i}.{ext}"),
            "export const marker = 'ジンバブエ';\n",
        );
    }

    let summary = index_all(&data, &db, &index);
    assert_eq!(
        summary.indexed, 6,
        "every member of the ESM/TSX family must be indexed, not silently dropped"
    );
    assert_eq!(
        summary.accepted_by_ext, 6,
        "they should be accepted by the allowlist, without sniffing"
    );
    assert!(summary.skipped.is_empty(), "{:?}", summary.skipped);

    let outcome = search(&index, "ジンバブエ");
    assert_eq!(outcome.hits.len(), 6, "{:?}", hit_names(&outcome));
}

// ── 5. Dot directories are indexed (issue #14 regression) ──────────────────

#[test]
fn dot_directories_are_indexed() {
    let (data, db, index) = tmp_dirs("dot_dirs");
    write_file(
        &data,
        ".github/workflows/ci.yml",
        "name: ワークフロー定義\n",
    );
    write_file(
        &data,
        ".opencode/notes.md",
        "エージェントのルールをここに書く\n",
    );
    write_file(&data, ".config/app.toml", "[section]\nkey = \"ルール\"\n");
    write_file(&data, "readme.md", "通常のファイル\n");

    let summary = index_all(&data, &db, &index);
    assert_eq!(
        summary.indexed, 4,
        "dot directories hold configuration users want to find — they must not be \
         excluded the way a VCS ignore rule would"
    );

    let outcome = search(&index, "ルール");
    let names = hit_names(&outcome);
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"notes.md".to_string()), "{names:?}");
    assert!(names.contains(&"app.toml".to_string()), "{names:?}");
}

// ── 6. Build artefacts stay out (one exclusion set, inherited from M0) ─────

#[test]
fn build_artifacts_are_not_indexed() {
    let (data, db, index) = tmp_dirs("artifacts");
    write_file(&data, "src/main.rs", "// マーカー語 ホウレンソウ\n");
    write_file(
        &data,
        "node_modules/pkg/index.js",
        "// マーカー語 ホウレンソウ\n",
    );
    write_file(
        &data,
        "target/debug/build.rs",
        "// マーカー語 ホウレンソウ\n",
    );
    write_file(&data, "__pycache__/mod.py", "# マーカー語 ホウレンソウ\n");

    let summary = index_all(&data, &db, &index);
    assert_eq!(
        summary.candidates, 1,
        "the crawler already dropped the artefacts, so they never reach this stage"
    );
    assert_eq!(summary.indexed, 1);

    let outcome = search(&index, "ホウレンソウ");
    assert_eq!(hit_names(&outcome), vec!["main.rs".to_string()]);
}

// ── 7. Extensionless text is picked up by sniffing ─────────────────────────

#[test]
fn extensionless_text_is_indexed_by_sniffing() {
    let (data, db, index) = tmp_dirs("sniff_text");
    write_file(&data, "Makefile", "all:\n\techo タラバガニ\n");
    write_file(
        &data,
        "LICENSE",
        "Permission is hereby granted, タラバガニ\n",
    );
    write_file(
        &data,
        "notes.wat",
        "unknown extension, still text: タラバガニ\n",
    );

    let summary = index_all(&data, &db, &index);
    assert_eq!(summary.indexed, 3);
    assert_eq!(
        summary.accepted_by_sniff, 3,
        "none of these are on the allowlist; sniffing is what saves them"
    );
    assert_eq!(summary.accepted_by_ext, 0);

    let outcome = search(&index, "タラバガニ");
    assert_eq!(outcome.hits.len(), 3, "{:?}", hit_names(&outcome));
}

// ── 8. Binary content is skipped, with a reason ────────────────────────────

#[test]
fn binary_content_is_skipped_with_a_reason() {
    let (data, db, index) = tmp_dirs("binary_skip");
    write_file(&data, "keep.md", "テキストは索引される\n");
    // Unknown extension + NUL bytes → rejected by sniffing.
    write_bytes(&data, "blob.dat", &[0x00, 0x01, 0x02, 0xFF, 0x00, 0x10]);
    // Known-binary extensions → rejected without opening the file.
    write_bytes(&data, "report.pdf", b"%PDF-1.7 not really a pdf");
    write_bytes(&data, "sheet.xlsx", b"PK\x03\x04 not really an xlsx");
    write_bytes(&data, "photo.png", b"\x89PNG\r\n\x1a\n");

    let summary = index_all(&data, &db, &index);
    assert_eq!(summary.indexed, 1);
    assert_eq!(summary.candidates, 5);
    assert_eq!(
        summary.skipped.get(&SkipReason::BinaryContent).copied(),
        Some(1),
        "{:?}",
        summary.skipped
    );
    assert_eq!(
        summary.skipped.get(&SkipReason::UnsupportedExt).copied(),
        Some(3),
        "PDF/Office/media are out of M1 scope but must still be counted: {:?}",
        summary.skipped
    );
    assert_eq!(summary.skipped_total(), 4);
    assert_eq!(
        summary.candidates,
        summary.indexed + summary.skipped_total(),
        "every candidate must be either indexed or explained"
    );
}

// ── 9. The size limit is enforced and counted ──────────────────────────────

#[test]
fn oversized_files_are_skipped_and_counted() {
    let (data, db, index) = tmp_dirs("too_large");
    write_file(&data, "small.md", "小さい\n");
    write_file(&data, "big.md", &"大きい ".repeat(2000));

    crawl(&data, &db);
    let mut config = ft_config(&db, &index);
    config.max_size = 1024;
    let summary = fulltext::build(&config).unwrap();

    assert_eq!(summary.indexed, 1);
    assert_eq!(
        summary.skipped.get(&SkipReason::TooLarge).copied(),
        Some(1),
        "{:?}",
        summary.skipped
    );
    assert_eq!(
        hit_names(&search(&index, "小さい")),
        vec!["small.md".to_string()]
    );
}

// ── 10. Empty files are counted, not silently dropped ──────────────────────

#[test]
fn empty_files_are_counted() {
    let (data, db, index) = tmp_dirs("empty_files");
    write_file(&data, "has_body.md", "中身あり\n");
    write_file(&data, "zero.md", "");
    write_file(&data, "whitespace.md", "   \n\n\t\n");

    let summary = index_all(&data, &db, &index);
    assert_eq!(summary.indexed, 1);
    assert_eq!(
        summary.skipped.get(&SkipReason::Empty).copied(),
        Some(2),
        "{:?}",
        summary.skipped
    );
}

// ── 11. An all-skipped corpus reports zero, with reasons ───────────────────

#[test]
fn corpus_with_no_text_reports_zero_indexed_and_reasons() {
    let (data, db, index) = tmp_dirs("all_skipped");
    write_bytes(&data, "a.png", b"\x89PNG\r\n\x1a\n");
    write_bytes(&data, "b.zip", b"PK\x03\x04");

    let summary = index_all(&data, &db, &index);
    assert_eq!(summary.indexed, 0);
    assert!(
        !summary.skipped.is_empty(),
        "a zero-document build must still explain itself"
    );
    assert_eq!(summary.skipped_total(), 2);
    // The core returns Ok; warning and non-zero exit are the CLI's job.
    assert_eq!(summary.candidates, 2);
}

// ── 12. Documents carry the stable schema-v0 file_id ───────────────────────

#[test]
fn hits_carry_the_stable_file_id() {
    let (data, db, index) = tmp_dirs("file_id_link");
    write_file(&data, "linked.md", "識別子の連携を確認する\n");

    index_all(&data, &db, &index);

    let outcome = search(&index, "識別子");
    assert_eq!(outcome.hits.len(), 1);
    let hit = &outcome.hits[0];

    let store = Store::open(db_path(&db)).unwrap();
    let expected: i64 = store
        .conn()
        .query_row(
            "SELECT file_id FROM files WHERE path LIKE '%linked.md' AND deleted_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        hit.file_id, expected,
        "the tantivy document must carry the SQLite file_id"
    );
    assert!(store.find_by_file_id(hit.file_id).unwrap().is_some());
}

// ── 13. A rename after the build resolves through the file_id ──────────────

#[test]
fn rename_after_build_resolves_to_the_current_path() {
    let (data, db, index) = tmp_dirs("rename_resolve");
    let original = write_file(&data, "before.md", "移動しても引ける文章\n");

    index_all(&data, &db, &index);

    // Move the file and refresh only the metadata index — the full-text index is
    // deliberately left stale.
    let moved = data.join("sub/after.md");
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::rename(&original, &moved).unwrap();
    crawl(&data, &db);

    let outcome = search_with_db(&index, &db, "移動");
    assert_eq!(outcome.hits.len(), 1);
    let hit = &outcome.hits[0];
    assert!(hit.indexed_path.ends_with("before.md"), "{hit:?}");
    assert!(
        hit.current_path
            .as_deref()
            .is_some_and(|p| p.ends_with("after.md")),
        "a stale full-text hit must resolve to the current path via file_id: {hit:?}"
    );
    assert!(hit.display_path().ends_with("after.md"));
    assert!(!hit.deleted);

    // Without the database the hit still comes back, just with the stale path.
    let bare = search(&index, "移動");
    assert!(bare.hits[0].current_path.is_none());
    assert!(bare.hits[0].display_path().ends_with("before.md"));
}

// ── 14. A deletion after the build is flagged, not silently returned ───────

#[test]
fn deleted_file_is_flagged_in_results() {
    let (data, db, index) = tmp_dirs("deleted_flag");
    let doomed = write_file(&data, "doomed.md", "消えた後も台帳には残る\n");
    write_file(&data, "alive.md", "こちらは残る\n");

    index_all(&data, &db, &index);

    fs::remove_file(&doomed).unwrap();
    crawl(&data, &db);

    let outcome = search_with_db(&index, &db, "台帳");
    assert_eq!(outcome.hits.len(), 1);
    assert!(
        outcome.hits[0].deleted,
        "a tombstoned file must be flagged rather than returned as if it were live"
    );
}

// ── 15. Snippets show the match ────────────────────────────────────────────

#[test]
fn snippet_shows_the_matched_region() {
    let (data, db, index) = tmp_dirs("snippet");
    write_file(
        &data,
        "long.md",
        &format!(
            "{}\n目的の一文はこの位置にある。合言葉はサツマイモ。\n{}",
            "前置きの段落。".repeat(40),
            "後書きの段落。".repeat(40)
        ),
    );

    index_all(&data, &db, &index);

    let outcome = search(&index, "サツマイモ");
    assert_eq!(outcome.hits.len(), 1);
    let snippet = &outcome.hits[0].snippet;
    assert!(
        snippet.contains("サツマイモ"),
        "the snippet should be taken from around the match, not the file head: {snippet:?}"
    );
    assert!(
        !snippet.contains('\n'),
        "the snippet must stay on one line: {snippet:?}"
    );
}

// ── 16. Results are ordered by score ───────────────────────────────────────

#[test]
fn results_are_ordered_by_descending_score() {
    let (data, db, index) = tmp_dirs("score_order");
    write_file(&data, "dense.md", &"検索 ".repeat(50));
    write_file(&data, "sparse.md", &format!("検索 {}", "余談 ".repeat(200)));

    index_all(&data, &db, &index);

    let outcome = search(&index, "検索");
    assert_eq!(outcome.hits.len(), 2);
    assert!(
        outcome.hits[0].score >= outcome.hits[1].score,
        "hits must come back in descending score order"
    );
    assert_eq!(
        hit_names(&outcome)[0],
        "dense.md",
        "the denser document should rank first"
    );
}

// ── 17. --no-sniff narrows the corpus to the allowlist ─────────────────────

#[test]
fn no_sniff_limits_indexing_to_the_extension_allowlist() {
    let (data, db, index) = tmp_dirs("no_sniff");
    write_file(&data, "listed.md", "許可リストにある\n");
    write_file(&data, "Makefile", "all:\n\techo 許可リストにない\n");

    crawl(&data, &db);
    let mut config = ft_config(&db, &index);
    config.no_sniff = true;
    let summary = fulltext::build(&config).unwrap();

    assert_eq!(summary.indexed, 1);
    assert_eq!(summary.accepted_by_sniff, 0);
    assert_eq!(
        summary.skipped.get(&SkipReason::UnsupportedExt).copied(),
        Some(1),
        "{:?}",
        summary.skipped
    );
}

// ── 18. --ext extends the allowlist ────────────────────────────────────────

#[test]
fn extra_extensions_extend_the_allowlist() {
    let (data, db, index) = tmp_dirs("extra_ext");
    // `.obj` is on the binary denylist, so without --ext it is skipped outright.
    write_bytes(&data, "model.obj", "v 0.0 0.0 0.0\n# コメント\n".as_bytes());

    crawl(&data, &db);

    let baseline = fulltext::build(&ft_config(&db, &index)).unwrap();
    assert_eq!(baseline.indexed, 0);
    assert_eq!(
        baseline.skipped.get(&SkipReason::UnsupportedExt).copied(),
        Some(1)
    );

    let mut config = ft_config(&db, &index);
    config.text_policy.add_text_exts(&["obj".to_string()]);
    let extended = fulltext::build(&config).unwrap();
    assert_eq!(extended.indexed, 1, "--ext must win over the denylist");
    assert_eq!(
        hit_names(&search(&index, "コメント")),
        vec!["model.obj".to_string()]
    );
}

// ── 19. Rebuilding refuses to erase an unrelated directory ─────────────────

#[test]
fn rebuild_refuses_a_directory_that_is_not_an_index() {
    let (data, db, index) = tmp_dirs("guard_dir");
    write_file(&data, "a.md", "本文\n");
    crawl(&data, &db);

    // A mistyped --index-dir pointing at real documents must not be wiped.
    fs::create_dir_all(&index).unwrap();
    fs::write(index.join("important.txt"), "do not delete me").unwrap();

    let err = fulltext::build(&ft_config(&db, &index)).unwrap_err();
    assert!(
        format!("{err:#}").contains("not a tantivy index"),
        "unexpected error: {err:#}"
    );
    assert!(
        index.join("important.txt").exists(),
        "the guard must leave the directory untouched"
    );
}

// ── 20. Building without a prior crawl fails loudly ────────────────────────

#[test]
fn build_without_a_crawl_is_an_error() {
    let (_data, db, index) = tmp_dirs("no_crawl");

    let err = fulltext::build(&ft_config(&db, &index)).unwrap_err();
    assert!(
        format!("{err:#}").contains("no crawl recorded"),
        "unexpected error: {err:#}"
    );
}

// ── 21. Searching a missing index says what to do ──────────────────────────

#[test]
fn searching_a_missing_index_is_an_error() {
    let (_data, _db, index) = tmp_dirs("missing_index");

    let err = fulltext::search(&SearchConfig::new(&index, "何か")).unwrap_err();
    assert!(
        format!("{err:#}").contains("sagasu fulltext"),
        "unexpected error: {err:#}"
    );
}

// ── 22. Rebuilding is idempotent and reflects new files ────────────────────

#[test]
fn rebuild_reflects_added_and_removed_files() {
    let (data, db, index) = tmp_dirs("rebuild");
    write_file(&data, "first.md", "最初の文章\n");

    let s1 = index_all(&data, &db, &index);
    assert_eq!(s1.indexed, 1);
    assert_eq!(search(&index, "最初").hits.len(), 1);

    write_file(&data, "second.md", "二番目の文章\n");
    let s2 = index_all(&data, &db, &index);
    assert_eq!(s2.indexed, 2, "a rebuild must pick up new files");
    assert_eq!(search(&index, "文章").hits.len(), 2);

    fs::remove_file(data.join("first.md")).unwrap();
    let s3 = index_all(&data, &db, &index);
    assert_eq!(s3.indexed, 1, "a rebuild must drop tombstoned files");
    assert!(search(&index, "最初").hits.is_empty());
}

// ── 23. The build records its link to the metadata index ───────────────────

#[test]
fn build_records_fulltext_state_in_the_metadata_index() {
    let (data, db, index) = tmp_dirs("ft_meta");
    write_file(&data, "a.md", "状態の記録\n");

    let summary = index_all(&data, &db, &index);

    let store = Store::open(db_path(&db)).unwrap();
    let stats = store.get_stats().unwrap();
    assert_eq!(stats.fulltext_docs, Some(summary.indexed as i64));
    assert_eq!(
        stats.fulltext_scan_generation,
        Some(stats.scan_generation),
        "a freshly built full-text index is at the current scan generation"
    );
    assert!(stats.fulltext_dir.is_some());

    // A further crawl leaves the full-text index behind, and that is visible.
    crawl(&data, &db);
    let stats2 = Store::open(db_path(&db)).unwrap().get_stats().unwrap();
    assert!(
        stats2.fulltext_scan_generation.unwrap() < stats2.scan_generation,
        "the full-text index should now read as one generation behind"
    );
}

// ── 24. A failed rebuild does not leave the old state claimed ──────────────

#[test]
fn failed_rebuild_clears_the_recorded_fulltext_state() {
    let (data, db, index) = tmp_dirs("failed_rebuild");
    write_file(&data, "a.md", "本文\n");
    index_all(&data, &db, &index);
    assert!(Store::open(db_path(&db))
        .unwrap()
        .get_stats()
        .unwrap()
        .fulltext_dir
        .is_some());

    // Make the rebuild fail: point --index-dir at a file.
    let blocked = index.parent().unwrap().join("blocked");
    fs::write(&blocked, "not a directory").unwrap();
    let mut config = ft_config(&db, &index);
    config.index_dir = blocked;
    assert!(fulltext::build(&config).is_err());

    let stats = Store::open(db_path(&db)).unwrap().get_stats().unwrap();
    assert!(
        stats.fulltext_dir.is_none() && stats.fulltext_docs.is_none(),
        "a failed build must not leave the previous index still claimed: {stats:?}"
    );
}

// ── 25. Phrase and negation queries work ───────────────────────────────────

#[test]
fn phrase_and_negation_queries_work() {
    let (data, db, index) = tmp_dirs("query_syntax");
    write_file(&data, "hit.md", "形態素解析の話をする\n");
    write_file(&data, "miss.md", "解析はするが形態素ではない話\n");

    index_all(&data, &db, &index);

    // Phrase: adjacency matters, so only the document with the exact sequence.
    let phrase = search(&index, "\"形態素解析\"");
    assert_eq!(hit_names(&phrase), vec!["hit.md".to_string()]);

    // Negation narrows an OR query.
    let negated = search(&index, "解析 -形態素");
    let names = hit_names(&negated);
    assert!(!names.contains(&"hit.md".to_string()), "{names:?}");
}

// ── 26. Reported sizes are plausible ───────────────────────────────────────

#[test]
fn summary_reports_text_and_index_sizes() {
    let (data, db, index) = tmp_dirs("sizes");
    for i in 0..50 {
        write_file(
            &data,
            &format!("doc{i}.md"),
            &format!("# 文書 {i}\n\n{}\n", "全文検索の索引を作る。".repeat(20)),
        );
    }

    let summary = index_all(&data, &db, &index);
    assert_eq!(summary.indexed, 50);
    assert!(summary.text_bytes > 0, "extracted text should be counted");
    assert!(
        summary.index_bytes > 0,
        "the index directory should have a size"
    );
    assert!(summary.elapsed_secs >= 0.0);
}

// ── 27. A plain-text file off the allowlist still gets indexed (#15) ───────

#[test]
fn plain_text_off_the_allowlist_is_indexed_by_sniffing_not_dropped() {
    let (data, db, index) = tmp_dirs("off_allowlist");
    // `mjs` used to be missing from a shorter allowlist and 41 files vanished
    // without a word (issue #15). It is on the list now, so it is accepted
    // without opening the file…
    write_file(&data, "app.mjs", "export const 見出し = 'タラバガニ';\n");
    // …and the extensions nobody thought of are still indexed, because the
    // extension is an entrance and the content is the decision.
    write_file(&data, "page.tmpl", "{{ タラバガニ }}\n");
    write_file(&data, "Makefile", "all:\n\techo タラバガニ\n");

    let summary = index_all(&data, &db, &index);
    assert_eq!(summary.indexed, 3, "{:?}", summary.skipped);
    assert_eq!(summary.accepted_by_ext, 1, "only .mjs is on the allowlist");
    assert_eq!(summary.accepted_by_sniff, 2);
    assert!(summary.skipped.is_empty(), "{:?}", summary.skipped);

    // Asserted through the document count rather than a query: whether these
    // three files got a *body* is the claim issue #15 is about, and the
    // Lindera-dependent query path is already covered by the tests above.
    assert_eq!(search(&index, "タラバガニ").total_docs, 3);
}

// ── 28. The skip report names the extensions behind it (#15) ──────────────

#[test]
fn format_skips_are_broken_down_by_extension() {
    let (data, db, index) = tmp_dirs("skip_breakdown");
    write_file(&data, "keep.md", "本文\n");
    for n in 0..3 {
        write_bytes(&data, &format!("a{n}.pdf"), b"%PDF-1.7 not really");
    }
    write_bytes(&data, "photo.png", b"\x89PNG\r\n\x1a\n");
    write_bytes(&data, "blob", &[0x00, 0x01, 0x02, 0xFF]);

    let summary = index_all(&data, &db, &index);
    assert_eq!(summary.indexed, 1);
    assert_eq!(summary.skipped_total(), 5);

    // "5 files skipped" is not actionable; ".pdf: 3" is. Most frequent first,
    // ties by extension, and `""` for a file with no extension at all.
    assert_eq!(
        summary.skipped_exts,
        vec![
            ("pdf".to_string(), 3),
            (String::new(), 1),
            ("png".to_string(), 1),
        ]
    );
}

// ── 29. A text config file extends the lists the same way --ext does (#15) ─

#[test]
fn a_text_config_file_extends_the_allowlist() {
    use sagasu_core::text::TextPolicy;

    let (data, db, index) = tmp_dirs("text_config");
    // `.obj` is denylisted and `.dat` is unknown-and-binary-looking; the config
    // file has to be able to move both.
    write_bytes(&data, "model.obj", "v 0.0\n# タラバガニ\n".as_bytes());
    write_file(&data, "notes.dat", "タラバガニのメモ\n");
    crawl(&data, &db);

    let config_path = db.join("sagasu-text.toml");
    fs::write(
        &config_path,
        "text_ext   = [\"obj\"]\nbinary_ext = [\"dat\"]\n",
    )
    .unwrap();

    let mut config = ft_config(&db, &index);
    config.text_policy = TextPolicy::load(&config_path).unwrap();
    let summary = fulltext::build(&config).unwrap();

    assert_eq!(
        summary.indexed, 1,
        "the config moved .obj onto the allowlist"
    );
    assert_eq!(summary.accepted_by_ext, 1);
    assert_eq!(
        summary.skipped.get(&SkipReason::UnsupportedExt).copied(),
        Some(1),
        "the config moved .dat onto the denylist: {:?}",
        summary.skipped
    );
    assert_eq!(search(&index, "タラバガニ").total_docs, 1);
    assert!(
        config.text_policy.digest().is_some(),
        "the file is digested"
    );
}

// ── 30. The extension rule travels with the index (#15) ───────────────────

#[test]
fn the_index_records_the_extension_policy_it_was_built_with() {
    use sagasu_core::text::TextPolicy;

    let (data, db, index) = tmp_dirs("policy_persist");
    write_bytes(&data, "model.obj", "v 0.0\n# タラバガニ\n".as_bytes());
    write_file(&data, "notes.dat", "メモ\n");
    crawl(&data, &db);

    let config_path = db.join("sagasu-text.toml");
    fs::write(
        &config_path,
        "text_ext   = [\"obj\"]\nbinary_ext = [\"dat\"]\n",
    )
    .unwrap();

    let mut config = ft_config(&db, &index);
    config.text_policy = TextPolicy::load(&config_path).unwrap();
    let summary = fulltext::build(&config).unwrap();
    assert_eq!(summary.indexed, 1);

    // The rule the build used is now the index's, and reading it back needs no
    // config file and no particular working directory. Before this, `sagasu
    // fulltext` picked up ./sagasu-text.toml automatically and a `sagasu
    // search` run from anywhere else did not — so the live grep judged an
    // edited `.obj` as binary and the file vanished from the answer.
    let store = Store::open(db_path(&db)).unwrap();
    let restored = TextPolicy::from_index(&store)
        .unwrap()
        .expect("the build recorded its policy");

    assert_eq!(restored.text_exts(), ["obj"]);
    assert_eq!(restored.binary_exts(), ["dat"]);
    assert!(restored.agrees_with(&config.text_policy));
    assert_eq!(
        restored.digest(),
        config.text_policy.digest(),
        "the digest identifies which config file produced the index"
    );

    // Round-trips by effect, not just by bytes.
    let round_tripped = TextPolicy::decode(&restored.encode()).unwrap();
    assert!(round_tripped.agrees_with(&restored));
    assert_eq!(
        round_tripped.classify_ext(Some("obj")),
        sagasu_core::text::ExtVerdict::Text
    );
    assert_eq!(
        round_tripped.classify_ext(Some("dat")),
        sagasu_core::text::ExtVerdict::Binary
    );
}

// ── 31. A rebuild without the config does not leave the old rule behind ───

#[test]
fn rebuilding_without_a_config_clears_the_recorded_policy() {
    use sagasu_core::text::TextPolicy;

    let (data, db, index) = tmp_dirs("policy_clear");
    write_bytes(&data, "model.obj", "v 0.0\n".as_bytes());
    write_file(&data, "a.md", "本文\n");
    crawl(&data, &db);

    let mut config = ft_config(&db, &index);
    config.text_policy.add_text_exts(&["obj".to_string()]);
    assert_eq!(fulltext::build(&config).unwrap().indexed, 2);

    // Rebuild with the built-in lists only. A stale `text_policy` row would
    // make later searches treat `.obj` as text while the index no longer holds
    // it — the disagreement pointed the other way.
    let plain = ft_config(&db, &index);
    assert_eq!(fulltext::build(&plain).unwrap().indexed, 1);

    let store = Store::open(db_path(&db)).unwrap();
    let restored = TextPolicy::from_index(&store).unwrap().unwrap();
    assert!(restored.is_empty(), "{:?}", restored.text_exts());
}
