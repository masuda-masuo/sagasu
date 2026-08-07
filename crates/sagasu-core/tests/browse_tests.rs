//! Integration tests for the facet drill-down (issue #5).
//!
//! Every test builds a real tree on disk, crawls it with the real crawler, tags
//! it with the real engine and then browses the real database. Nothing here
//! re-implements the ranking or the label: the assertions are about *properties*
//! the drill-down promises — that a facet count equals the size of the group it
//! leads to, that a step always narrows, that the same database gives the same
//! view, that a value which cannot narrow is named rather than offered — or
//! about a concrete outcome a human can check by eye (a named file reached in
//! four steps).

use std::fs;
use std::path::{Path, PathBuf};

use sagasu_core::browse::{self, BrowseQuery, BrowseView};
use sagasu_core::store::Store;
use sagasu_core::tagindex::{self, TagConfig};
use sagasu_core::tags::Tag;
use sagasu_core::walk::{self, CrawlConfig};

// ── helpers ─────────────────────────────────────────────────────────────────

/// A temporary working area: the tree to crawl, and a database *outside* it.
fn tmp_dirs(name: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("sagasu_browse_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let (data, db) = (base.join("data"), base.join("db"));
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&db).unwrap();
    (data, db)
}

fn db_path(db_dir: &Path) -> PathBuf {
    db_dir.join("test.db")
}

fn write(dir: &Path, rel: &str, content: &str) {
    write_bytes(dir, rel, content.as_bytes());
}

fn write_bytes(dir: &Path, rel: &str, content: &[u8]) {
    let p = dir.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&p, content).unwrap();
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

fn tag(db_dir: &Path) {
    tagindex::build(&TagConfig::new(db_path(db_dir))).unwrap();
}

fn open(db_dir: &Path) -> Store {
    Store::open(db_path(db_dir)).unwrap()
}

fn tags(list: &[&str]) -> Vec<Tag> {
    list.iter().map(|t| Tag::parse(t).unwrap()).collect()
}

fn view(store: &Store, selection: &[&str]) -> BrowseView {
    browse::browse(store, &BrowseQuery::new(tags(selection))).unwrap()
}

/// A minimal PNG header — enough for the magic sniffer.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";

/// A tree with a shape a drill-down can actually work on: two clients, two
/// years, a mix of formats, and — deliberately — one scan buried where nobody
/// would look for it.
///
/// The needle is `clients/acme/2024/renewal/scan-0001.png`. Every step towards
/// it has decoys: acme has images outside `renewal/`, `renewal/` has images in
/// the other year, and the other client and the archive have images of their
/// own. So no single tag finds it, and no step can be skipped.
fn build_corpus(data: &Path) {
    for year in ["2023", "2024"] {
        for n in 0..6 {
            write(
                data,
                &format!("clients/acme/{year}/invoices/invoice-{n:03}.txt"),
                "invoice",
            );
            write(
                data,
                &format!("clients/beta/{year}/invoices/invoice-{n:03}.txt"),
                "invoice",
            );
        }
        for n in 0..4 {
            write(
                data,
                &format!("clients/acme/{year}/reports/report-{n:03}.md"),
                "# report",
            );
        }
    }
    // Decoys: another client's photos, an unrelated archive, acme's own photos,
    // and four earlier scans in the same renewal folder but the wrong year.
    for n in 0..5 {
        write_bytes(
            data,
            &format!("clients/beta/2024/photos/site-{n:03}.png"),
            PNG,
        );
    }
    for n in 0..3 {
        write_bytes(data, &format!("archive/scans/old-{n:03}.png"), PNG);
    }
    for n in 0..3 {
        write_bytes(
            data,
            &format!("clients/acme/2024/photos/office-{n:03}.png"),
            PNG,
        );
    }
    for n in 0..4 {
        write_bytes(
            data,
            &format!("clients/acme/2023/renewal/scan-{n:04}.png"),
            PNG,
        );
    }
    write_bytes(data, "clients/acme/2024/renewal/scan-0001.png", PNG);
    // A pile of source files, so `ext:` has something to say too.
    for n in 0..8 {
        write(data, &format!("tooling/scripts/step-{n:03}.py"), "print(1)");
    }
}

/// Follow the drill-down greedily — always the top value of the top axis —
/// until the group is small enough to read, and report the path taken.
fn walk_down(store: &Store, max_steps: usize, small_enough: i64) -> (Vec<String>, Vec<i64>) {
    let mut selection: Vec<Tag> = Vec::new();
    let mut sizes: Vec<i64> = Vec::new();
    for _ in 0..max_steps {
        let v = browse::browse(store, &BrowseQuery::new(selection.clone())).unwrap();
        sizes.push(v.matched);
        if v.matched <= small_enough {
            break;
        }
        let Some(next) = v.axes.first().and_then(|a| a.values.first()) else {
            break;
        };
        selection.push(next.tag.clone());
    }
    (selection.iter().map(|t| t.to_string()).collect(), sizes)
}

/// Everything a view says, as one string — the comparison a determinism test
/// needs, since a reshuffle of two equal-scoring axes is exactly the kind of
/// wobble a field-by-field assertion would miss.
fn render(v: &BrowseView) -> String {
    let mut out = String::new();
    out.push_str(&format!("matched={} corpus={}\n", v.matched, v.corpus));
    for term in &v.label {
        out.push_str(&format!(
            "label {} {} {} {:.9}\n",
            term.tag, term.files, term.corpus_files, term.weight
        ));
    }
    for u in &v.universal {
        out.push_str(&format!("universal {u}\n"));
    }
    for axis in &v.axes {
        out.push_str(&format!(
            "axis {} {:.9} {:.9} {} {}\n",
            axis.namespace, axis.score, axis.coverage, axis.files, axis.distinct
        ));
        for value in &axis.values {
            out.push_str(&format!("  value {} {}\n", value.tag, value.files));
        }
    }
    out
}

// ── 1. The acceptance criterion ─────────────────────────────────────────────

#[test]
fn a_file_whose_directory_is_unknown_is_reached_in_four_steps() {
    let (d, db) = tmp_dirs("reach");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db);
    let store = open(&db);

    // The question is "where is that scanned renewal document?", and the answer
    // is a path the user does not know. What they do know is that it is an
    // image, and that it has something to do with acme's renewal — which is
    // exactly what the facet axes offer.
    let steps: Vec<Vec<&str>> = vec![
        vec![],
        vec!["kind:image"],
        vec!["kind:image", "path:acme"],
        vec!["kind:image", "path:acme", "path:renewal"],
        vec!["kind:image", "path:acme", "path:renewal", "date:2024"],
    ];
    let mut sizes = Vec::new();
    for step in &steps {
        let v = view(&store, step);
        sizes.push(v.matched);
        // Every step of the way, the tag the next step uses was actually on
        // offer — a scripted path that the tool would not have shown proves
        // nothing about the tool.
        if let Some(next) = steps.iter().find(|s| s.len() == step.len() + 1) {
            let wanted = next.last().unwrap();
            let offered: Vec<String> = v
                .axes
                .iter()
                .flat_map(|a| a.values.iter().map(|x| x.tag.to_string()))
                .collect();
            assert!(
                offered.contains(&wanted.to_string()),
                "step {step:?} did not offer {wanted}: {offered:?}"
            );
        }
    }

    // Strictly narrowing, and the last group is small enough to read.
    for pair in sizes.windows(2) {
        assert!(pair[1] < pair[0], "a step must narrow: {sizes:?}");
    }
    let last = view(&store, steps.last().unwrap());
    assert_eq!(last.matched, 1, "sizes were {sizes:?}");
    assert!(
        last.preview[0]
            .path
            .replace('\\', "/")
            .ends_with("clients/acme/2024/renewal/scan-0001.png"),
        "reached {:?} instead",
        last.preview[0].path
    );
}

#[test]
fn following_the_top_ranked_value_narrows_the_group_every_time() {
    let (d, db) = tmp_dirs("greedy");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db);
    let store = open(&db);

    // Nothing about the corpus is assumed here — just that the ranking, left to
    // drive itself, never proposes a step that fails to narrow. An axis whose
    // top value covered the whole group would loop forever, which is the bug
    // the `universal` exclusion exists to prevent.
    let (path, sizes) = walk_down(&store, 12, 3);
    assert!(!path.is_empty(), "the root offered nothing to drill into");
    for pair in sizes.windows(2) {
        assert!(
            pair[1] < pair[0],
            "the greedy path {path:?} did not narrow: {sizes:?}"
        );
    }
    // The descent has to *end*: either it reached a readable group, or it ran
    // out of axes because every remaining tag is shared by the whole group. What
    // it must never do is offer a step that changes nothing, which is what the
    // strict-narrowing check above rules out and what would otherwise loop until
    // `max_steps`.
    let last = view(&store, &path.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    assert!(
        last.matched <= 3 || last.axes.is_empty(),
        "greedy descent neither narrowed nor ran out: {sizes:?} after {path:?}"
    );
    assert!(
        path.len() < 12,
        "the descent used every step it was allowed: {path:?}"
    );
}

// ── 2. The counts have to mean what they say ────────────────────────────────

#[test]
fn every_offered_value_leads_to_a_group_of_exactly_the_promised_size() {
    let (d, db) = tmp_dirs("promise");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db);
    let store = open(&db);

    // The failure this rules out is the one design.md §6-1 already had to fix
    // once for `sagasu tags`: a facet offering three files and delivering one.
    // Here the check is exhaustive over two levels of the tree.
    for first in [
        vec![],
        vec!["kind:image"],
        vec!["kind:text"],
        vec!["path:acme"],
    ] {
        let v = view(&store, &first);
        for axis in &v.axes {
            for value in &axis.values {
                let mut deeper: Vec<Tag> = tags(&first);
                deeper.push(value.tag.clone());
                let actual = tagindex::count_files_with_tags(&store, &deeper).unwrap();
                assert_eq!(
                    actual, value.files,
                    "{first:?} + {} promised {} and delivers {actual}",
                    value.tag, value.files
                );
                assert!(
                    value.files < v.matched,
                    "{first:?} + {} does not narrow anything",
                    value.tag
                );
            }
        }
    }
}

#[test]
fn the_root_view_is_the_whole_live_index() {
    let (d, db) = tmp_dirs("root");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db);
    let store = open(&db);

    let v = view(&store, &[]);
    let live: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM files WHERE deleted_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(v.is_root());
    assert_eq!(v.matched, live);
    assert_eq!(v.corpus, live);
    assert!(!v.axes.is_empty());
}

#[test]
fn a_file_deleted_and_re_crawled_leaves_no_trace_in_the_facet_counts() {
    let (d, db) = tmp_dirs("tombstone");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db);
    let before = view(&open(&db), &["kind:image"]).matched;

    fs::remove_file(d.join("archive/scans/old-000.png")).unwrap();
    fs::remove_file(d.join("archive/scans/old-001.png")).unwrap();
    crawl(&d, &db);
    tag(&db);

    let after = view(&open(&db), &["kind:image"]);
    assert_eq!(after.matched, before - 2, "tombstones must not be counted");
    // …and the axis values must agree with the new total, not the old one.
    for axis in &after.axes {
        for value in &axis.values {
            assert!(
                value.files <= after.matched,
                "{} > {}",
                value.tag,
                value.files
            );
        }
    }
}

// ── 3. What must never be offered ───────────────────────────────────────────

#[test]
fn a_tag_already_selected_is_never_offered_again() {
    let (d, db) = tmp_dirs("reselect");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db);
    let store = open(&db);

    let selection = ["kind:image", "path:acme"];
    let v = view(&store, &selection);
    for axis in &v.axes {
        for value in &axis.values {
            assert!(
                !selection.contains(&value.tag.to_string().as_str()),
                "{} was offered back to the user",
                value.tag
            );
        }
    }
    for u in &v.universal {
        assert!(
            !selection.contains(&u.to_string().as_str()),
            "{u} was listed as a property of the group, but it *is* the query"
        );
    }
}

#[test]
fn a_value_every_file_shares_is_named_as_a_property_not_offered_as_a_step() {
    let (d, db) = tmp_dirs("universal");
    // Every file under `only/png` is a PNG, so `format:png` and `ext:png`
    // describe the group without being able to narrow it.
    for n in 0..6 {
        write_bytes(&d, &format!("only/png/shot-{n:03}.png"), PNG);
    }
    write(&d, "elsewhere/notes.txt", "text");
    crawl(&d, &db);
    tag(&db);
    let store = open(&db);

    let v = view(&store, &["path:png"]);
    assert_eq!(v.matched, 6);

    // Every axis in this group is a dead end: the six files are identical in
    // every namespace they have. So there is nothing to rank…
    assert!(
        v.axes.is_empty(),
        "no axis here can narrow the group: {:?}",
        v.axes
            .iter()
            .map(|a| (a.namespace.clone(), a.distinct))
            .collect::<Vec<_>>()
    );
    assert!(
        v.axes_total > 0 && v.axes_refining == 0,
        "the dead-end namespaces must still be counted: {} vs {}",
        v.axes_total,
        v.axes_refining
    );

    // …and this is precisely the case where a per-axis list of shared values
    // would have been dropped along with its axis. The view must still describe
    // the group.
    let named: Vec<String> = v.universal.iter().map(|t| t.to_string()).collect();
    for want in ["format:png", "ext:png", "kind:image", "path:only"] {
        assert!(
            named.contains(&want.to_string()),
            "{want} is shared by the whole group and was not reported: {named:?}"
        );
    }
    // The selection is not a description of itself.
    assert!(!named.contains(&"path:png".to_string()), "{named:?}");
    // Stable order, so a UI does not reshuffle the sentence between runs.
    let mut sorted = named.clone();
    sorted.sort();
    assert_eq!(named, sorted);
}

// ── 4. The label ────────────────────────────────────────────────────────────

#[test]
fn the_label_names_what_is_distinctive_about_a_group() {
    let (d, db) = tmp_dirs("label");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db);
    let store = open(&db);

    let v = view(&store, &["path:renewal"]);
    let terms: Vec<String> = v.label.iter().map(|t| t.tag.to_string()).collect();
    // This group is one scanned PNG. `format:png` is on nine files corpus-wide;
    // `ext:txt`-ish structural noise is on dozens. The label must reach for the
    // former.
    assert!(
        terms.iter().any(|t| t == "format:png" || t == "ext:png"),
        "the label missed what makes this group what it is: {terms:?}"
    );

    // A term the whole corpus shares must not out-rank a rare one. `path:clients`
    // covers most of the tree, so if it appears at all it appears last.
    let common = terms.iter().position(|t| t == "path:clients");
    let rare = terms
        .iter()
        .position(|t| t == "format:png" || t == "ext:png");
    if let (Some(c), Some(r)) = (common, rare) {
        assert!(r < c, "a corpus-wide tag out-ranked a rare one: {terms:?}");
    }
}

#[test]
fn the_label_never_just_repeats_the_query() {
    let (d, db) = tmp_dirs("labelquery");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db);
    let store = open(&db);

    let selection = ["kind:image", "path:acme"];
    let v = view(&store, &selection);
    for term in &v.label {
        assert!(
            !selection.contains(&term.tag.to_string().as_str()),
            "{} is the query, not a description of the answer",
            term.tag
        );
    }
    assert!(v.label_vocabulary >= v.label.len());
}

// ── 5. Determinism ──────────────────────────────────────────────────────────

#[test]
fn the_same_database_gives_the_same_view_every_time() {
    let (d, db) = tmp_dirs("stable");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db);

    let store = open(&db);
    let first = render(&view(&store, &[]));
    let second = render(&view(&store, &[]));
    assert_eq!(first, second, "two calls disagreed");

    // A fresh connection must not see a different hierarchy either — a temp
    // table left over from the previous call would be exactly that bug.
    let third = render(&view(&open(&db), &[]));
    assert_eq!(first, third);
}

#[test]
fn re_crawling_and_re_tagging_an_unchanged_tree_does_not_move_the_hierarchy() {
    let (d, db) = tmp_dirs("rebuild");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db);
    let before = [
        render(&view(&open(&db), &[])),
        render(&view(&open(&db), &["kind:image"])),
    ];

    // The property design.md §6 asks for by name: a user's spatial memory must
    // survive a re-index.
    crawl(&d, &db);
    tag(&db);
    let after = [
        render(&view(&open(&db), &[])),
        render(&view(&open(&db), &["kind:image"])),
    ];
    assert_eq!(before, after, "the facet hierarchy moved under a re-index");
}

#[test]
fn two_calls_with_different_selections_do_not_contaminate_each_other() {
    let (d, db) = tmp_dirs("isolation");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db);
    let store = open(&db);

    // One connection, three calls: the second must not be answered out of the
    // first one's materialised selection.
    let images = render(&view(&store, &["kind:image"]));
    let _ = view(&store, &["kind:text"]);
    let images_again = render(&view(&store, &["kind:image"]));
    assert_eq!(images, images_again);
}

// ── 6. Honest emptiness ─────────────────────────────────────────────────────

#[test]
fn an_index_with_no_tag_layer_answers_empty_rather_than_failing() {
    let (d, db) = tmp_dirs("untagged");
    build_corpus(&d);
    crawl(&d, &db);
    // No `sagasu tag` at all.
    let store = open(&db);

    let v = view(&store, &[]);
    assert!(v.axes.is_empty());
    assert!(v.label.is_empty());
    assert_eq!(v.axes_total, 0);
    assert!(v.matched > 0, "the files are still there");
    // …and the view says why, rather than leaving the caller to guess.
    assert!(!v.snapshot.built());
}

#[test]
fn a_selection_no_file_matches_is_an_empty_view_not_an_error() {
    let (d, db) = tmp_dirs("nomatch");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db);
    let store = open(&db);

    let v = view(&store, &["kind:image", "kind:text"]);
    assert_eq!(v.matched, 0);
    assert!(v.axes.is_empty());
    assert!(v.label.is_empty());
    assert!(v.preview.is_empty());
    assert!(v.corpus > 0);
}

#[test]
fn the_snapshot_reports_how_far_behind_the_tag_layer_is() {
    let (d, db) = tmp_dirs("behind");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db);
    assert_eq!(view(&open(&db), &[]).snapshot.behind(), 0);

    // A crawl without a re-tag: every file added since is invisible to a tag
    // filter, and the view has to carry that fact to whoever renders it.
    write(
        &d,
        "clients/acme/2024/renewal/scan-0002.png",
        "not tagged yet",
    );
    crawl(&d, &db);
    let v = view(&open(&db), &[]);
    assert_eq!(v.snapshot.behind(), 1);
    assert!(v.snapshot.built());
}

// ── 7. The preview is a page ────────────────────────────────────────────────

#[test]
fn the_preview_is_a_page_and_the_count_beside_it_is_the_total() {
    let (d, db) = tmp_dirs("preview");
    build_corpus(&d);
    crawl(&d, &db);
    tag(&db);
    let store = open(&db);

    let q = BrowseQuery {
        preview: 3,
        ..BrowseQuery::new(tags(&["kind:text"]))
    };
    let v = browse::browse(&store, &q).unwrap();
    assert_eq!(v.preview.len(), 3);
    assert!(
        v.matched > 3,
        "this corpus should have more text files than the page holds"
    );
    // `file_id` order, so two runs page the same way.
    let ids: Vec<i64> = v.preview.iter().map(|r| r.file_id).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted);

    let none = browse::browse(
        &store,
        &BrowseQuery {
            preview: 0,
            ..BrowseQuery::new(tags(&["kind:text"]))
        },
    )
    .unwrap();
    assert!(none.preview.is_empty());
    assert_eq!(none.matched, v.matched);
}

// ── 8. The display budget is part of the ranking ────────────────────────────

#[test]
fn an_axis_of_singletons_loses_to_one_that_actually_splits_the_group() {
    let (d, db) = tmp_dirs("singleton");
    // 200 files in 200 distinct directories — `path:` has 200 singleton values,
    // eight of which fit on screen — against two extensions splitting the group
    // 100/100. The scale matters and is the point: at 40 files the eight shown
    // singletons are a fifth of the corpus and the singleton axis wins *on the
    // merits*. It is only once the group outgrows the display that offering
    // them stops being a step, and the score has to fall off then, not before.
    for n in 0..200 {
        let ext = if n % 2 == 0 { "txt" } else { "md" };
        write(&d, &format!("flat/dir-{n:03}/file.{ext}"), "content");
    }
    crawl(&d, &db);
    tag(&db);
    let store = open(&db);

    let v = view(&store, &[]);
    let ext = v.axes.iter().position(|a| a.namespace == "ext");
    let path = v.axes.iter().position(|a| a.namespace == "path");
    assert!(
        ext.is_some(),
        "`ext:` should be on offer: {:?}",
        v.axes.iter().map(|a| &a.namespace).collect::<Vec<_>>()
    );
    if let (Some(e), Some(p)) = (ext, path) {
        assert!(
            e < p,
            "a 200-way singleton axis out-ranked a clean 100/100 split: {:?}",
            v.axes
                .iter()
                .map(|a| (a.namespace.clone(), a.score))
                .collect::<Vec<_>>()
        );
    }
    // …and the axis that did make it must say how much of itself is off screen.
    let path_axis = v.axes.iter().find(|a| a.namespace == "path").unwrap();
    assert!(path_axis.distinct > path_axis.values.len() as i64);
    assert!(
        path_axis.tail_assignments > 0,
        "the values that did not fit must be counted, not dropped"
    );
}

#[test]
fn widening_the_display_budget_can_change_which_axis_wins() {
    let (d, db) = tmp_dirs("budget");
    // `ext:` splits 100/100; `path:` has 200 values of one file each. With a
    // budget of 8 the singletons are almost worthless; with a budget of 256 they
    // identify the file outright. The score is defined against the budget
    // precisely so that this shows up as a different answer to a different
    // question, rather than as a wrong answer to one question.
    for n in 0..200 {
        let ext = if n % 2 == 0 { "txt" } else { "md" };
        write(&d, &format!("flat/dir-{n:03}/file.{ext}"), "content");
    }
    crawl(&d, &db);
    tag(&db);
    let store = open(&db);

    let narrow = browse::browse(
        &store,
        &BrowseQuery {
            max_values: 8,
            ..BrowseQuery::new(Vec::new())
        },
    )
    .unwrap();
    let wide = browse::browse(
        &store,
        &BrowseQuery {
            max_values: 256,
            ..BrowseQuery::new(Vec::new())
        },
    )
    .unwrap();

    let score = |v: &BrowseView, ns: &str| {
        v.axes
            .iter()
            .find(|a| a.namespace == ns)
            .map(|a| a.score)
            .unwrap_or(0.0)
    };
    assert!(score(&narrow, "ext") > score(&narrow, "path"));
    assert!(
        score(&wide, "path") > score(&wide, "ext"),
        "with room for all 40 values the singleton axis is the informative one"
    );
}
