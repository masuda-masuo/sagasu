//! The machine-readable rendering (`--json`, docs/cli.md §4).
//!
//! ## Who this is for
//!
//! Shell scripts, other tools, agents — callers on the far side of a process
//! boundary who cannot link against `sagasu-core`. **The M4 Tauri UI is not one
//! of them**: it links the core directly and calls e.g.
//! [`sagasu_core::browse::browse`] itself (design.md §3), so this is not a
//! second contract describing the core's structs. It is a second *rendering of
//! the CLI's output*, and its obligation is to the human-readable rendering
//! next to it, not to the core: every number and every sentence that appears on
//! screen appears here too (docs/cli.md §4-2).
//!
//! ## Why the conversions live here rather than on the core types
//!
//! Deriving `Serialize` on the core structs would be less code and would be
//! wrong. The core types are M4's internal interface; hanging JSON's concerns
//! off them (field naming, the 2^53 problem, which of two names for a count is
//! the public one) puts two contracts on one type, and the moment they disagree
//! the compiler has nothing to say about it. The cost of doing it here is a
//! function per command; the benefit is that changing the JSON cannot change
//! what M4 sees, and vice versa.
//!
//! ## Two shapes
//!
//! Result streams are JSON Lines — one event per line, `type` first. Summaries
//! are a single object. `search` emitting its hits one line at a time and
//! `index` emitting one object at the end is the same distinction rg makes, and
//! it follows from the data: a hit list has no length known in advance, a build
//! summary is one thing that happened.

use serde_json::{json, Map, Value};

use sagasu_core::browse::BrowseView;
use sagasu_core::config::ConfigOrigin;
use sagasu_core::delta::{DeltaStatus, ScanMarker};
use sagasu_core::fresh::{FreshOutcome, HitOrigin};
use sagasu_core::fulltext::{FulltextSummary, SearchOutcome};
use sagasu_core::store::{FileRow, IndexStats};
use sagasu_core::tagindex::TagSummary;
use sagasu_core::tags::{Tag, TagSource};
use sagasu_core::text::TextPolicy;
use sagasu_core::walk::{CrawlSummary, ExcludeSet, HashSummary};

use crate::output::{mib, Report, TagDelta, TagFreshnessReport};

/// The schema version every rendering carries (docs/cli.md §4-3).
///
/// While this string is unchanged, fields are only ever *added*. A removal, a
/// rename or a change of meaning bumps it, and gets listed in the PR body.
pub(crate) const SCHEMA: &str = "v0";

// ── Emitting ────────────────────────────────────────────────────────────────

/// One JSON value on one line. `Value`'s `Display` is compact, which is what a
/// line-delimited stream needs.
fn line(value: Value) {
    println!("{value}");
}

/// The first line of every stream.
pub(crate) fn meta(command: &str, mut fields: Map<String, Value>) {
    let mut out = Map::new();
    out.insert("type".into(), json!("meta"));
    out.insert("schema".into(), json!(SCHEMA));
    out.insert("command".into(), json!(command));
    out.append(&mut fields);
    line(Value::Object(out));
}

/// The whole of a summary-shaped command's output.
pub(crate) fn summary(command: &str, mut fields: Map<String, Value>, report: &Report) {
    let mut out = Map::new();
    out.insert("schema".into(), json!(SCHEMA));
    out.insert("command".into(), json!(command));
    out.append(&mut fields);
    out.insert("warnings".into(), json!(report.warnings()));
    line(Value::Object(out));
}

/// The trailing `warning` events of a stream-shaped command's output.
///
/// Emitted last, matching where the human rendering puts them, and repeating
/// what stderr already said (docs/cli.md §4-2 — neither channel substitutes for
/// the other).
pub(crate) fn warnings(report: &Report) {
    for message in report.warnings() {
        line(json!({"type": "warning", "message": message}));
    }
}

/// Convenience: build the field map of an object literal.
fn fields(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

// ── Shared fragments ────────────────────────────────────────────────────────

fn tag_value(tag: &Tag) -> Value {
    json!({"tag": tag.to_string(), "namespace": tag.namespace(), "value": tag.value()})
}

fn tags_value(tags: &[Tag]) -> Value {
    Value::Array(tags.iter().map(tag_value).collect())
}

fn delta_status(status: &DeltaStatus) -> (&'static str, Value, Value) {
    match status {
        DeltaStatus::Complete => ("complete", Value::Null, Value::Null),
        DeltaStatus::Truncated { limit } => ("truncated", json!(limit), Value::Null),
        DeltaStatus::RescanRequired(reason) => {
            ("rescan_required", Value::Null, json!(reason.as_str()))
        }
    }
}

fn config_origin(origin: &ConfigOrigin) -> Value {
    match origin {
        ConfigOrigin::Explicit(p) => json!({"path": p.display().to_string(), "found": "named"}),
        ConfigOrigin::Discovered(p) => {
            json!({"path": p.display().to_string(), "found": "discovered"})
        }
        ConfigOrigin::None => json!({"path": Value::Null, "found": "none"}),
    }
}

fn text_policy(policy: &TextPolicy) -> Value {
    json!({
        "describe": policy.describe(),
        "source": policy.source().map(|p| p.display().to_string()),
        "digest": policy.digest(),
        "text_ext": policy.text_exts(),
        "binary_ext": policy.binary_exts(),
    })
}

fn exclusion(excludes: &ExcludeSet) -> Value {
    json!({
        "names": excludes.names(),
        "hidden": excludes.hidden_policy().as_str(),
        "gitignore": {
            "applied": excludes.uses_gitignore(),
            "rules": excludes.gitignore_rules(),
            "digest": excludes.gitignore_digest(),
        },
    })
}

fn file_row(row: &FileRow, exists: bool) -> Value {
    json!({"type": "file", "file_id": row.file_id, "path": row.path, "exists": exists})
}

// ── search / find ───────────────────────────────────────────────────────────

/// The `delta` / `timing` / `merge` / `hit` events shared by `search` and
/// `find`, in the order the human rendering prints them.
pub(crate) fn fresh(outcome: &FreshOutcome) {
    if let Some(d) = &outcome.delta {
        let (status, limit, reason) = delta_status(&d.status);
        line(json!({
            "type": "delta",
            "entries": d.entries,
            "source": d.kind.as_str(),
            "cached": d.cached,
            "scanned": d.scanned,
            "excluded": d.excluded,
            "errors": d.errors,
            "detects_renames": d.detects_renames,
            "status": status,
            "truncated_at": limit,
            "rescan_reason": reason,
        }));
    }

    let t = &outcome.timing;
    line(json!({
        "type": "timing",
        "setup_ms": t.setup_ms,
        "index_ms": t.index_ms,
        "delta_ms": t.delta_ms,
        "live_ms": t.live_ms,
        "merge_ms": t.merge_ms,
        "overhead_ms": t.overhead_ms(),
        "total_ms": t.total_ms,
    }));
    line(json!({
        "type": "merge",
        "index_candidates": outcome.index_candidates,
        "dropped_changed": outcome.dropped_changed,
        "dropped_deleted": outcome.dropped_deleted,
    }));

    for hit in &outcome.hits {
        line(json!({
            "type": "hit",
            "origin": hit.origin.as_str(),
            "file_id": hit.file_id,
            "path": hit.path,
            "score": hit.score,
            "size": hit.size,
            "mtime_ns": hit.mtime_ns,
            "age_secs": hit.mtime_ns.map(crate::output::age_secs),
            "snippet": hit.snippet,
        }));
    }
    let _ = HitOrigin::Index;
}

/// `sagasu search` over the merged path.
pub(crate) fn search(
    query: &str,
    db: &str,
    index_dir: &str,
    outcome: &FreshOutcome,
    fresh_on: bool,
) {
    meta(
        "search",
        fields(json!({
            "query": query,
            "db": db,
            "index_dir": index_dir,
            "fresh": fresh_on,
            "text_policy": text_policy(&outcome.text_policy),
        })),
    );
    line(json!({
        "type": "summary",
        "hits": outcome.hits.len(),
        "live_hits": outcome.live_hits,
        "index_hits": outcome.hits.len() - outcome.live_hits,
        "total_docs": outcome.total_docs,
    }));
    fresh(outcome);
}

/// `sagasu search` with no metadata index behind it.
pub(crate) fn search_index_only(query: &str, index_dir: &str, outcome: &SearchOutcome) {
    meta(
        "search",
        fields(json!({
            "query": query,
            "db": Value::Null,
            "index_dir": index_dir,
            "fresh": false,
            "text_policy": Value::Null,
        })),
    );
    line(json!({
        "type": "summary",
        "hits": outcome.hits.len(),
        "live_hits": 0,
        "index_hits": outcome.hits.len(),
        "total_docs": outcome.total_docs,
    }));
    line(json!({
        "type": "timing",
        "setup_ms": 0.0,
        "index_ms": outcome.match_ms,
        "delta_ms": 0.0,
        "live_ms": 0.0,
        "merge_ms": 0.0,
        "overhead_ms": 0.0,
        "total_ms": outcome.elapsed_ms,
    }));
    for hit in &outcome.hits {
        line(json!({
            "type": "hit",
            "origin": "index",
            "file_id": hit.file_id,
            "path": hit.display_path(),
            "indexed_path": hit.indexed_path,
            "deleted": hit.deleted,
            "score": hit.score,
            "size": Value::Null,
            "mtime_ns": hit.mtime_ns,
            "snippet": hit.snippet,
        }));
    }
}

/// `sagasu find`.
pub(crate) fn find(query: &str, db: &str, outcome: &FreshOutcome, fresh_on: bool) {
    meta(
        "find",
        fields(json!({"query": query, "db": db, "fresh": fresh_on})),
    );
    line(json!({
        "type": "summary",
        "hits": outcome.hits.len(),
        "live_hits": outcome.live_hits,
        "index_hits": outcome.hits.len() - outcome.live_hits,
    }));
    fresh(outcome);
}

// ── index / hash / fulltext ─────────────────────────────────────────────────

/// `sagasu index`.
///
/// One object at the end, where the human rendering prints the scope *before*
/// the crawl. That ordering exists so a person can hit Ctrl-C on a wrong root;
/// a program has the whole object before it acts either way.
pub(crate) fn index(
    root: &str,
    excludes: &ExcludeSet,
    summary_data: &CrawlSummary,
    report: &Report,
) {
    let mut skipped = Map::new();
    for (name, count) in &summary_data.skipped {
        skipped.insert(name.clone(), json!(count));
    }
    summary(
        "index",
        fields(json!({
            "root": root,
            "exclusion": exclusion(excludes),
            "scanned": summary_data.scanned,
            "indexed": summary_data.indexed,
            "added": summary_data.added,
            "changed": summary_data.changed,
            "renamed": summary_data.renamed,
            "deleted": summary_data.deleted,
            "skipped": Value::Object(skipped),
            "skipped_total": summary_data.skipped_total(),
            "skipped_hidden": summary_data.skipped_hidden,
            "skipped_gitignore": summary_data.skipped_gitignore,
            "unreadable": summary_data.errors,
            "unreadable_samples": summary_data.error_samples,
            "elapsed_secs": summary_data.elapsed_secs,
        })),
        report,
    );
}

/// `sagasu hash`.
pub(crate) fn hash(summary_data: &HashSummary, report: &Report) {
    summary(
        "hash",
        fields(json!({
            "hashed": summary_data.hashed,
            "skipped_too_large": summary_data.skipped_too_large,
            "skipped_unreadable": summary_data.skipped_unreadable,
        })),
        report,
    );
}

/// `sagasu fulltext`.
pub(crate) fn fulltext(
    index_dir: &str,
    origin: &ConfigOrigin,
    policy: &TextPolicy,
    summary_data: &FulltextSummary,
    report: &Report,
) {
    let mut skipped = Map::new();
    for (reason, count) in &summary_data.skipped {
        skipped.insert(reason.as_str().to_string(), json!(count));
    }
    let skipped_exts: Vec<Value> = summary_data
        .skipped_exts
        .iter()
        .map(|(ext, count)| json!({"ext": ext, "files": count}))
        .collect();
    let extract_errors: Vec<Value> = summary_data
        .extract_errors
        .iter()
        .map(|(path, reason)| json!({"path": path, "reason": reason}))
        .collect();

    summary(
        "fulltext",
        fields(json!({
            "index_dir": index_dir,
            "config": config_origin(origin),
            "text_policy": text_policy(policy),
            "candidates": summary_data.candidates,
            "indexed": summary_data.indexed,
            "accepted_by_ext": summary_data.accepted_by_ext,
            "accepted_by_sniff": summary_data.accepted_by_sniff,
            "accepted_by_extract": summary_data.accepted_by_extract,
            "skipped": Value::Object(skipped),
            "skipped_total": summary_data.skipped_total(),
            "skipped_exts": skipped_exts,
            "extract_errors": extract_errors,
            "text_bytes": summary_data.text_bytes,
            "text_mib": mib(summary_data.text_bytes),
            "index_bytes": summary_data.index_bytes,
            "index_mib": mib(summary_data.index_bytes),
            "index_ratio_pct": if summary_data.text_bytes > 0 {
                json!(100.0 * summary_data.index_bytes as f64 / summary_data.text_bytes as f64)
            } else {
                Value::Null
            },
            "elapsed_secs": summary_data.elapsed_secs,
        })),
        report,
    );
}

// ── tag / tags / browse ─────────────────────────────────────────────────────

/// `sagasu tag`.
pub(crate) fn tag(
    origin: &ConfigOrigin,
    summary_data: &TagSummary,
    read_magic: bool,
    read_embedded: bool,
    report: &Report,
) {
    let namespaces: Vec<Value> = summary_data
        .namespaces
        .iter()
        .map(|(ns, stat)| {
            json!({
                "namespace": ns,
                "files": stat.files,
                "coverage_pct": summary_data.namespace_coverage(ns),
                "distinct": stat.distinct,
                "rows": stat.rows,
            })
        })
        .collect();
    let capped_namespaces: Vec<Value> = summary_data
        .capped_dropped_namespaces
        .iter()
        .map(|(ns, n)| json!({"namespace": ns, "dropped": n}))
        .collect();
    let embedded_errors: Vec<Value> = summary_data
        .embedded_errors
        .iter()
        .map(|(path, reason)| json!({"path": path, "reason": reason}))
        .collect();

    summary(
        "tag",
        fields(json!({
            "config": config_origin(origin),
            "rules_count": summary_data.rules_count,
            "files": summary_data.files,
            "tagged": summary_data.tagged,
            "coverage_pct": summary_data.coverage(),
            "tagged_semantic": summary_data.tagged_semantic,
            "semantic_coverage_pct": summary_data.semantic_coverage(),
            "rows": summary_data.rows,
            "distinct": summary_data.distinct,
            "scan_generation": summary_data.scan_generation,
            "magic": {
                "read": read_magic,
                "present": summary_data.magic_present,
                "missing": summary_data.magic_missing,
                "read_now": summary_data.magic_read,
                "unreadable": summary_data.magic_unreadable,
            },
            "embedded": {
                "read": read_embedded,
                "candidates": summary_data.embedded_candidates,
                "with_metadata": summary_data.embedded_read,
                "failed": summary_data.embedded_failed,
                "errors": embedded_errors,
            },
            "capped": {
                "files": summary_data.capped,
                "tags_dropped": summary_data.capped_dropped,
                "by_namespace": capped_namespaces,
            },
            "namespaces": namespaces,
            "elapsed_secs": summary_data.elapsed_secs,
        })),
        report,
    );
}

/// The `tag_layer` and `delta` events `tags` and `browse` share.
pub(crate) fn tag_layer(r: &TagFreshnessReport) {
    line(json!({
        "type": "tag_layer",
        "built": r.built,
        "rows": r.rows,
        "files": r.files,
        "distinct": r.distinct,
        "generation": r.generation,
        "scan_generation": r.scan_generation,
        "behind": r.behind,
        "rules": r.rules,
        // Said in the machine rendering too: "level with the index" and "level
        // with the filesystem" are different claims, and only the first is
        // knowable from the database.
        "snapshot": "tags describe the corpus as of that scan; files created or \
                     renamed since carry no tags and are not merged in",
    }));

    let event = match &r.delta {
        TagDelta::NotProbed => json!({"type": "delta", "probed": false, "reason": "--no-fresh"}),
        TagDelta::NoMarker => json!({
            "type": "delta", "probed": false, "reason": "no freshness marker in the index"
        }),
        TagDelta::Failed(e) => json!({"type": "delta", "probed": false, "reason": e}),
        TagDelta::Probed {
            entries,
            source,
            scanned,
            excluded,
            status,
        } => {
            let (status, limit, reason) = delta_status(status);
            json!({
                "type": "delta",
                "probed": true,
                "entries": entries,
                "source": source,
                "scanned": scanned,
                "excluded": excluded,
                "status": status,
                "truncated_at": limit,
                "rescan_reason": reason,
            })
        }
    };
    line(event);
}

/// `sagasu tags` — the file-listing mode.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tags_files(
    db: &str,
    selection: &[Tag],
    freshness: &TagFreshnessReport,
    rows: &[FileRow],
    gone: &[FileRow],
    total: i64,
    fetched: i64,
) {
    meta(
        "tags",
        fields(json!({"db": db, "mode": "files", "tags": tags_value(selection)})),
    );
    tag_layer(freshness);
    line(json!({
        "type": "summary",
        "hits": rows.len(),
        // The index count was never existence-checked, so it can only ever be
        // an upper bound. Named as one rather than left to read as a fact.
        "total": total,
        "total_is_upper_bound": true,
        "page": fetched,
        "dropped": gone.len(),
    }));
    for row in rows {
        line(file_row(row, true));
    }
    for row in gone {
        line(file_row(row, false));
    }
}

/// `sagasu tags` — the namespace / value listing modes.
pub(crate) fn tags_counts(
    db: &str,
    mode: &str,
    namespace: Option<&str>,
    freshness: &TagFreshnessReport,
    namespaces: &[sagasu_core::tagindex::NamespaceCount],
    counts: &[sagasu_core::tagindex::TagCount],
    total: i64,
) {
    meta(
        "tags",
        fields(json!({"db": db, "mode": mode, "namespace": namespace})),
    );
    tag_layer(freshness);
    for ns in namespaces {
        line(json!({
            "type": "namespace",
            "namespace": ns.namespace,
            "files": ns.files,
            "distinct": ns.distinct,
        }));
    }
    for tc in counts {
        let mut event = fields(tag_value(&tc.tag));
        event.insert("type".into(), json!("tag_count"));
        event.insert("files".into(), json!(tc.files));
        line(Value::Object(event));
    }
    line(json!({
        "type": "summary",
        "shown": counts.len(),
        "total": total,
    }));
}

/// `sagasu tags --file` — the explain mode.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tags_explain(
    db: &str,
    path: &str,
    origin: &ConfigOrigin,
    freshness: &TagFreshnessReport,
    file_id: Option<i64>,
    stored: &[(Tag, u32)],
    computed: &[(Tag, u32)],
    dropped: &[(Tag, u32)],
    capped: bool,
) {
    meta(
        "tags",
        fields(json!({
            "db": db,
            "mode": "explain",
            "path": path,
            "config": config_origin(origin),
            "file_id": file_id,
            "indexed": file_id.is_some(),
        })),
    );
    tag_layer(freshness);
    let entry = |kind: &str, (tag, sources): &(Tag, u32)| {
        let mut event = fields(tag_value(tag));
        event.insert("type".into(), json!(kind));
        event.insert("sources".into(), json!(TagSource::describe(*sources)));
        Value::Object(event)
    };
    for t in stored {
        line(entry("stored_tag", t));
    }
    for t in computed {
        line(entry("computed_tag", t));
    }
    for t in dropped {
        line(entry("dropped_tag", t));
    }
    line(json!({
        "type": "summary",
        "stored": stored.len(),
        "computed": computed.len(),
        "capped": capped,
        "dropped": dropped.len(),
    }));
}

/// `sagasu browse`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn browse(
    db: &str,
    view: &BrowseView,
    freshness: &TagFreshnessReport,
    next_command: Option<&str>,
    next_reason: Option<&str>,
    rows: &[FileRow],
    gone: &[FileRow],
    previewed: bool,
) {
    meta(
        "browse",
        fields(json!({"db": db, "selected": tags_value(&view.selected)})),
    );
    tag_layer(freshness);

    let label: Vec<Value> = view
        .label
        .iter()
        .map(|t| {
            json!({
                "tag": t.tag.to_string(),
                "weight": t.weight,
                "files": t.files,
                "corpus_files": t.corpus_files,
            })
        })
        .collect();
    line(json!({
        "type": "view",
        "matched": view.matched,
        "corpus": view.corpus,
        "share": if view.corpus > 0 { view.matched as f64 / view.corpus as f64 } else { 0.0 },
        "matched_is_upper_bound": true,
        "label": label,
        "label_vocabulary": view.label_vocabulary,
        "universal": tags_value(&view.universal),
        "axes_shown": view.axes.len(),
        "axes_total": view.axes_total,
        "axes_refining": view.axes_refining,
    }));

    for axis in &view.axes {
        let values: Vec<Value> = axis
            .values
            .iter()
            .map(|v| json!({"tag": v.tag.to_string(), "files": v.files, "share": v.share}))
            .collect();
        line(json!({
            "type": "axis",
            "namespace": axis.namespace,
            "score": axis.score,
            "coverage": axis.coverage,
            "files": axis.files,
            "distinct": axis.distinct,
            "tail_assignments": axis.tail_assignments,
            "multi_valued": crate::browse::is_multi_valued(axis),
            "values": values,
        }));
    }

    if previewed {
        for row in rows {
            line(file_row(row, true));
        }
        for row in gone {
            line(file_row(row, false));
        }
    }

    let next = match (&view.recommended, next_command, next_reason) {
        (Some(step), Some(command), _) => json!({
            "type": "next",
            "command": command,
            "tag": step.tag.to_string(),
            "namespace": step.namespace,
            "files": step.files,
            "share": step.share,
            "bits": step.bits,
            "reason": Value::Null,
        }),
        (_, command, reason) => json!({
            "type": "next",
            "command": command,
            "tag": Value::Null,
            "namespace": Value::Null,
            "files": Value::Null,
            "share": Value::Null,
            "bits": Value::Null,
            "reason": reason,
        }),
    };
    line(next);
}

// ── status ──────────────────────────────────────────────────────────────────

/// `sagasu status`.
pub(crate) fn status(stats: &IndexStats, exclusion_value: Value, unreadable: u64, report: &Report) {
    // 64-bit journal identifiers go out as strings: they are the one value in
    // this schema that can exceed 2^53, and a consumer reading the stream in
    // JavaScript would silently round them (docs/cli.md §4-3).
    let delta_marker = match &stats.delta_marker {
        Some(ScanMarker::Mtime { started_ns }) => json!({
            "kind": "mtime",
            "started_ns": started_ns.to_string(),
        }),
        Some(ScanMarker::Usn {
            volume,
            journal_id,
            next_usn,
            maximum_size,
            recorded_ns,
        }) => json!({
            "kind": "usn",
            "volume": volume,
            "journal_id": journal_id.to_string(),
            "next_usn": next_usn.to_string(),
            "maximum_size": maximum_size,
            "maximum_size_mib": mib(*maximum_size),
            "recorded_ns": recorded_ns.to_string(),
        }),
        None => json!({"kind": Value::Null}),
    };

    summary(
        "status",
        fields(json!({
            "root_path": stats.root_path,
            "schema_version": stats.schema_version,
            "scan_marker_age_secs": stats.scan_marker_ns.map(crate::output::age_secs),
            "delta_marker": delta_marker,
            "exclusion": exclusion_value,
            "unreadable": unreadable,
            "scan_generation": stats.scan_generation,
            "live_files": stats.live_count,
            "tombstones": stats.tombstone_count,
            "null_hashes": stats.null_hash_count,
            "fulltext": {
                "built": stats.fulltext_dir.is_some(),
                "dir": stats.fulltext_dir,
                "documents": stats.fulltext_docs.unwrap_or(0),
                "scan_generation": stats.fulltext_scan_generation,
                "behind": stats.fulltext_scan_generation
                    .map(|g| stats.scan_generation - g),
            },
            "tags": {
                "built": stats.tag_scan_generation.is_some(),
                "rows": stats.tag_rows,
                "files": stats.tag_files.unwrap_or(0),
                "distinct": stats.distinct_tags,
                "scan_generation": stats.tag_scan_generation,
                "behind": stats.tag_scan_generation.map(|g| stats.scan_generation - g),
                "rules": stats.tag_rules,
            },
            // The USN marker's remaining lifetime needs the journal's current
            // NextUsn, which means opening the volume. `status` stays read-only
            // unless asked; the design for the opt-in probe is docs/cli.md §9-1,
            // and it is not implemented.
            "journal": {"checked": false, "reason": "not implemented (docs/cli.md §9-1)"},
        })),
        report,
    );
}

/// The `exclusion` value of `status`, which has three states rather than one
/// shape — see `status::PolicyState`.
pub(crate) fn exclusion_state(
    state: &str,
    excludes: Option<&ExcludeSet>,
    detail: Option<&str>,
) -> Value {
    match excludes {
        Some(e) => {
            let mut map = fields(exclusion(e));
            map.insert("state".into(), json!(state));
            Value::Object(map)
        }
        None => json!({"state": state, "detail": detail}),
    }
}
