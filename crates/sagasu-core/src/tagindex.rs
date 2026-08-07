//! Persisting the tag layer: building it into SQLite and querying it back.
//!
//! The engine in [`crate::tags`] is a pure function; this module is everything
//! around it that touches state — the batched pass over the metadata index, the
//! `tags` / `file_tags` rows, the coverage measurement, and the read side that
//! `sagasu tags` answers from. Keeping the two apart is what makes the purity
//! claim in [`crate::tags`] structural rather than a promise: there is no
//! database handle and no filesystem call in that module to accidentally reach
//! for.
//!
//! ## Atomicity
//!
//! A build replaces the *whole* layer inside one transaction. A corpus where
//! some files carry this pass's tags and some the last pass's is a set of facet
//! counts that mean nothing, and no error message afterwards can undo it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rusqlite::params;

use crate::store::{FileRow, Store, MAGIC_LEN};
use crate::tagrules::RuleSet;
use crate::tags::{self, Tag};

// ── Build pass ──────────────────────────────────────────────────────────────

/// Rows pulled from SQLite per round of the tagging loop.
const BATCH: i64 = 4096;

/// `meta` keys describing the tag layer (documented in docs/schema_v0.md).
/// Written together at the end of a successful build and cleared at the start of
/// one, so they always describe tags that are actually in the database.
const TAG_META_KEYS: &[&str] = &[
    "tag_marker",
    "tag_scan_generation",
    "tag_files",
    "tag_rows",
    "tag_rules",
    "tag_rules_digest",
];

/// Configuration for a tag build.
#[derive(Debug, Clone)]
pub struct TagConfig {
    /// SQLite metadata index to tag.
    pub db_path: PathBuf,
    /// User rule file. `None` = built-in generators only.
    pub rules_path: Option<PathBuf>,
    /// Read the leading bytes of files whose `magic` column is still NULL.
    ///
    /// On by default ([`TagConfig::new`]). `magic` is nulled whenever a crawl
    /// sees a content change, so a build that does not re-read it silently
    /// demotes `format:` back to an extension guess for exactly the files that
    /// were most recently worked on.
    pub read_magic: bool,
    /// Skip the head read for files larger than this. Reading the head of a
    /// huge file is cheap, but seeking into a cold archive is not free either.
    pub magic_max_size: u64,
}

impl TagConfig {
    /// Config with the documented defaults for a database.
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            rules_path: None,
            read_magic: true,
            magic_max_size: u64::MAX,
        }
    }
}

/// Per-namespace coverage.
#[derive(Debug, Clone, Default)]
pub struct NamespaceStat {
    /// Live files carrying at least one tag in this namespace.
    pub files: u64,
    /// Distinct values in this namespace.
    pub distinct: u64,
    /// Rows written for this namespace.
    pub rows: u64,
}

/// Summary of a tag build — the acceptance-criterion measurement of issue #4.
#[derive(Debug, Clone, Default)]
pub struct TagSummary {
    /// Live files considered.
    pub files: u64,
    /// Files that got at least one tag.
    pub tagged: u64,
    /// Files that got at least one tag outside [`tags::STRUCTURAL_NAMESPACES`].
    pub tagged_semantic: u64,
    /// Rows written to `file_tags`.
    pub rows: u64,
    /// Distinct `namespace:value` pairs used.
    pub distinct: u64,
    /// Coverage per namespace, in namespace order.
    pub namespaces: BTreeMap<String, NamespaceStat>,
    /// Files whose `magic` column was already populated.
    pub magic_present: u64,
    /// Files with no magic bytes available — format tags for these came from
    /// the extension alone. Reported because "the format axis is weaker than it
    /// looks" is exactly the kind of gap that must not stay invisible.
    pub magic_missing: u64,
    /// Files whose leading bytes this pass read.
    pub magic_read: u64,
    /// Files the head read could not open.
    pub magic_unreadable: u64,
    /// Files that hit [`tags::MAX_TAGS_PER_FILE`].
    pub capped: u64,
    /// Tags the cap discarded, summed over every file.
    pub capped_dropped: u64,
    /// Those same discards broken down by namespace, in namespace order.
    ///
    /// A cap that is only reported as a file count says "something was lost";
    /// this says *what*, which is the difference between a user shrugging and a
    /// user noticing that the axis they were about to filter on is incomplete.
    pub capped_dropped_namespaces: BTreeMap<String, u64>,
    /// Rule file used, if any.
    pub rules_path: Option<String>,
    /// Number of rules loaded.
    pub rules_count: usize,
    /// Scan generation the tags were built from.
    pub scan_generation: i64,
    /// Wall-clock duration.
    pub elapsed_secs: f64,
}

impl TagSummary {
    /// Share of live files with at least one tag.
    pub fn coverage(&self) -> f64 {
        ratio(self.tagged, self.files)
    }

    /// Share of live files with at least one tag outside the structural
    /// namespaces — the number that actually says something was *inferred*.
    pub fn semantic_coverage(&self) -> f64 {
        ratio(self.tagged_semantic, self.files)
    }

    /// Share of live files carrying a tag in `namespace`.
    pub fn namespace_coverage(&self, namespace: &str) -> f64 {
        ratio(
            self.namespaces.get(namespace).map_or(0, |s| s.files),
            self.files,
        )
    }
}

fn ratio(n: u64, d: u64) -> f64 {
    if d == 0 {
        0.0
    } else {
        100.0 * n as f64 / d as f64
    }
}

/// Generate tags for every live file and replace the stored tag layer.
///
/// The whole pass runs in one transaction: a half-tagged index — some files
/// carrying tags from this run and some from the last — would be a corpus whose
/// facet counts mean nothing, and no error message can undo that after the fact.
pub fn build(config: &TagConfig) -> Result<TagSummary> {
    let t0 = Instant::now();
    let store = Store::open(&config.db_path)
        .with_context(|| format!("failed to open metadata index {:?}", config.db_path))?;

    let rules = match &config.rules_path {
        Some(p) => RuleSet::load(p)?,
        None => RuleSet::empty(),
    };

    let root = store.meta_get("root_path")?;
    let scan_generation = store.scan_generation();

    let mut summary = TagSummary {
        rules_path: rules
            .source()
            .map(|p| p.display().to_string())
            .or_else(|| config.rules_path.as_ref().map(|p| p.display().to_string())),
        rules_count: rules.len(),
        scan_generation,
        ..Default::default()
    };

    let tx = store.begin_tx()?;

    // Clear the descriptors first: if this pass dies half way, the database must
    // not keep claiming a tag build that no longer matches its contents.
    for key in TAG_META_KEYS {
        Store::meta_set_tx(&tx, key, "")?;
    }

    // Full rebuild. Tags belonging to files that have since been tombstoned go
    // with it, so `file_tags` only ever describes live files.
    tx.execute("DELETE FROM file_tags", [])?;

    let mut tag_ids: HashMap<Tag, i64> = HashMap::new();
    let mut ns_files: BTreeMap<String, u64> = BTreeMap::new();
    let mut ns_rows: BTreeMap<String, u64> = BTreeMap::new();
    let mut ns_values: BTreeMap<String, std::collections::HashSet<String>> = BTreeMap::new();

    let mut cursor: i64 = 0;
    loop {
        let rows = store.get_live_files_batch(cursor, BATCH)?;
        let Some(last) = rows.last() else { break };
        cursor = last.file_id;

        for row in &rows {
            summary.files += 1;

            let magic = resolve_magic(&store, row, config, &mut summary);
            let facts = tags::FileFacts {
                path: &row.path,
                root: root.as_deref(),
                ext: row.ext.as_deref(),
                magic: magic.as_deref(),
            };
            let set = tags::tags_for(&facts, &rules);

            if set.capped {
                summary.capped += 1;
                summary.capped_dropped += set.dropped.len() as u64;
                for (ns, n) in set.dropped_by_namespace() {
                    *summary.capped_dropped_namespaces.entry(ns).or_insert(0) += n;
                }
            }
            if set.is_empty() {
                continue;
            }
            summary.tagged += 1;
            if set.has_semantic_tag() {
                summary.tagged_semantic += 1;
            }

            let mut seen_ns: Vec<&str> = Vec::new();
            for (tag, sources) in &set.tags {
                let tag_id = match tag_ids.get(tag) {
                    Some(id) => *id,
                    None => {
                        let id = intern_tag(&tx, tag)?;
                        tag_ids.insert(tag.clone(), id);
                        id
                    }
                };
                tx.prepare_cached(
                    "INSERT OR REPLACE INTO file_tags (file_id, tag_id, sources) \
                     VALUES (?1, ?2, ?3)",
                )?
                .execute(params![row.file_id, tag_id, *sources as i64])?;
                summary.rows += 1;

                let ns = tag.namespace();
                *ns_rows.entry(ns.to_string()).or_insert(0) += 1;
                ns_values
                    .entry(ns.to_string())
                    .or_default()
                    .insert(tag.value().to_string());
                if !seen_ns.contains(&ns) {
                    seen_ns.push(ns);
                    *ns_files.entry(ns.to_string()).or_insert(0) += 1;
                }
            }
        }
    }

    // A rebuild can orphan tags that no live file uses any more. Leaving them
    // would make `sagasu tags` list buckets that hold nothing.
    tx.execute(
        "DELETE FROM tags WHERE tag_id NOT IN (SELECT tag_id FROM file_tags)",
        [],
    )?;

    summary.distinct = tag_ids.len() as u64;
    for (ns, files) in ns_files {
        let rows = ns_rows.get(&ns).copied().unwrap_or(0);
        let distinct = ns_values.get(&ns).map_or(0, |v| v.len() as u64);
        summary.namespaces.insert(
            ns,
            NamespaceStat {
                files,
                distinct,
                rows,
            },
        );
    }

    let marker_ns = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    Store::meta_set_tx(&tx, "tag_marker", &marker_ns.to_string())?;
    Store::meta_set_tx(&tx, "tag_scan_generation", &scan_generation.to_string())?;
    Store::meta_set_tx(&tx, "tag_files", &summary.tagged.to_string())?;
    Store::meta_set_tx(&tx, "tag_rows", &summary.rows.to_string())?;
    if let Some(path) = &summary.rules_path {
        Store::meta_set_tx(&tx, "tag_rules", path)?;
    }
    if let Some(digest) = rules.digest() {
        Store::meta_set_tx(&tx, "tag_rules_digest", digest)?;
    }

    tx.commit()?;
    store.wal_checkpoint()?;

    summary.elapsed_secs = t0.elapsed().as_secs_f64();
    Ok(summary)
}

/// The leading bytes to tag a file with, reading them if asked to.
fn resolve_magic(
    store: &Store,
    row: &FileRow,
    config: &TagConfig,
    summary: &mut TagSummary,
) -> Option<Vec<u8>> {
    if let Some(magic) = &row.magic {
        summary.magic_present += 1;
        return Some(magic.clone());
    }
    if !config.read_magic || row.size <= 0 || row.size as u64 > config.magic_max_size {
        summary.magic_missing += 1;
        return None;
    }
    match read_head(&row.path, MAGIC_LEN) {
        Some(head) if !head.is_empty() => {
            // Persist it: the column exists for exactly this, and the next pass
            // (or `sagasu tags --file`) then needs no I/O at all.
            let _ = store.update_magic(row.file_id, &head);
            summary.magic_read += 1;
            Some(head)
        }
        Some(_) => {
            summary.magic_missing += 1;
            None
        }
        None => {
            summary.magic_unreadable += 1;
            None
        }
    }
}

/// Read at most `len` leading bytes of a file.
fn read_head(path: &str, len: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; len];
    let mut filled = 0usize;
    loop {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => {
                filled += n;
                if filled == len {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    buf.truncate(filled);
    Some(buf)
}

/// Get (or create) the row id of a tag.
fn intern_tag(tx: &rusqlite::Transaction<'_>, tag: &Tag) -> Result<i64> {
    tx.prepare_cached("INSERT OR IGNORE INTO tags (namespace, value) VALUES (?1, ?2)")?
        .execute(params![tag.namespace(), tag.value()])?;
    let id: i64 = tx
        .prepare_cached("SELECT tag_id FROM tags WHERE namespace=?1 AND value=?2")?
        .query_row(params![tag.namespace(), tag.value()], |row| row.get(0))?;
    Ok(id)
}

// ── Queries ─────────────────────────────────────────────────────────────────

/// Restricts a `file_tags` query to files that are still live.
///
/// `file_tags` is only rebuilt by `sagasu tag`, but tombstones are created by
/// `sagasu index`, so between a crawl and the next tag pass the junction table
/// holds rows for files the index already knows are gone. Every count a user
/// reads has to exclude them or the facet tree disagrees with the drill-down it
/// leads to: measured on a three-file corpus with two deleted and re-crawled,
/// `sagasu tags kind:` offered `3 kind:text` while `sagasu tags kind:text`
/// answered `1 of 1 files`. A facet bucket that promises three and delivers one
/// is worse than one that was never offered.
///
/// Shared as one string so the aggregate queries, [`files_with_tags`] and the
/// drill-down in [`crate::browse`] cannot drift apart on what "live" means.
pub(crate) const LIVE_FILES_JOIN: &str =
    "JOIN files f ON f.file_id = ft.file_id AND f.deleted_at IS NULL";

/// A tag and the number of live files carrying it.
#[derive(Debug, Clone)]
pub struct TagCount {
    /// The tag.
    pub tag: Tag,
    /// Live files carrying it.
    pub files: i64,
}

/// A namespace and its totals.
#[derive(Debug, Clone)]
pub struct NamespaceCount {
    /// The namespace.
    pub namespace: String,
    /// Distinct values in it.
    pub distinct: i64,
    /// Live files carrying at least one tag in it.
    pub files: i64,
}

/// Namespaces present in the index, most-used first, then alphabetical.
///
/// Counts live files only — see [`LIVE_FILES_JOIN`].
pub fn namespace_counts(store: &Store) -> Result<Vec<NamespaceCount>> {
    let mut stmt = store.conn().prepare(&format!(
        "SELECT t.namespace, COUNT(DISTINCT t.tag_id), COUNT(DISTINCT ft.file_id)
         FROM tags t JOIN file_tags ft ON ft.tag_id = t.tag_id
         {LIVE_FILES_JOIN}
         GROUP BY t.namespace
         ORDER BY COUNT(DISTINCT ft.file_id) DESC, t.namespace",
    ))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(NamespaceCount {
                namespace: row.get(0)?,
                distinct: row.get(1)?,
                files: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Tags ordered by file count, optionally restricted to one namespace.
///
/// The secondary sort is the tag itself, so two tags with the same count always
/// come back in the same order — a facet list that reshuffles between runs is
/// as disorienting as one that changes content.
/// Counts live files only — see [`LIVE_FILES_JOIN`].
pub fn tag_counts(store: &Store, namespace: Option<&str>, limit: i64) -> Result<Vec<TagCount>> {
    let sql = format!(
        "SELECT t.namespace, t.value, COUNT(ft.file_id) AS n
         FROM tags t JOIN file_tags ft ON ft.tag_id = t.tag_id
         {LIVE_FILES_JOIN}
         WHERE (?1 IS NULL OR t.namespace = ?1)
         GROUP BY t.tag_id
         ORDER BY n DESC, t.namespace, t.value
         LIMIT ?2"
    );
    let mut stmt = store.conn().prepare(&sql)?;
    let rows = stmt
        .query_map(params![namespace, limit], |row| {
            let ns: String = row.get(0)?;
            let value: String = row.get(1)?;
            Ok((ns, value, row.get::<_, i64>(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(rows
        .into_iter()
        .filter_map(|(ns, value, files)| {
            Tag::new(&ns, &value)
                .ok()
                .map(|tag| TagCount { tag, files })
        })
        .collect())
}

/// Drop repeated tags, keeping the first occurrence.
///
/// `sagasu tags kind:image kind:image` is a filter for one tag written twice,
/// not an unsatisfiable one — but the `HAVING COUNT(DISTINCT t.tag_id) = N` test
/// below counts *distinct* tag ids against the number of arguments, so a repeat
/// makes the condition permanently unreachable and the answer a confident
/// `hits: 0`. Removing the duplicate at the door is the only place the two
/// numbers can be brought back into agreement.
pub(crate) fn dedup_tags(tags: &[Tag]) -> Vec<Tag> {
    let mut seen: std::collections::HashSet<&Tag> = std::collections::HashSet::new();
    tags.iter()
        .filter(|t| seen.insert(t))
        .cloned()
        .collect::<Vec<_>>()
}

/// The `file_id`s of live files carrying every one of `tags`, as a subquery and
/// its bound values. Parameters `1..=2n` are the tag halves and `2n+1` is the
/// count; a caller may bind `2n+2` for its own use.
fn all_tags_subquery(tags: &[Tag]) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let placeholders = (0..tags.len())
        .map(|i| format!("(?{}, ?{})", i * 2 + 1, i * 2 + 2))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT ft.file_id
         FROM file_tags ft
         JOIN tags t ON t.tag_id = ft.tag_id
         {LIVE_FILES_JOIN}
         WHERE (t.namespace, t.value) IN ({placeholders})
         GROUP BY ft.file_id
         HAVING COUNT(DISTINCT t.tag_id) = ?{}",
        tags.len() * 2 + 1
    );
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(tags.len() * 2 + 2);
    for tag in tags {
        values.push(Box::new(tag.namespace().to_string()));
        values.push(Box::new(tag.value().to_string()));
    }
    values.push(Box::new(tags.len() as i64));
    (sql, values)
}

/// The `file_id`s a facet selection covers, as a subquery and its bound values.
///
/// The same thing [`all_tags_subquery`] produces, except that an **empty**
/// selection is the whole live index rather than an error. That distinction is
/// the root of the drill-down in [`crate::browse`]: "nothing chosen yet" is a
/// legitimate place to stand, and it has to mean the same "live" as every step
/// below it — which is why it goes through this function and not a second,
/// separately-written `WHERE deleted_at IS NULL` somewhere else.
///
/// Parameters are numbered `1..=values.len()`; a caller binding its own (a
/// `LIMIT`, say) continues from `values.len() + 1`.
pub(crate) fn selection_subquery(tags: &[Tag]) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    if tags.is_empty() {
        return (
            "SELECT lf.file_id FROM files lf WHERE lf.deleted_at IS NULL".to_string(),
            Vec::new(),
        );
    }
    all_tags_subquery(&dedup_tags(tags))
}

/// Live files matching a selection, ordered by `file_id`, at most `limit`.
///
/// An empty selection means the whole live index — see [`selection_subquery`].
/// Pair this with [`count_selection`]: the length of what comes back is the size
/// of the *page*, and reporting that as the number of matches is the same
/// silent-omission failure the freshness design exists to prevent.
pub fn files_in_selection(store: &Store, tags: &[Tag], limit: i64) -> Result<Vec<FileRow>> {
    let (sub, mut values) = selection_subquery(tags);
    let sql = format!(
        "SELECT f.file_id, f.path, f.ext, f.size, f.mtime_ns, f.ctime_ns, f.magic, f.blake3,
                f.fs_id, f.last_seen_scan, f.deleted_at
         FROM files f
         WHERE f.file_id IN ({sub})
         ORDER BY f.file_id
         LIMIT ?{}",
        values.len() + 1
    );
    values.push(Box::new(limit));

    let mut stmt = store.conn().prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = values.iter().map(|b| b.as_ref()).collect();
    let rows = stmt
        .query_map(params.as_slice(), crate::store::row_to_file_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Live files carrying **all** of `tags`, ordered by `file_id`, at most `limit`.
///
/// Rejects an empty tag list: `sagasu tags` with no arguments lists namespaces,
/// so reaching here with none is a caller bug rather than a request for the
/// whole index. [`files_in_selection`] is the form that accepts it.
pub fn files_with_tags(store: &Store, tags: &[Tag], limit: i64) -> Result<Vec<FileRow>> {
    if tags.is_empty() {
        bail!("no tags given");
    }
    files_in_selection(store, tags, limit)
}

/// Split index rows by whether the file is still on disk *right now*.
///
/// ## Why a tag query has to do this at all
///
/// No delta source can report a deletion: the mtime walk only sees what is
/// there, and a file that is gone leaves nothing to enumerate (design.md §5).
/// So the freshness design handles deletion on the *other* side of the merge —
/// every index hit that survives the changed-set test is checked for existence
/// before it is returned ([`crate::fresh::find`] does exactly this). A tag
/// filter answers from the same index rows and is subject to the same gap: with
/// no check, `sagasu tags kind:text` happily lists paths that were deleted
/// after the crawl, while `sagasu find` at the same instant drops them.
///
/// The check is bounded by the size of the page, not by the corpus — the caller
/// applies it to the rows it is about to print.
///
/// Returns `(still on disk, gone)`, both in the input order.
pub fn partition_existing(rows: Vec<FileRow>) -> (Vec<FileRow>, Vec<FileRow>) {
    rows.into_iter()
        .partition(|row| Path::new(&row.path).exists())
}

/// How many live files carry **all** of `tags` — the total behind the page
/// [`files_with_tags`] returns.
///
/// "Live" here is the *index's* notion: not tombstoned. A file deleted since the
/// last crawl is still counted, because finding that out costs one `stat` per
/// row and this is a whole-corpus aggregate. Callers must present it as an upper
/// bound; [`partition_existing`] is what turns the printed page into a number
/// that has actually been checked.
pub fn count_files_with_tags(store: &Store, tags: &[Tag]) -> Result<i64> {
    if tags.is_empty() {
        bail!("no tags given");
    }
    count_selection(store, tags)
}

/// How many live files a selection matches. An empty selection is the whole
/// live index — see [`selection_subquery`].
///
/// Same caveat as [`count_files_with_tags`]: this is the *index's* notion of
/// live, so it is an upper bound until the rows are checked against the
/// filesystem by [`partition_existing`].
pub fn count_selection(store: &Store, tags: &[Tag]) -> Result<i64> {
    let (sub, values) = selection_subquery(tags);
    let sql = format!("SELECT COUNT(*) FROM ({sub})");
    let params: Vec<&dyn rusqlite::ToSql> = values.iter().map(|b| b.as_ref()).collect();
    let n = store
        .conn()
        .query_row(&sql, params.as_slice(), |row| row.get(0))?;
    Ok(n)
}

/// How many distinct tags [`tag_counts`] would have to choose from, before its
/// `limit` cuts the list. A facet list that shows twenty of a hundred and eleven
/// buckets without saying so reads as the whole axis.
/// Counts live files only — see [`LIVE_FILES_JOIN`].
pub fn tag_counts_total(store: &Store, namespace: Option<&str>) -> Result<i64> {
    let n = store.conn().query_row(
        &format!(
            "SELECT COUNT(*) FROM (
                 SELECT t.tag_id FROM tags t JOIN file_tags ft ON ft.tag_id = t.tag_id
                 {LIVE_FILES_JOIN}
                 WHERE (?1 IS NULL OR t.namespace = ?1)
                 GROUP BY t.tag_id)"
        ),
        params![namespace],
        |row| row.get(0),
    )?;
    Ok(n)
}

/// The stored tags of one file, with their source masks, in tag order.
pub fn tags_of_file(store: &Store, file_id: i64) -> Result<Vec<(Tag, u32)>> {
    let mut stmt = store.conn().prepare(
        "SELECT t.namespace, t.value, ft.sources
         FROM file_tags ft JOIN tags t ON t.tag_id = ft.tag_id
         WHERE ft.file_id = ?1
         ORDER BY t.namespace, t.value",
    )?;
    let rows = stmt
        .query_map(params![file_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u32,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(ns, value, sources)| Tag::new(&ns, &value).ok().map(|t| (t, sources)))
        .collect())
}

/// Look up a live file row by exact path.
pub fn file_by_path(store: &Store, path: &str) -> Result<Option<FileRow>> {
    let tx = store.begin_tx()?;
    let row = store.find_by_path(&tx, path)?;
    Ok(row)
}

/// Compute the tags a path *would* get, without consulting the tag tables.
///
/// This is the debugging surface for rule files: it answers "why does this file
/// have these tags" from the engine itself rather than from whatever a previous
/// build happened to store, so a rule can be checked before a full pass is run.
pub fn explain(
    path: &Path,
    root: Option<&str>,
    rules: &RuleSet,
    read_magic: bool,
) -> Result<tags::TagSet> {
    let path_str = path.to_string_lossy().into_owned();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .filter(|e| !e.is_empty());
    let magic = read_magic
        .then(|| read_head(&path_str, MAGIC_LEN))
        .flatten();

    Ok(tags::tags_for(
        &tags::FileFacts {
            path: &path_str,
            root,
            ext: ext.as_deref(),
            magic: magic.as_deref(),
        },
        rules,
    ))
}
