//! Integration tests for the rule-based tag engine (issue #4).
//!
//! Every test builds a real tree on disk, crawls it with the real crawler and
//! tags it with the real engine, then asserts against what ended up in SQLite.
//! Nothing here re-implements the expected tags in the test: the assertions are
//! about *properties* the engine promises (determinism, atomicity, magic beating
//! the extension, rules being order-independent) or about a handful of concrete
//! tags a human can check by eye.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sagasu_core::store::Store;
use sagasu_core::tagindex::{self, TagConfig};
use sagasu_core::tagrules::RuleSet;
use sagasu_core::walk::{self, CrawlConfig};

// ── helpers ─────────────────────────────────────────────────────────────────

/// A temporary working area: the tree to crawl, and a database *outside* it.
fn tmp_dirs(name: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("sagasu_tag_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let (data, db) = (base.join("data"), base.join("db"));
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&db).unwrap();
    (data, db)
}

fn db_path(db_dir: &Path) -> PathBuf {
    db_dir.join("test.db")
}

fn write(dir: &Path, rel: &str, content: &str) -> PathBuf {
    write_bytes(dir, rel, content.as_bytes())
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
        threads: 1,
    })
    .unwrap();
}

fn tag(db_dir: &Path, read_magic: bool) -> tagindex::TagSummary {
    tagindex::build(&TagConfig {
        db_path: db_path(db_dir),
        rules_path: None,
        read_magic,
        magic_max_size: u64::MAX,
    })
    .unwrap()
}

fn tag_with_rules(db_dir: &Path, rules: &Path, read_magic: bool) -> tagindex::TagSummary {
    tagindex::build(&TagConfig {
        db_path: db_path(db_dir),
        rules_path: Some(rules.to_path_buf()),
        read_magic,
        magic_max_size: u64::MAX,
    })
    .unwrap()
}

/// Every stored tag, keyed by the file's path relative to `data`.
///
/// This is the snapshot the determinism tests compare. It deliberately goes
/// through the database rather than through the engine, so it also proves the
/// persistence layer is not where a difference could creep in.
fn snapshot(data: &Path, db_dir: &Path) -> BTreeMap<String, Vec<String>> {
    let store = Store::open(db_path(db_dir)).unwrap();
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut cursor = 0i64;
    loop {
        let rows = store.get_live_files_batch(cursor, 1000).unwrap();
        let Some(last) = rows.last() else { break };
        cursor = last.file_id;
        for row in &rows {
            let rel = Path::new(&row.path)
                .strip_prefix(data.canonicalize().unwrap())
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| row.path.clone());
            let list = tagindex::tags_of_file(&store, row.file_id)
                .unwrap()
                .into_iter()
                .map(|(t, _)| t.to_string())
                .collect();
            out.insert(rel, list);
        }
    }
    out
}

/// Build a small tree that exercises every generator at least once.
fn build_corpus(data: &Path) {
    write(
        data,
        "clients/acme-corp/invoice-2024-03-15_v2.txt",
        "amount 100",
    );
    write(data, "clients/acme-corp/proposal_final.md", "# proposal");
    write(data, "clients/beta/notes.txt", "hello");
    write(data, "写真/2024/IMG_4821.txt", "not really a jpeg");
    write(data, "写真/2024/Screenshot 2024-03-15.txt", "shot");
    write(data, "src/main.rs", "fn main() {}");
    write(data, "src/config.toml", "[a]\nb = 1\n");
    write(data, ".gitignore", "target\n");
    write(data, "backup/report.bak", "old text");
}

// ── 1. Determinism ──────────────────────────────────────────────────────────

#[test]
fn re_running_the_whole_pipeline_produces_identical_tags() {
    let (d, db) = tmp_dirs("determinism");
    build_corpus(&d);

    crawl(&d, &db);
    tag(&db, true);
    let first = snapshot(&d, &db);

    // A second crawl bumps the scan generation and re-stats every file; a second
    // tag pass rebuilds the whole layer from scratch. Neither may move a tag.
    crawl(&d, &db);
    tag(&db, true);
    let second = snapshot(&d, &db);

    assert_eq!(first, second, "re-indexing must not disturb any tag");
    assert!(!first.is_empty());
}

#[test]
fn editing_a_files_content_does_not_disturb_its_tags() {
    let (d, db) = tmp_dirs("edit");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db, true);
    let before = snapshot(&d, &db);

    // Same name, different bytes — and the crawl nulls `magic` on a change, so
    // the second pass re-reads it. The tags still must not move: nothing the
    // engine looks at (format, path, name) changed.
    write(
        &d,
        "clients/beta/notes.txt",
        "hello, a good deal more text now",
    );
    crawl(&d, &db);
    tag(&db, true);

    assert_eq!(before, snapshot(&d, &db));
}

#[test]
fn a_rename_moves_the_tags_with_the_file() {
    let (d, db) = tmp_dirs("rename");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db, true);

    let store = Store::open(db_path(&db)).unwrap();
    let old_path = d
        .canonicalize()
        .unwrap()
        .join("clients/beta/notes.txt")
        .to_string_lossy()
        .into_owned();
    let file_id = tagindex::file_by_path(&store, &old_path)
        .unwrap()
        .unwrap()
        .file_id;
    drop(store);

    fs::create_dir_all(d.join("clients/gamma")).unwrap();
    fs::rename(
        d.join("clients/beta/notes.txt"),
        d.join("clients/gamma/notes.txt"),
    )
    .unwrap();
    crawl(&d, &db);
    tag(&db, true);

    let store = Store::open(db_path(&db)).unwrap();
    let row = store.find_by_file_id(file_id).unwrap().unwrap();
    assert!(
        row.path
            .replace('\\', "/")
            .ends_with("clients/gamma/notes.txt"),
        "the stable id should have followed the rename: {}",
        row.path
    );

    let now: Vec<String> = tagindex::tags_of_file(&store, file_id)
        .unwrap()
        .into_iter()
        .map(|(t, _)| t.to_string())
        .collect();
    assert!(
        now.contains(&"path:gamma".to_string()),
        "the new directory must be reflected: {now:?}"
    );
    assert!(
        !now.contains(&"path:beta".to_string()),
        "the old directory must not linger: {now:?}"
    );
}

// ── 2. Format detection from real bytes ─────────────────────────────────────

#[test]
fn magic_bytes_beat_a_lying_extension_and_the_contradiction_is_tagged() {
    let (d, db) = tmp_dirs("magic");
    // A real PDF header in a file called `.txt`, and a real PNG header in a
    // file called `.jpg`.
    let mut pdf = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog >>\nendobj\n");
    write_bytes(&d, "docs/mislabelled.txt", &pdf);
    write_bytes(
        &d,
        "docs/photo.jpg",
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01",
    );

    crawl(&d, &db);
    let summary = tag(&db, true);
    assert_eq!(summary.magic_read, 2, "both files should have been read");

    let snap = snapshot(&d, &db);
    let txt = &snap["docs/mislabelled.txt"];
    assert!(txt.contains(&"format:pdf".to_string()), "{txt:?}");
    assert!(txt.contains(&"ext:txt".to_string()), "{txt:?}");
    // `.txt` carries no binary expectation, so a surprising format there is not
    // reported as an anomaly — only the other direction is.
    assert!(
        !txt.contains(&"anomaly:format-mismatch".to_string()),
        "{txt:?}"
    );

    let jpg = &snap["docs/photo.jpg"];
    assert!(jpg.contains(&"format:png".to_string()), "{jpg:?}");
    assert!(
        jpg.contains(&"anomaly:format-mismatch".to_string()),
        "a .jpg holding a PNG must be flagged: {jpg:?}"
    );
}

#[test]
fn without_magic_the_format_axis_degrades_visibly_rather_than_silently() {
    let (d, db) = tmp_dirs("nomagic");
    write_bytes(&d, "a/x.bin", &[0u8, 1, 2, 3, 4, 5, 6, 7]);
    write(&d, "a/y.txt", "text");
    crawl(&d, &db);

    // No `--read-magic`, no `sagasu hash`: the column is NULL for both files and
    // the summary has to say so. This count is what makes "format tags are only
    // extension guesses right now" visible from the outside.
    let summary = tag(&db, false);
    assert_eq!(summary.magic_present, 0);
    assert_eq!(summary.magic_missing, 2);
    assert_eq!(summary.magic_read, 0);

    // `.bin` is not on any text list, so with no bytes to look at there is no
    // format claim at all — rather than a wrong one.
    let snap = snapshot(&d, &db);
    assert!(
        !snap["a/x.bin"].iter().any(|t| t.starts_with("format:")),
        "{:?}",
        snap["a/x.bin"]
    );

    // Reading the bytes fills the column, and a *later* pass then needs no I/O.
    let second = tag(&db, true);
    assert_eq!(second.magic_read, 2);
    let third = tag(&db, false);
    assert_eq!(third.magic_present, 2, "magic must have been persisted");
    assert_eq!(third.magic_missing, 0);
}

// ── 3. User rules ───────────────────────────────────────────────────────────

#[test]
fn user_rules_add_tags_and_the_result_does_not_depend_on_rule_order() {
    let (d, db) = tmp_dirs("rules_order");
    build_corpus(&d);
    crawl(&d, &db);

    let a = d.parent().unwrap().join("rules-a.toml");
    let b = d.parent().unwrap().join("rules-b.toml");
    fs::write(
        &a,
        r#"
        [[rule]]
        path = "clients/**"
        tags = ["project:client-work"]

        [[rule]]
        path = "clients/acme-corp/**"
        tags = ["client:acme"]
        "#,
    )
    .unwrap();
    // Same two rules, written the other way round.
    fs::write(
        &b,
        r#"
        [[rule]]
        path = "clients/acme-corp/**"
        tags = ["client:acme"]

        [[rule]]
        path = "clients/**"
        tags = ["project:client-work"]
        "#,
    )
    .unwrap();

    tag_with_rules(&db, &a, true);
    let first = snapshot(&d, &db);
    tag_with_rules(&db, &b, true);
    let second = snapshot(&d, &db);

    assert_eq!(first, second, "rule order must not change the outcome");

    let invoice = &first["clients/acme-corp/invoice-2024-03-15_v2.txt"];
    assert!(
        invoice.contains(&"project:client-work".to_string()),
        "{invoice:?}"
    );
    assert!(invoice.contains(&"client:acme".to_string()), "{invoice:?}");
    assert!(
        !first["src/main.rs"]
            .iter()
            .any(|t| t.starts_with("project:")),
        "a rule must not reach outside its glob"
    );
}

#[test]
fn a_broken_rule_file_leaves_the_existing_tags_untouched() {
    let (d, db) = tmp_dirs("rules_broken");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db, true);
    let before = snapshot(&d, &db);

    let bad = d.parent().unwrap().join("bad.toml");
    fs::write(
        &bad,
        r#"
        [[rule]]
        path = "clients/**"
        tags = ["missing-namespace"]
        "#,
    )
    .unwrap();

    let err = tagindex::build(&TagConfig {
        db_path: db_path(&db),
        rules_path: Some(bad),
        read_magic: true,
        magic_max_size: u64::MAX,
    })
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("namespace:value"),
        "unexpected error: {err:#}"
    );

    assert_eq!(
        before,
        snapshot(&d, &db),
        "a failed pass must not half-tag the corpus"
    );
}

#[test]
fn the_documented_example_rule_file_actually_loads() {
    // A configuration example that does not parse is worse than no example at
    // all: it is copied, it fails, and the failure looks like a tool bug.
    let example = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/examples/sagasu-tags.toml")
        .canonicalize()
        .expect("docs/examples/sagasu-tags.toml must exist");
    let set = RuleSet::load(&example).expect("the documented example must load");
    assert!(set.len() >= 5, "the example should carry real rules");
    assert!(set.digest().is_some());
}

// ── 4. Lifecycle: deletion, staleness, atomicity ────────────────────────────

#[test]
fn tags_of_a_deleted_file_are_removed_on_the_next_pass() {
    let (d, db) = tmp_dirs("delete");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db, true);

    let store = Store::open(db_path(&db)).unwrap();
    let path = d
        .canonicalize()
        .unwrap()
        .join("clients/beta/notes.txt")
        .to_string_lossy()
        .into_owned();
    let file_id = tagindex::file_by_path(&store, &path)
        .unwrap()
        .unwrap()
        .file_id;
    assert!(!tagindex::tags_of_file(&store, file_id).unwrap().is_empty());
    drop(store);

    fs::remove_file(d.join("clients/beta/notes.txt")).unwrap();
    crawl(&d, &db);
    tag(&db, true);

    let store = Store::open(db_path(&db)).unwrap();
    assert!(
        store
            .find_by_file_id(file_id)
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some(),
        "the file should now be a tombstone"
    );
    assert!(
        tagindex::tags_of_file(&store, file_id).unwrap().is_empty(),
        "a tombstone must not keep contributing to facet counts"
    );
    // `path:beta` was that file's only holder, so the tag itself must be gone
    // rather than lingering as an empty bucket.
    let counts = tagindex::tag_counts(&store, Some("path"), 1000).unwrap();
    assert!(
        !counts.iter().any(|c| c.tag.value() == "beta"),
        "orphaned tags must be collected: {:?}",
        counts.iter().map(|c| c.tag.to_string()).collect::<Vec<_>>()
    );
}

#[test]
fn the_build_records_the_generation_it_ran_against() {
    let (d, db) = tmp_dirs("generation");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db, true);

    let store = Store::open(db_path(&db)).unwrap();
    let stats = store.get_stats().unwrap();
    assert_eq!(
        stats.tag_scan_generation,
        Some(stats.scan_generation),
        "a fresh tag pass is level with the index"
    );
    assert!(stats.tag_rows > 0);
    drop(store);

    // Another crawl leaves the tag layer one generation behind, which is what
    // `sagasu status` and `sagasu tags` report to the user.
    crawl(&d, &db);
    let store = Store::open(db_path(&db)).unwrap();
    let stats = store.get_stats().unwrap();
    assert_eq!(stats.tag_scan_generation, Some(stats.scan_generation - 1));
}

// ── 5. Queries ──────────────────────────────────────────────────────────────

#[test]
fn filtering_by_several_tags_requires_all_of_them() {
    let (d, db) = tmp_dirs("query");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db, true);

    let store = Store::open(db_path(&db)).unwrap();
    let acme = sagasu_core::Tag::parse("path:acme-corp").unwrap();
    let date = sagasu_core::Tag::parse("date:2024-03").unwrap();

    let only_acme = tagindex::files_with_tags(&store, std::slice::from_ref(&acme), 100).unwrap();
    let both = tagindex::files_with_tags(&store, &[acme, date], 100).unwrap();

    assert_eq!(only_acme.len(), 2, "two files live under clients/acme-corp");
    assert_eq!(both.len(), 1, "only the dated one carries both tags");
    assert!(both[0]
        .path
        .replace('\\', "/")
        .contains("invoice-2024-03-15_v2"));
}

#[test]
fn namespace_and_tag_counts_agree_with_what_was_written() {
    let (d, db) = tmp_dirs("counts");
    build_corpus(&d);
    crawl(&d, &db);
    let summary = tag(&db, true);

    let store = Store::open(db_path(&db)).unwrap();
    let namespaces = tagindex::namespace_counts(&store).unwrap();

    for ns in &namespaces {
        let stat = summary
            .namespaces
            .get(&ns.namespace)
            .unwrap_or_else(|| panic!("summary is missing namespace {}", ns.namespace));
        assert_eq!(stat.files, ns.files as u64, "namespace {}", ns.namespace);
        assert_eq!(
            stat.distinct, ns.distinct as u64,
            "namespace {}",
            ns.namespace
        );
    }

    let rows_from_namespaces: u64 = summary.namespaces.values().map(|s| s.rows).sum();
    assert_eq!(summary.rows, rows_from_namespaces);
    assert_eq!(summary.rows, store.get_stats().unwrap().tag_rows as u64);
    assert!(summary.tagged <= summary.files);
    assert!(summary.tagged_semantic <= summary.tagged);
}

#[test]
fn coverage_separates_what_was_inferred_from_what_was_merely_read_off_the_path() {
    let (d, db) = tmp_dirs("coverage");
    build_corpus(&d);
    crawl(&d, &db);
    let summary = tag(&db, true);

    // Every file sits in a directory and has (almost always) an extension, so
    // raw coverage is the number that cannot fail.
    assert_eq!(summary.coverage(), 100.0);
    // The semantic figure is the one with something to say — and it is strictly
    // smaller here, because plain `notes.txt` and `main.rs` carry no date,
    // version or naming pattern.
    assert!(
        summary.semantic_coverage() < 100.0 && summary.semantic_coverage() > 0.0,
        "semantic coverage was {}",
        summary.semantic_coverage()
    );
}

// ── 6. Concrete tags a human can check ──────────────────────────────────────

#[test]
fn the_generators_produce_the_tags_a_reader_would_expect() {
    let (d, db) = tmp_dirs("expected");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db, true);
    let snap = snapshot(&d, &db);

    let invoice = &snap["clients/acme-corp/invoice-2024-03-15_v2.txt"];
    for want in [
        "date:2024",
        "date:2024-03",
        "version:v2",
        "path:clients",
        "path:acme-corp",
        "path:acme",
        "ext:txt",
        "kind:text",
    ] {
        assert!(
            invoice.contains(&want.to_string()),
            "{want} missing: {invoice:?}"
        );
    }

    assert!(snap["clients/acme-corp/proposal_final.md"].contains(&"version:final".to_string()));
    assert!(snap["写真/2024/IMG_4821.txt"].contains(&"pattern:camera".to_string()));
    assert!(snap["写真/2024/IMG_4821.txt"].contains(&"date:2024".to_string()));
    assert!(snap["写真/2024/Screenshot 2024-03-15.txt"].contains(&"pattern:screenshot".to_string()));
    assert!(snap[".gitignore"].contains(&"pattern:dotfile".to_string()));
    assert!(snap["backup/report.bak"].contains(&"pattern:backup".to_string()));
    assert!(snap["src/main.rs"].contains(&"kind:code".to_string()));
    assert!(snap["src/config.toml"].contains(&"kind:config".to_string()));

    // A Japanese directory name is a tag like any other.
    assert!(
        snap["写真/2024/IMG_4821.txt"].contains(&"path:写真".to_string()),
        "{:?}",
        snap["写真/2024/IMG_4821.txt"]
    );
}

// ── 7. Schema migration ─────────────────────────────────────────────────────

#[test]
fn a_v1_database_gains_the_tag_tables_without_losing_its_files() {
    let (d, db) = tmp_dirs("migrate");
    build_corpus(&d);
    crawl(&d, &db);

    // Simulate an index written before the tag layer existed.
    {
        let store = Store::open(db_path(&db)).unwrap();
        store.meta_set("schema_version", "1").unwrap();
        store
            .conn()
            .execute_batch("DROP TABLE file_tags; DROP TABLE tags;")
            .unwrap();
    }

    let store = Store::open(db_path(&db)).unwrap();
    let stats = store.get_stats().unwrap();
    assert_eq!(stats.schema_version, sagasu_core::store::SCHEMA_VERSION);
    assert!(stats.live_count > 0, "the crawl's rows must survive");
    drop(store);

    // And tagging works against the migrated database.
    let summary = tag(&db, true);
    assert!(summary.rows > 0);
}

#[test]
fn a_database_from_a_newer_build_is_refused_rather_than_misread() {
    let (_d, db) = tmp_dirs("future");
    // A database claiming a schema this build does not know, and *missing* the
    // tables this build would create. Dropping them is what makes the refusal
    // checkable: if the DDL ran before the version gate, they would come back.
    {
        let store = Store::open(db_path(&db)).unwrap();
        store
            .conn()
            .execute_batch("DROP TABLE file_tags; DROP TABLE tags;")
            .unwrap();
        store
            .meta_set(
                "schema_version",
                &(sagasu_core::store::SCHEMA_VERSION + 1).to_string(),
            )
            .unwrap();
    }
    let before = table_names(&db_path(&db));
    assert!(
        !before.contains(&"tags".to_string()),
        "the fixture must start without the tag tables: {before:?}"
    );

    let err = match Store::open(db_path(&db)) {
        Ok(_) => panic!("opening a future-schema database must fail"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("newer than this build"),
        "unexpected error: {err}"
    );

    // A refusal that has already written to the database is not a refusal: the
    // next build would find tables a newer sagasu never put there.
    assert_eq!(
        before,
        table_names(&db_path(&db)),
        "a refused open must not have changed the database"
    );
}

/// Table names in a SQLite file, read through a connection that runs no DDL.
fn table_names(db: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap();
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    names
}
