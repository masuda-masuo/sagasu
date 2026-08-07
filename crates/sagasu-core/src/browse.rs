//! Facet drill-down: turning a tag selection into the next question to ask.
//!
//! ## What this is for
//!
//! [`crate::tags`] manufactures the semantic axis and [`crate::tagindex`] stores
//! and counts it. Neither of them answers the question a person actually has in
//! front of a corpus they did not organise: *which axis should I look at next?*
//! design.md §6 calls that a facet hierarchy generated from tag co-occurrence
//! rather than from an embedding clustering, and this module is it — given a set
//! of already-chosen tags it returns the remaining tag axes ranked by how much
//! they would tell you, each with its top values and their counts, plus a
//! machine-generated label for the group you are standing in.
//!
//! ## Why the ranking lives here and not in the CLI
//!
//! The Tauri UI of M4 is the second consumer of exactly this computation, and a
//! drill-down whose ranking lived in `println!` calls would have to be rewritten
//! there — differently, by accident. So the whole of it is one function,
//! [`browse`], over plain data ([`BrowseQuery`] in, [`BrowseView`] out) with no
//! formatting and no I/O beyond the database. The CLI in `sagasu-cli` is a
//! printer for [`BrowseView`] and nothing else.
//!
//! ## The ranking: expected bits, over the values a screen can hold
//!
//! An axis is worth showing next when reading it tells you *where your file is*.
//! That is entropy, so the score is entropy — in bits — over the buckets the
//! caller can actually display:
//!
//! ```text
//! score(ns) = H(top-m values, tail, residue)   [bits]
//! ```
//!
//! For a selection of `n` files and a namespace `ns`, let `c_v` be the number of
//! files in the selection carrying `ns:v`. The buckets are
//!
//! | Bucket | Weight | Meaning |
//! |---|---|---|
//! | each of the top `m` values | `c_v` | a value the caller will show |
//! | tail | `Σ c_v − Σ_top c_v` | the values that did not fit on screen |
//! | residue | `max(0, n − Σ c_v)` | the part of the selection this axis cannot place |
//!
//! and `H = −Σ p·log2 p` over them, with `p` the bucket's share of the total
//! weight. Nothing else is multiplied in. Every failure mode of a facet axis is
//! already a low `H`:
//!
//! - **An axis that covers little of the set** — a huge residue bucket, so `H`
//!   collapses towards 0. (Coverage is still *reported*, in
//!   [`FacetAxis::coverage`], because it explains the score; it is not a second
//!   factor in it.)
//! - **An axis that is all singletons** (`path:` over a wide flat directory) —
//!   the `m` shown buckets hold one file each and the tail holds everything
//!   else, so `H` is near 0 even though the raw entropy of the full value
//!   distribution is *maximal*. This is the case that makes the display budget
//!   part of the formula rather than an afterthought: an axis you cannot walk
//!   through the values of is not a step you can take.
//! - **An axis with one dominant value** (99/1) — `H ≈ 0.08`.
//! - **A balanced `m`-way split** — `H ≈ log2(m)`, the best a single step can do.
//!
//! Because `m` is the same for every axis in one call, the scores are directly
//! comparable, and the number keeps a real unit: *the bits you gain by reading
//! this axis's displayed values*.
//!
//! A value carried by **every** file in the selection is not a step at all —
//! choosing it returns the same set — so it is excluded from the buckets and
//! from the offered values. It is not thrown away: it is collected in
//! [`BrowseView::universal`], because "everything in this group is a PNG under
//! `scans/`" is worth knowing, just not worth clicking. That list hangs off the
//! *view* rather than off an axis on purpose: an axis whose values are all
//! universal is dropped from the ranking entirely, and anything reported inside
//! it would have gone with it — which is the silent omission this project keeps
//! finding in its own output.
//!
//! Weights are tag *assignments*, not a partition: a file with two `path:` tags
//! is counted in both buckets, which is why `Σ c_v` may exceed `n` and the
//! residue is then zero.
//!
//! One consequence worth stating plainly, because it looks like a bug and is
//! not: on a *small* group a singleton axis can legitimately win. Eight buckets
//! out of forty files is a fifth of the group, so an axis of forty singletons
//! scores 1.32 bits against a clean 20/20 split's 1.00 — and it deserves to,
//! because landing on one of those eight ends the search outright. The
//! singleton axis only collapses when the group is large enough that the shown
//! values are a rounding error, which is exactly when offering them would be
//! useless. Raising the display budget is therefore a real change of question,
//! not just a longer list.
//!
//! ## The label: c-TF-IDF over the tag vocabulary
//!
//! A drill-down step needs a name for the group it just produced. The classic
//! class-based TF-IDF applies directly once you say what a document and a class
//! are, so:
//!
//! - a **document** is one file, and its words are its tags — the whole
//!   vocabulary of `namespace:value` pairs, path tokens included (they are
//!   `path:` tags, produced by [`crate::tags`] from the directory names);
//! - a **class** is the current selection: every file matching the chosen tags,
//!   concatenated into one meta-document;
//! - the **background** is the live corpus.
//!
//! ```text
//! W(t) = (c_S(t) / |S|) · ln(1 + N / df(t))
//! ```
//!
//! `c_S(t)` is the files in the selection carrying `t`, `df(t)` the live files in
//! the whole index carrying it, `N` the live files in the index. The left factor
//! is the term frequency of the class meta-document (share of the group), the
//! right is the inverse document frequency; a tag that is everywhere in this
//! group *and* everywhere in the corpus scores low, a tag that is everywhere in
//! this group and nowhere else scores high. Tags the user explicitly selected are
//! excluded — they are the query, not a description of the answer.
//!
//! Reading tags rather than re-tokenising file names is deliberate: the tags are
//! already the deterministic, cased-folded, capped vocabulary this project
//! defends, so a label cannot say something the facet counts would deny.
//!
//! ## Determinism
//!
//! design.md §6 asks for a hierarchy that does not reshuffle under re-indexing,
//! because a user's spatial memory is built on it. Two things secure that:
//!
//! - the tag layer itself is a pure function of the file (see [`crate::tags`]);
//! - every ordering here is a **total** order. Scores are floats, so they are
//!   compared through [`rank_key`], which rounds to a fixed 1e-6 grid; ties then
//!   fall through to integer counts and finally to the tag or namespace name,
//!   which cannot tie. Rounding first also means a one-ulp difference in `log2`
//!   or `ln` between two platforms' libm cannot reorder an axis list.
//!
//! ## What the counts are, and are not
//!
//! Every number here comes from the index, restricted to files the index knows
//! are live ([`crate::tagindex`]'s `LIVE_FILES_JOIN`). None of them is checked
//! against the filesystem, so [`BrowseView::matched`] and every facet count is an
//! **upper bound** — the same footing as `sagasu tags`, and the caller has to
//! present it that way. [`BrowseView::snapshot`] carries the generation gap so a
//! UI cannot render the view without having been handed the reason it might be
//! wrong.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::store::{FileRow, Store};
use crate::tagindex::{self, LIVE_FILES_JOIN};
use crate::tags::Tag;

// ── Defaults ────────────────────────────────────────────────────────────────

/// Axes proposed per step. Four fits on a screen and keeps a step a decision
/// rather than a search.
pub const DEFAULT_MAX_AXES: usize = 4;

/// Values shown per axis — also the `m` of the ranking formula, so changing it
/// changes which axes win, not just how many rows are printed.
pub const DEFAULT_MAX_VALUES: usize = 8;

/// Terms in a generated group label.
pub const DEFAULT_LABEL_TERMS: usize = 5;

/// Files previewed under a view.
pub const DEFAULT_PREVIEW: usize = 5;

/// Grid the float scores are rounded onto before they are compared.
///
/// Ranking on raw `f64` would make the order of two all-but-equal axes depend on
/// the last bits of a `log2`, which is not guaranteed identical between
/// platforms' libm. Rounding to 1e-6 turns those into genuine ties, which the
/// integer and name tiebreaks then resolve the same way everywhere.
const RANK_SCALE: f64 = 1e6;

/// The integer a score is ranked by. See [`RANK_SCALE`].
pub fn rank_key(score: f64) -> i64 {
    if score.is_nan() {
        return i64::MIN;
    }
    (score * RANK_SCALE).round() as i64
}

// ── Query ───────────────────────────────────────────────────────────────────

/// What to browse, and how much of it to bring back.
#[derive(Debug, Clone)]
pub struct BrowseQuery {
    /// Tags already chosen, ANDed. Empty = the whole live index.
    pub selected: Vec<Tag>,
    /// Axes to return, after ranking.
    pub max_axes: usize,
    /// Values per axis — the `m` of the ranking formula.
    pub max_values: usize,
    /// Terms in the generated label.
    pub label_terms: usize,
    /// Files to bring back as a preview of the selection. 0 = none.
    pub preview: usize,
}

impl BrowseQuery {
    /// A query with the documented defaults.
    pub fn new(selected: Vec<Tag>) -> Self {
        Self {
            selected,
            max_axes: DEFAULT_MAX_AXES,
            max_values: DEFAULT_MAX_VALUES,
            label_terms: DEFAULT_LABEL_TERMS,
            preview: DEFAULT_PREVIEW,
        }
    }
}

// ── View ────────────────────────────────────────────────────────────────────

/// One offerable next step: a tag and how much of the selection it keeps.
#[derive(Debug, Clone)]
pub struct FacetValue {
    /// The tag to add to the selection.
    pub tag: Tag,
    /// Live files in the selection carrying it — the size of the set the user
    /// would land in. An index-side count, never existence-checked.
    pub files: i64,
    /// `files` as a share of the current selection.
    pub share: f64,
}

/// One candidate axis, scored.
#[derive(Debug, Clone)]
pub struct FacetAxis {
    /// The namespace.
    pub namespace: String,
    /// Expected bits gained by reading [`FacetAxis::values`] — the ranking score.
    /// See the module docs.
    pub score: f64,
    /// Share of the selection carrying *any* tag in this namespace. Reported to
    /// explain the score, not multiplied into it.
    pub coverage: f64,
    /// Live files in the selection carrying any tag in this namespace.
    pub files: i64,
    /// Distinct values that would actually narrow the selection.
    pub distinct: i64,
    /// The top [`BrowseQuery::max_values`] of those, count-descending.
    pub values: Vec<FacetValue>,
    /// Tag assignments behind the values that did not fit. Non-zero means the
    /// axis has more to offer than is on screen, and the caller must say so.
    pub tail_assignments: i64,
}

/// One term of a generated group label.
#[derive(Debug, Clone)]
pub struct LabelTerm {
    /// The tag.
    pub tag: Tag,
    /// c-TF-IDF weight. See the module docs.
    pub weight: f64,
    /// Files in the selection carrying it.
    pub files: i64,
    /// Live files in the whole index carrying it.
    pub corpus_files: i64,
}

/// What the tag layer is a snapshot *of*.
///
/// Carried in the view rather than left for the caller to fetch, so a UI cannot
/// render a drill-down without having been handed the reason it may be missing
/// files (design.md §6-1: the tag layer is an index-time snapshot, and files
/// created since carry no tags).
#[derive(Debug, Clone)]
pub struct TagLayerSnapshot {
    /// Scan generation the tag layer was built from. `None` = never built.
    pub tag_scan_generation: Option<i64>,
    /// Scan generation of the metadata index right now.
    pub scan_generation: i64,
}

impl TagLayerSnapshot {
    /// Whether `sagasu tag` has ever run against this index.
    pub fn built(&self) -> bool {
        self.tag_scan_generation.is_some()
    }

    /// Crawls that happened after the tag layer was built. Greater than 0 means
    /// a tag filter is missing every file indexed since.
    pub fn behind(&self) -> i64 {
        match self.tag_scan_generation {
            Some(g) => (self.scan_generation - g).max(0),
            None => 0,
        }
    }
}

/// The answer to "I have chosen these tags — what now?".
#[derive(Debug, Clone)]
pub struct BrowseView {
    /// The selection, deduplicated, in the order given.
    pub selected: Vec<Tag>,
    /// Live files matching all of it. An index-side count and therefore an upper
    /// bound: no row here has been checked against the filesystem.
    pub matched: i64,
    /// Live files in the whole index — the denominator of the corpus.
    pub corpus: i64,
    /// Machine-generated label for the selection, best term first.
    pub label: Vec<LabelTerm>,
    /// Terms the label was chosen from, before [`BrowseQuery::label_terms`] cut
    /// it. A label that shows 5 of 5 and one that shows 5 of 900 deserve
    /// different amounts of trust.
    pub label_vocabulary: usize,
    /// Tags every file in the selection carries, excluding the selection
    /// itself, in tag order. Not steps — choosing one returns the same group —
    /// but the truest one-line description of what the group *is*.
    ///
    /// On the view rather than on an axis because an axis with no refining value
    /// is dropped from the ranking, and a list attached to it would vanish with
    /// it. Measured while writing the tests: a group of six PNGs in one folder
    /// reported *nothing* about itself, because every one of its axes was a dead
    /// end.
    pub universal: Vec<Tag>,
    /// Ranked axes, best first, at most [`BrowseQuery::max_axes`].
    pub axes: Vec<FacetAxis>,
    /// Namespaces present anywhere in the selection.
    pub axes_total: usize,
    /// How many of those have at least one value that would narrow the
    /// selection. The difference from [`BrowseView::axes_total`] is the number of
    /// axes that exist here but are dead ends, and it is reported rather than
    /// quietly absent from the list.
    pub axes_refining: usize,
    /// First files of the selection, `file_id` order. Not existence-checked —
    /// that is the caller's job, exactly as in `sagasu tags`.
    pub preview: Vec<FileRow>,
    /// Freshness of the layer all of the above was read from.
    pub snapshot: TagLayerSnapshot,
}

impl BrowseView {
    /// Whether the selection is empty — the root of the hierarchy.
    pub fn is_root(&self) -> bool {
        self.selected.is_empty()
    }
}

// ── Entropy ─────────────────────────────────────────────────────────────────

/// Bits gained by reading the first `shown` of `counts`.
///
/// `counts` are the file counts of an axis's refining values, descending;
/// `matched` is the size of the selection. Buckets are the first `shown` counts,
/// one tail bucket for the rest, and one residue bucket for the part of the
/// selection the axis says nothing about. See the module docs for why nothing
/// else enters the score.
pub fn axis_bits(counts: &[i64], matched: i64, shown: usize) -> f64 {
    let assignments: i64 = counts.iter().sum();
    if assignments <= 0 || matched <= 0 {
        return 0.0;
    }
    let head = &counts[..shown.min(counts.len())];
    let tail = assignments - head.iter().sum::<i64>();
    // A file may carry several values of one namespace, so the assignments can
    // outnumber the files; there is then no part of the selection left over.
    let residue = (matched - assignments).max(0);
    let weight = (assignments + residue) as f64;

    let mut bits = 0.0;
    for bucket in head.iter().copied().chain([tail, residue]) {
        if bucket <= 0 {
            continue;
        }
        let p = bucket as f64 / weight;
        bits -= p * p.log2();
    }
    bits
}

/// c-TF-IDF weight of one term. See the module docs for the definition.
pub fn label_weight(
    files_in_selection: i64,
    selection: i64,
    corpus_files: i64,
    corpus: i64,
) -> f64 {
    if selection <= 0 || corpus_files <= 0 {
        return 0.0;
    }
    let tf = files_in_selection as f64 / selection as f64;
    let idf = (1.0 + corpus as f64 / corpus_files as f64).ln();
    tf * idf
}

// ── The one entry point ─────────────────────────────────────────────────────

/// Rank the remaining tag axes of a selection, and label the group it forms.
///
/// One database read pass, no filesystem access, no formatting. See the module
/// docs for the ranking and the label.
pub fn browse(store: &Store, query: &BrowseQuery) -> Result<BrowseView> {
    let selected = tagindex::dedup_tags(&query.selected);
    let snapshot = TagLayerSnapshot {
        tag_scan_generation: store
            .meta_get("tag_scan_generation")?
            .and_then(|s| s.parse().ok()),
        scan_generation: store.scan_generation(),
    };

    let matched = materialize_selection(store, &selected)?;
    let corpus: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM files WHERE deleted_at IS NULL",
        [],
        |row| row.get(0),
    )?;

    let value_counts = selection_tag_counts(store)?;
    let namespace_files = selection_namespace_files(store)?;

    // `value_counts` arrives in `tag_id` order, which is insertion order of the
    // tag layer and therefore not something a user could predict. Everything
    // below either sorts explicitly or is a per-tag lookup, except this list —
    // so it is sorted here.
    let universal: Vec<Tag> = if matched > 0 {
        let mut v: Vec<Tag> = value_counts
            .iter()
            .filter(|(tag, n)| *n >= matched && !selected.contains(tag))
            .map(|(tag, _)| tag.clone())
            .collect();
        v.sort();
        v
    } else {
        Vec::new()
    };

    let axes = rank_axes(&value_counts, &namespace_files, &selected, matched, query);
    let axes_total = namespace_files.len();
    let axes_refining = axes.len();

    let label = build_label(store, &value_counts, &selected, matched, corpus, query)?;
    let label_vocabulary = value_counts
        .iter()
        .filter(|(tag, _)| !selected.contains(tag))
        .count();

    let preview = if query.preview == 0 {
        Vec::new()
    } else {
        tagindex::files_in_selection(store, &selected, query.preview as i64)?
    };

    Ok(BrowseView {
        selected,
        matched,
        corpus,
        label,
        label_vocabulary,
        universal,
        axes: axes.into_iter().take(query.max_axes).collect(),
        axes_total,
        axes_refining,
        preview,
        snapshot,
    })
}

// ── Steps ───────────────────────────────────────────────────────────────────

/// Temporary table holding the `file_id`s of the current selection.
///
/// It exists only in this connection's `temp` schema — nothing is written to the
/// index file — and it is rebuilt on every [`browse`] call.
const SELECTION_TABLE: &str = "temp.sagasu_browse_selection";

/// Evaluate the selection once, into [`SELECTION_TABLE`]. Returns its size.
///
/// ## Why not just repeat the subquery
///
/// `tagindex::selection_subquery` groups `file_tags` by `file_id` to test the
/// AND of the chosen tags, and SQLite serves that by walking `file_tags` in
/// primary-key order — a full pass over the junction table regardless of how
/// small the selection turns out to be. Three of the four reads below need that
/// set, and inlining the subquery in each made a drill-down step pay for it
/// three times. Measured on `/usr` (63,901 files, 555,664 tag rows, release
/// build, `--no-fresh` so the number is the database work alone): one step went
/// from 1.27s to 0.93s at `kind:image`, and from 1.34s to 1.01s at the root.
/// The pass that remains is the one that actually evaluates the selection.
///
/// Materialising it once also makes the three reads provably talk about the
/// *same* set. A selection re-derived per query is a set that could disagree
/// with itself if the definition of "live" were ever edited in one place only.
fn materialize_selection(store: &Store, selected: &[Tag]) -> Result<i64> {
    let conn = store.conn();
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS {SELECTION_TABLE};
         CREATE TABLE {SELECTION_TABLE} (file_id INTEGER PRIMARY KEY);"
    ))?;

    let (sub, values) = tagindex::selection_subquery(selected);
    let params: Vec<&dyn rusqlite::ToSql> = values.iter().map(|b| b.as_ref()).collect();
    conn.execute(
        &format!("INSERT INTO {SELECTION_TABLE} (file_id) {sub}"),
        params.as_slice(),
    )?;

    let n = conn.query_row(
        &format!("SELECT COUNT(*) FROM {SELECTION_TABLE}"),
        [],
        |row| row.get(0),
    )?;
    Ok(n)
}

/// Every tag carried by a file in the selection, with how many of those files
/// carry it. `(file_id, tag_id)` is the primary key of `file_tags`, so the row
/// count per tag *is* the file count.
fn selection_tag_counts(store: &Store) -> Result<Vec<(Tag, i64)>> {
    let mut stmt = store.conn().prepare(&format!(
        "SELECT t.namespace, t.value, COUNT(*)
         FROM {SELECTION_TABLE} s
         JOIN file_tags ft ON ft.file_id = s.file_id
         JOIN tags t ON t.tag_id = ft.tag_id
         GROUP BY ft.tag_id"
    ))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(ns, value, n)| Tag::new(&ns, &value).ok().map(|t| (t, n)))
        .collect())
}

/// Files in the selection carrying at least one tag of each namespace. Distinct
/// files, not assignments — a file with six `path:` tags covers the axis once.
fn selection_namespace_files(store: &Store) -> Result<BTreeMap<String, i64>> {
    let mut stmt = store.conn().prepare(&format!(
        "SELECT t.namespace, COUNT(DISTINCT ft.file_id)
         FROM {SELECTION_TABLE} s
         JOIN file_tags ft ON ft.file_id = s.file_id
         JOIN tags t ON t.tag_id = ft.tag_id
         GROUP BY t.namespace"
    ))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().collect())
}

/// Live files in the *whole* index carrying each tag that occurs in the
/// selection — the `df(t)` of the label formula.
///
/// Restricted to the selection's vocabulary rather than fetched for every tag in
/// the database: on a large corpus the `path:` namespace alone is most of the
/// vocabulary, and none of it can be a label term for a group it does not touch.
fn corpus_tag_counts(store: &Store) -> Result<BTreeMap<Tag, i64>> {
    let mut stmt = store.conn().prepare(&format!(
        "SELECT t.namespace, t.value, COUNT(*)
         FROM file_tags ft
         JOIN tags t ON t.tag_id = ft.tag_id
         {LIVE_FILES_JOIN}
         WHERE ft.tag_id IN (
             SELECT v.tag_id FROM {SELECTION_TABLE} s
             JOIN file_tags v ON v.file_id = s.file_id
         )
         GROUP BY ft.tag_id"
    ))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(ns, value, n)| Tag::new(&ns, &value).ok().map(|t| (t, n)))
        .collect())
}

/// Group the selection's tags into namespaces, score each, and sort.
///
/// Axes with nothing that would narrow the selection are dropped here — the
/// count of what survived is what the caller compares against `axes_total`.
fn rank_axes(
    value_counts: &[(Tag, i64)],
    namespace_files: &BTreeMap<String, i64>,
    selected: &[Tag],
    matched: i64,
    query: &BrowseQuery,
) -> Vec<FacetAxis> {
    let mut by_namespace: BTreeMap<&str, Vec<(&Tag, i64)>> = BTreeMap::new();
    for (tag, n) in value_counts {
        by_namespace
            .entry(tag.namespace())
            .or_default()
            .push((tag, *n));
    }

    let mut axes: Vec<FacetAxis> = Vec::new();
    for (namespace, mut entries) in by_namespace {
        // Count descending, then the tag itself: the same convention
        // `tagindex::tag_counts` uses, so a facet list never reshuffles between
        // the two commands that show it.
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

        let mut refining: Vec<(&Tag, i64)> = Vec::new();
        for (tag, n) in entries {
            // The selection itself is not a step (re-offering it is a no-op,
            // and letting it into the buckets would hand every axis a value the
            // size of the whole group); nor is a value the whole group already
            // shares. Those are collected on the view as `universal`.
            if selected.contains(tag) || (matched > 0 && n >= matched) {
                continue;
            }
            refining.push((tag, n));
        }
        if refining.is_empty() {
            continue;
        }

        let counts: Vec<i64> = refining.iter().map(|(_, n)| *n).collect();
        let score = axis_bits(&counts, matched, query.max_values);
        let files = namespace_files.get(namespace).copied().unwrap_or(0);
        let coverage = if matched > 0 {
            files as f64 / matched as f64
        } else {
            0.0
        };
        let shown = query.max_values.min(refining.len());
        let tail_assignments = counts.iter().sum::<i64>() - counts[..shown].iter().sum::<i64>();

        axes.push(FacetAxis {
            namespace: namespace.to_string(),
            score,
            coverage,
            files,
            distinct: refining.len() as i64,
            values: refining[..shown]
                .iter()
                .map(|(tag, n)| FacetValue {
                    tag: (*tag).clone(),
                    files: *n,
                    share: if matched > 0 {
                        *n as f64 / matched as f64
                    } else {
                        0.0
                    },
                })
                .collect(),
            tail_assignments,
        });
    }

    // A total order: rounded score, then coverage, then how many values the axis
    // has, then the namespace name — which cannot tie.
    axes.sort_by(|a, b| {
        rank_key(b.score)
            .cmp(&rank_key(a.score))
            .then_with(|| rank_key(b.coverage).cmp(&rank_key(a.coverage)))
            .then_with(|| b.distinct.cmp(&a.distinct))
            .then_with(|| a.namespace.cmp(&b.namespace))
    });
    axes
}

/// The c-TF-IDF label of the selection.
fn build_label(
    store: &Store,
    value_counts: &[(Tag, i64)],
    selected: &[Tag],
    matched: i64,
    corpus: i64,
    query: &BrowseQuery,
) -> Result<Vec<LabelTerm>> {
    if query.label_terms == 0 || matched <= 0 || value_counts.is_empty() {
        return Ok(Vec::new());
    }
    let df = corpus_tag_counts(store)?;

    let mut terms: Vec<LabelTerm> = value_counts
        .iter()
        .filter(|(tag, _)| !selected.contains(tag))
        .map(|(tag, files)| {
            // A tag can only be missing from `df` if the two queries disagreed
            // about what is live, which they cannot: both go through
            // `LIVE_FILES_JOIN`. Falling back to the in-selection count keeps
            // the weight finite rather than inventing an infinite idf.
            let corpus_files = df.get(tag).copied().unwrap_or(*files).max(*files);
            LabelTerm {
                tag: tag.clone(),
                weight: label_weight(*files, matched, corpus_files, corpus),
                files: *files,
                corpus_files,
            }
        })
        .collect();

    terms.sort_by(|a, b| {
        rank_key(b.weight)
            .cmp(&rank_key(a.weight))
            .then_with(|| a.tag.cmp(&b.tag))
    });
    terms.truncate(query.label_terms);
    Ok(terms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_axis_that_splits_evenly_beats_one_with_a_dominant_value() {
        let even = axis_bits(&[25, 25, 25, 25], 100, 8);
        let lopsided = axis_bits(&[99, 1], 100, 8);
        assert!((even - 2.0).abs() < 1e-9, "four even buckets are two bits");
        assert!(lopsided < 0.1, "{lopsided}");
        assert!(even > lopsided);
    }

    #[test]
    fn an_axis_of_singletons_scores_near_zero_despite_maximal_raw_entropy() {
        // 1000 values, one file each: the raw distribution has ~10 bits of
        // entropy, but only eight of them can be shown, so a step through this
        // axis almost never lands anywhere.
        let counts = vec![1i64; 1000];
        let bits = axis_bits(&counts, 1000, 8);
        assert!(bits < 0.15, "singleton axis scored {bits}");
        // …and it must lose to a plain two-way split of the same set.
        assert!(axis_bits(&[500, 500], 1000, 8) > bits);
    }

    #[test]
    fn an_axis_that_covers_little_of_the_set_is_punished_by_the_residue_bucket() {
        // Two even values over 5% of the selection: 95% of it goes to residue.
        let sparse = axis_bits(&[25, 25], 1000, 8);
        let full = axis_bits(&[500, 500], 1000, 8);
        assert!(sparse < 0.4, "{sparse}");
        assert!((full - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_multi_valued_axis_has_no_residue() {
        // 100 files, each carrying two of the four values: 200 assignments.
        let bits = axis_bits(&[50, 50, 50, 50], 100, 8);
        assert!((bits - 2.0).abs() < 1e-9, "{bits}");
    }

    #[test]
    fn an_empty_or_impossible_axis_scores_zero_rather_than_nan() {
        assert_eq!(axis_bits(&[], 10, 8), 0.0);
        assert_eq!(axis_bits(&[5], 0, 8), 0.0);
        assert_eq!(axis_bits(&[0, 0], 10, 8), 0.0);
        // One value on every file: caller excludes it, but the maths must not
        // produce a NaN if it does not.
        assert!(axis_bits(&[10], 10, 8).is_finite());
    }

    #[test]
    fn the_label_prefers_a_tag_specific_to_the_group_over_a_common_one() {
        // Both tags cover the whole 10-file group; one is on 10 files corpus-wide,
        // the other on 10_000.
        let specific = label_weight(10, 10, 10, 10_000);
        let common = label_weight(10, 10, 10_000, 10_000);
        assert!(specific > common, "{specific} vs {common}");
        // And a rare tag on half the group still loses to a rare tag on all of it.
        assert!(label_weight(10, 10, 10, 10_000) > label_weight(5, 10, 10, 10_000));
    }

    #[test]
    fn label_weights_are_finite_at_the_edges() {
        assert_eq!(label_weight(0, 0, 0, 0), 0.0);
        assert_eq!(label_weight(1, 1, 0, 10), 0.0);
        assert!(label_weight(1, 1, 1, 1).is_finite());
    }

    #[test]
    fn the_rank_key_turns_a_last_bit_difference_into_a_tie() {
        let a = 2.0f64;
        // The next representable float above 2.0 — what a differing libm `log2`
        // could hand back for the same distribution on another platform.
        let b = f64::from_bits(a.to_bits() + 1);
        assert_ne!(a, b);
        assert_eq!(rank_key(a), rank_key(b));
        // …but a difference the display would show is still a difference.
        assert_ne!(rank_key(2.0), rank_key(2.001));
    }
}
