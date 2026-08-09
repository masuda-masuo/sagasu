//! Integration tests for the UI backend layer (issue #68, M4 step 1).
//!
//! Every test builds a real tree on disk, crawls it with the real crawler, tags
//! it with the real engine and then calls the real [`sagasu_ui::browse_view`].
//! Nothing here re-implements the facet ranking or the label — those are
//! `sagasu-core`'s tests. What is asserted here is what this layer is *for*:
//!
//! - the JSON that reaches a webview carries every field of the core view,
//!   including the staleness facts a UI is not allowed to render without;
//! - a file deleted after indexing is reported in the data rather than dropped
//!   in silence.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use sagasu_core::store::Store;
use sagasu_core::tagindex::{self, TagConfig};
use sagasu_core::walk::{self, CrawlConfig};
use sagasu_ui::{browse_view, BrowseQueryDto, BrowseViewDto};

// ── helpers ─────────────────────────────────────────────────────────────────

/// A temporary working area: the tree to crawl, and a database *outside* it.
fn tmp_dirs(name: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("sagasu_ui_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let (data, db) = (base.join("data"), base.join("db"));
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&db).unwrap();
    (data, db)
}

fn db_path(db_dir: &Path) -> PathBuf {
    db_dir.join("test.db")
}

fn write(dir: &Path, rel: &str, content: &[u8]) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, content).unwrap();
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

fn tag(db_dir: &Path) {
    tagindex::build(&TagConfig::new(db_path(db_dir))).unwrap();
}

fn open(db_dir: &Path) -> Store {
    Store::open(db_path(db_dir)).unwrap()
}

/// A minimal PNG header — enough for the magic sniffer.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";

/// A tree with enough shape for the drill-down to have something to say: two
/// years, three formats, a directory structure worth a `path:` axis.
fn build_corpus(data: &Path) {
    for year in ["2023", "2024"] {
        for n in 0..4 {
            write(
                data,
                &format!("reports/{year}/report-{n:03}.txt"),
                b"a report",
            );
            write(data, &format!("notes/{year}/note-{n:03}.md"), b"# a note");
        }
    }
    for n in 0..3 {
        write(data, &format!("scans/scan-{n:03}.png"), PNG);
    }
}

/// Index, tag, and browse in one go. Returns the DTO the UI would receive.
fn indexed_view(name: &str, query: BrowseQueryDto) -> (PathBuf, PathBuf, BrowseViewDto) {
    let (data, db) = tmp_dirs(name);
    build_corpus(&data);
    crawl(&data, &db);
    tag(&db);
    let view = browse_view(&open(&db), &query).unwrap();
    (data, db, view)
}

/// Assert `object` has every one of `keys` — present, not merely non-null.
fn assert_keys(object: &Value, keys: &[&str], what: &str) {
    let map = object
        .as_object()
        .unwrap_or_else(|| panic!("{what} is not a JSON object: {object}"));
    for key in keys {
        assert!(
            map.contains_key(*key),
            "{what} has no `{key}` key: {object}"
        );
    }
    // The other half of "no silent drops": a field added to the DTO without
    // being listed here is a field this test is not watching.
    let mut extra: Vec<&str> = map
        .keys()
        .map(|k| k.as_str())
        .filter(|k| !keys.contains(k))
        .collect();
    extra.sort();
    assert!(extra.is_empty(), "{what} has unlisted key(s) {extra:?}");
}

// ── the boundary carries the whole view ─────────────────────────────────────

#[test]
fn the_json_a_webview_receives_carries_every_field_of_the_core_view() {
    let (_data, _db, view) = indexed_view("full_json", BrowseQueryDto::default());
    let json = serde_json::to_value(&view).unwrap();

    assert_keys(
        &json,
        &[
            "selected",
            "matched",
            "corpus",
            "label",
            "label_vocabulary",
            "universal",
            "axes",
            "axes_total",
            "axes_refining",
            "recommended",
            "preview",
            "dropped",
            "snapshot",
        ],
        "the view",
    );

    // The staleness facts, which docs/browse.md §5 says a UI cannot render a
    // view without. Mandatory fields, not options: the key is there even in the
    // "never built" case, which is what the next test covers.
    assert_keys(
        &json["snapshot"],
        &["tag_scan_generation", "scan_generation", "built", "behind"],
        "the snapshot",
    );
    assert_eq!(json["snapshot"]["built"], Value::Bool(true));
    assert_eq!(json["snapshot"]["behind"], serde_json::json!(0));
    assert!(json["snapshot"]["tag_scan_generation"].is_i64());

    // 16 reports/notes + 3 scans, all live, none of them deleted.
    assert_eq!(json["matched"], serde_json::json!(19));
    assert_eq!(json["corpus"], serde_json::json!(19));

    // The axes really made it across, with their values.
    let axes = json["axes"].as_array().unwrap();
    assert!(!axes.is_empty(), "no axes in {json}");
    assert_keys(
        &axes[0],
        &[
            "namespace",
            "score",
            "coverage",
            "files",
            "distinct",
            "values",
            "tail_assignments",
        ],
        "an axis",
    );
    let values = axes[0]["values"].as_array().unwrap();
    assert!(!values.is_empty(), "axis with no values: {}", axes[0]);
    assert_keys(&values[0], &["tag", "files", "share"], "a facet value");
    assert_keys(
        &values[0]["tag"],
        &["tag", "namespace", "value"],
        "a facet value's tag",
    );
    // The joined form is the one a UI hands straight back as a selection.
    let namespace = axes[0]["namespace"].as_str().unwrap();
    let offered = values[0]["tag"]["tag"].as_str().unwrap();
    assert!(offered.starts_with(&format!("{namespace}:")), "{offered}");

    // The label and its terms.
    let label = json["label"].as_array().unwrap();
    assert!(!label.is_empty(), "no label in {json}");
    assert_keys(
        &label[0],
        &["tag", "weight", "files", "corpus_files"],
        "a label term",
    );

    // The recommended step — present here because there is something on screen.
    assert_keys(
        &json["recommended"],
        &["namespace", "tag", "files", "share", "bits"],
        "the recommendation",
    );

    // The preview rows, in full: every column of `files`, not just the two the
    // CLI prints.
    let preview = json["preview"].as_array().unwrap();
    assert_eq!(
        preview.len(),
        5,
        "the default preview is 5 rows: {preview:?}"
    );
    assert_keys(
        &preview[0],
        &[
            "file_id",
            "path",
            "ext",
            "size",
            "mtime_ns",
            "ctime_ns",
            "magic",
            "blake3",
            "fs_id",
            "last_seen_scan",
            "deleted_at",
        ],
        "a preview row",
    );
    // The 2^53 guard is on the real value from the real crawl, not a fixture.
    let mtime = preview[0]["mtime_ns"]
        .as_str()
        .expect("mtime_ns is a string");
    assert!(
        mtime.parse::<i64>().unwrap() > 2i64.pow(53),
        "a real mtime_ns must be past what JavaScript can hold: {mtime}"
    );

    // Nothing was deleted between the crawl and the browse, so the check ran
    // and found nothing — and says so with an empty list rather than silence.
    assert_eq!(json["dropped"], serde_json::json!([]));

    // …and the whole thing survives the round trip a webview does not do but a
    // test can: nothing in the encoding is lossy.
    let back: BrowseViewDto = serde_json::from_value(json).unwrap();
    assert_eq!(back, view);
}

#[test]
fn an_index_with_no_tag_layer_still_hands_over_the_staleness_facts() {
    // The case the mandatory-field rule exists for: `sagasu tag` never ran, so
    // there is no facet tree at all. A UI still has to be told *that*, in the
    // data, rather than rendering an empty drill-down as an answer.
    let (data, db) = tmp_dirs("untagged");
    build_corpus(&data);
    crawl(&data, &db);
    let view = browse_view(&open(&db), &BrowseQueryDto::default()).unwrap();
    let json = serde_json::to_value(&view).unwrap();

    assert!(
        json["snapshot"]
            .as_object()
            .unwrap()
            .contains_key("tag_scan_generation"),
        "the key must be present even when there is no tag layer: {json}"
    );
    assert_eq!(json["snapshot"]["tag_scan_generation"], Value::Null);
    assert_eq!(json["snapshot"]["built"], Value::Bool(false));
    assert!(json["axes"].as_array().unwrap().is_empty());
}

// ── the existence check ─────────────────────────────────────────────────────

#[test]
fn a_file_deleted_after_indexing_moves_from_preview_to_dropped() {
    let (_data, db, before) = indexed_view("deleted", BrowseQueryDto::default());
    assert_eq!(before.preview.len(), 5);
    assert!(before.dropped.is_empty());

    // Delete one of the rows the preview just returned. The index does not
    // know: no delta source can report a deletion (design.md §5), and nothing
    // re-crawls between here and the next call.
    let victim = before.preview[0].clone();
    fs::remove_file(&victim.path).unwrap();

    let after = browse_view(&open(&db), &BrowseQueryDto::default()).unwrap();

    assert_eq!(
        after.dropped.len(),
        1,
        "the deleted row must be reported: {:?}",
        after.dropped
    );
    assert_eq!(after.dropped[0].path, victim.path);
    assert_eq!(after.dropped[0].file_id, victim.file_id);
    assert!(
        !after.preview.iter().any(|r| r.path == victim.path),
        "a path that no longer exists must not be offered as a file"
    );
    assert_eq!(after.preview.len(), 4);

    // The index-side count is untouched — it is an upper bound, and this is
    // exactly the gap between it and what is on disk. Reporting the drop is the
    // only thing that lets a UI say so.
    assert_eq!(after.matched, before.matched);

    // And it reaches the webview as data, not as a log line.
    let json = serde_json::to_value(&after).unwrap();
    assert_eq!(json["dropped"].as_array().unwrap().len(), 1);
    assert_eq!(json["dropped"][0]["path"], serde_json::json!(victim.path));
}

#[test]
fn asking_for_no_preview_still_reports_an_empty_drop_list() {
    // `dropped` is a plain field, not one that appears when something went
    // wrong, so a consumer can read it unconditionally.
    let query = BrowseQueryDto {
        preview: 0,
        ..BrowseQueryDto::default()
    };
    let (_data, _db, view) = indexed_view("no_preview", query);
    assert!(view.preview.is_empty());
    assert!(view.dropped.is_empty());
    let json = serde_json::to_value(&view).unwrap();
    assert_eq!(json["preview"], serde_json::json!([]));
    assert_eq!(json["dropped"], serde_json::json!([]));
}

// ── the query side ──────────────────────────────────────────────────────────

#[test]
fn a_selection_sent_as_json_narrows_the_view_it_comes_back_with() {
    let (data, db) = tmp_dirs("selection");
    build_corpus(&data);
    crawl(&data, &db);
    tag(&db);
    let store = open(&db);

    let root = browse_view(&store, &BrowseQueryDto::default()).unwrap();
    assert_eq!(root.matched, 19);
    assert!(root.is_root());

    // The shape a webview actually sends: JSON, with only the fields it cares
    // about.
    let query: BrowseQueryDto =
        serde_json::from_str(r#"{"selected": ["ext:png"], "preview": 10}"#).unwrap();
    let scans = browse_view(&store, &query).unwrap();

    assert_eq!(scans.matched, 3, "three PNGs in the corpus");
    assert!(!scans.is_root());
    assert_eq!(scans.selected.len(), 1);
    assert_eq!(scans.selected[0].tag, "ext:png");
    assert_eq!(scans.selected[0].namespace, "ext");
    assert_eq!(scans.selected[0].value, "png");
    assert_eq!(scans.preview.len(), 3, "all three fit in a preview of 10");
    assert!(scans.preview.iter().all(|r| r.path.ends_with(".png")));
    // The corpus denominator does not move with the selection.
    assert_eq!(scans.corpus, root.corpus);
}

#[test]
fn a_malformed_tag_from_the_webview_is_an_error_and_not_an_empty_view() {
    let (data, db) = tmp_dirs("bad_tag");
    build_corpus(&data);
    crawl(&data, &db);
    tag(&db);

    let query = BrowseQueryDto::new(vec!["ext png".to_string()]);
    let err = browse_view(&open(&db), &query).unwrap_err();
    assert!(err.to_string().contains("ext png"), "{err}");
}
