//! The webview-boundary types: a serializable mirror of the core browse tree.
//!
//! ## Why these exist at all
//!
//! `docs/cli.md` §8 draws one line and this module is on the far side of it:
//! the core types are M4's *internal* interface, so they carry no `Serialize`,
//! and anything that has to cross a serialization boundary is a separate type
//! that says so. A Tauri command returns its value to JavaScript through serde,
//! which would otherwise mean deriving `Serialize` on `BrowseView` — and with
//! it, on `FileRow`, `Tag`, and everything else the tree reaches. That single
//! derive would put two contracts (Rust callers, and a JSON wire format nobody
//! versions) on one set of structs, and the compiler would have nothing to say
//! the day they disagree.
//!
//! So: the core computes, this module transports.
//!
//! ## The one rule these conversions keep
//!
//! **Nothing is dropped silently.** Every conversion below destructures its
//! source struct rather than reading fields off it, so a new field on a core
//! type is a compile error here rather than a value that quietly stops reaching
//! the UI. That is the whole reason for the slightly awkward
//! `let Foo { a, b, c } = source;` shape in code that could have written
//! `source.a`.
//!
//! ## Two places the JSON is deliberately not the Rust type
//!
//! - **Nanosecond timestamps go out as decimal strings.** `mtime_ns` is around
//!   1.75e18 today; JavaScript's `Number.MAX_SAFE_INTEGER` is 9.0e15. A webview
//!   parsing them as numbers would round them, silently, and a file-modified
//!   time that is wrong in its last three digits looks exactly like one that is
//!   right. Same convention `docs/cli.md` §4-3 already applies to USNs.
//!   `size`, `file_id`, `last_seen_scan` and the scan generations stay numbers:
//!   none of them can reach 2^53 without the index being impossible.
//! - **Byte columns go out as lowercase hex.** `magic` / `blake3` / `fs_id` are
//!   `Vec<u8>`; serde's default rendering of that is an array of integers,
//!   which is four times the bytes and is not what anyone displays.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use sagasu_core::browse::{
    self, BrowseQuery, BrowseView, FacetAxis, FacetValue, LabelTerm, NextStep, TagLayerSnapshot,
};
use sagasu_core::store::FileRow;
use sagasu_core::tagindex;
use sagasu_core::tags::Tag;

// ── Query ───────────────────────────────────────────────────────────────────

/// [`BrowseQuery`], as the webview sends it.
///
/// Every field but `selected` has a default, so `{"selected": ["kind:image"]}`
/// is a complete request — the defaults are the core's own
/// (`browse::DEFAULT_*`), read from there rather than copied, so the UI and the
/// CLI cannot drift apart on what an unspecified query means.
///
/// `deny_unknown_fields` is load-bearing rather than tidy: a JavaScript caller
/// that sends `maxValues` or `previews` would otherwise get the *default* back
/// with no indication that its request was ignored, which is the quietly-wrong
/// answer this project keeps designing against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowseQueryDto {
    /// Tags already chosen, `namespace:value`, ANDed. Empty = the whole live
    /// index, which is where an exploration starts.
    #[serde(default)]
    pub selected: Vec<String>,
    /// Axes to return, after ranking.
    #[serde(default = "default_max_axes")]
    pub max_axes: usize,
    /// Values per axis — also the `m` of the ranking formula, so changing it
    /// changes *which* axes win, not only how many rows come back.
    #[serde(default = "default_max_values")]
    pub max_values: usize,
    /// Terms in the generated group label.
    #[serde(default = "default_label_terms")]
    pub label_terms: usize,
    /// Files to bring back as a preview of the selection. 0 = none.
    #[serde(default = "default_preview")]
    pub preview: usize,
}

fn default_max_axes() -> usize {
    browse::DEFAULT_MAX_AXES
}

fn default_max_values() -> usize {
    browse::DEFAULT_MAX_VALUES
}

fn default_label_terms() -> usize {
    browse::DEFAULT_LABEL_TERMS
}

fn default_preview() -> usize {
    browse::DEFAULT_PREVIEW
}

impl Default for BrowseQueryDto {
    fn default() -> Self {
        Self {
            selected: Vec::new(),
            max_axes: default_max_axes(),
            max_values: default_max_values(),
            label_terms: default_label_terms(),
            preview: default_preview(),
        }
    }
}

impl BrowseQueryDto {
    /// A query over `selected`, with the documented defaults.
    pub fn new(selected: Vec<String>) -> Self {
        Self {
            selected,
            ..Self::default()
        }
    }

    /// Parse into the core query.
    ///
    /// The only thing that can fail is a malformed tag, and the error names
    /// which one: a UI that sent five tags and got back
    /// `tag "kind image" is not in namespace:value form` cannot tell the user
    /// which chip to fix.
    pub fn to_core(&self) -> Result<BrowseQuery> {
        let selected = self
            .selected
            .iter()
            .map(|t| Tag::parse(t).with_context(|| format!("bad tag {t:?} in the browse query")))
            .collect::<Result<Vec<_>>>()?;
        // Written out field by field rather than with `..Default::default()`:
        // a new knob on `BrowseQuery` must fail to compile here, not silently
        // become un-settable from the UI.
        Ok(BrowseQuery {
            selected,
            max_axes: self.max_axes,
            max_values: self.max_values,
            label_terms: self.label_terms,
            preview: self.preview,
        })
    }
}

// ── Leaves ──────────────────────────────────────────────────────────────────

/// One `namespace:value` tag.
///
/// Carries the joined form *and* both halves. The joined form is the identity a
/// UI round-trips back in [`BrowseQueryDto::selected`]; the halves save every
/// consumer from re-splitting on a `:` that the value is allowed to contain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagDto {
    /// `namespace:value` — the form [`BrowseQueryDto`] accepts back.
    pub tag: String,
    pub namespace: String,
    pub value: String,
}

impl From<&Tag> for TagDto {
    fn from(tag: &Tag) -> Self {
        // `Tag`'s fields are private, so this is the one conversion here that
        // cannot be a destructuring guard; its whole surface is the two
        // accessors below plus `Display`, and all three are used.
        Self {
            tag: tag.to_string(),
            namespace: tag.namespace().to_string(),
            value: tag.value().to_string(),
        }
    }
}

fn tags(list: &[Tag]) -> Vec<TagDto> {
    list.iter().map(TagDto::from).collect()
}

/// Lowercase hex, for the byte columns. See the module docs.
fn hex(bytes: &Option<Vec<u8>>) -> Option<String> {
    bytes.as_ref().map(|b| {
        let mut out = String::with_capacity(b.len() * 2);
        for byte in b {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    })
}

/// A row of the `files` table.
///
/// Every column, not just the two the CLI prints: this is the boundary the M4
/// file list, its detail pane and its sort orders all read from, and a column
/// that never crosses it is a feature the frontend cannot have without a change
/// on this side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRowDto {
    pub file_id: i64,
    pub path: String,
    pub ext: Option<String>,
    pub size: i64,
    /// Decimal string — exceeds 2^53. See the module docs.
    pub mtime_ns: String,
    /// Decimal string — exceeds 2^53. See the module docs.
    pub ctime_ns: String,
    /// Lowercase hex.
    pub magic: Option<String>,
    /// Lowercase hex.
    pub blake3: Option<String>,
    /// Lowercase hex.
    pub fs_id: Option<String>,
    pub last_seen_scan: i64,
    /// Scan generation this row was tombstoned at. `None` = live.
    pub deleted_at: Option<i64>,
}

impl From<&FileRow> for FileRowDto {
    fn from(row: &FileRow) -> Self {
        let FileRow {
            file_id,
            path,
            ext,
            size,
            mtime_ns,
            ctime_ns,
            magic,
            blake3,
            fs_id,
            last_seen_scan,
            deleted_at,
        } = row;
        Self {
            file_id: *file_id,
            path: path.clone(),
            ext: ext.clone(),
            size: *size,
            mtime_ns: mtime_ns.to_string(),
            ctime_ns: ctime_ns.to_string(),
            magic: hex(magic),
            blake3: hex(blake3),
            fs_id: hex(fs_id),
            last_seen_scan: *last_seen_scan,
            deleted_at: *deleted_at,
        }
    }
}

fn rows(list: &[FileRow]) -> Vec<FileRowDto> {
    list.iter().map(FileRowDto::from).collect()
}

/// What the tag layer is a snapshot *of*.
///
/// Mandatory, and mandatory at every level: `docs/browse.md` §5 says a UI
/// "cannot render a view without receiving them", so this is a plain field of
/// [`BrowseViewDto`] rather than an `Option`, and the two derived facts a
/// consumer actually renders — has a tag layer ever been built, and how many
/// crawls has it fallen behind — travel with it rather than being left as
/// arithmetic the frontend might get wrong or skip.
///
/// `tag_scan_generation` is itself nullable because "never built" is a real
/// state; the *key* is always present, and [`TagLayerSnapshotDto::built`] says
/// the same thing without asking the reader to interpret `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagLayerSnapshotDto {
    /// Scan generation the tag layer was built from. `null` = never built.
    pub tag_scan_generation: Option<i64>,
    /// Scan generation of the metadata index right now.
    pub scan_generation: i64,
    /// Whether `sagasu tag` has ever run against this index.
    pub built: bool,
    /// Crawls that happened after the tag layer was built. Greater than 0 means
    /// every file indexed since is missing from the counts above.
    pub behind: i64,
}

impl From<&TagLayerSnapshot> for TagLayerSnapshotDto {
    fn from(snapshot: &TagLayerSnapshot) -> Self {
        let (built, behind) = (snapshot.built(), snapshot.behind());
        let TagLayerSnapshot {
            tag_scan_generation,
            scan_generation,
        } = snapshot;
        Self {
            tag_scan_generation: *tag_scan_generation,
            scan_generation: *scan_generation,
            built,
            behind,
        }
    }
}

// ── The facet tree ──────────────────────────────────────────────────────────

/// One offerable next step: a tag and how much of the selection it keeps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FacetValueDto {
    pub tag: TagDto,
    /// Live files in the selection carrying it. An index-side count, never
    /// existence-checked.
    pub files: i64,
    /// `files` as a share of the current selection.
    pub share: f64,
}

impl From<&FacetValue> for FacetValueDto {
    fn from(value: &FacetValue) -> Self {
        let FacetValue { tag, files, share } = value;
        Self {
            tag: TagDto::from(tag),
            files: *files,
            share: *share,
        }
    }
}

/// One candidate axis, scored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FacetAxisDto {
    pub namespace: String,
    /// Expected bits gained by reading `values` — the ranking score.
    pub score: f64,
    /// Share of the selection carrying *any* tag in this namespace. Reported to
    /// explain the score, not multiplied into it.
    pub coverage: f64,
    pub files: i64,
    /// Distinct values that would actually narrow the selection.
    pub distinct: i64,
    /// The top `max_values` of those, count-descending.
    pub values: Vec<FacetValueDto>,
    /// Tag assignments behind the values that did not fit. Non-zero means the
    /// axis has more to offer than is on screen, and the UI must say so.
    pub tail_assignments: i64,
}

impl From<&FacetAxis> for FacetAxisDto {
    fn from(axis: &FacetAxis) -> Self {
        let FacetAxis {
            namespace,
            score,
            coverage,
            files,
            distinct,
            values,
            tail_assignments,
        } = axis;
        Self {
            namespace: namespace.clone(),
            score: *score,
            coverage: *coverage,
            files: *files,
            distinct: *distinct,
            values: values.iter().map(FacetValueDto::from).collect(),
            tail_assignments: *tail_assignments,
        }
    }
}

/// The one value worth taking next, out of everything on screen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NextStepDto {
    pub namespace: String,
    pub tag: TagDto,
    /// Live files that would remain.
    pub files: i64,
    /// `files` as a share of the current selection.
    pub share: f64,
    /// Bits this single step resolves: `H(share, 1 − share)`.
    pub bits: f64,
}

impl From<&NextStep> for NextStepDto {
    fn from(step: &NextStep) -> Self {
        let NextStep {
            namespace,
            tag,
            files,
            share,
            bits,
        } = step;
        Self {
            namespace: namespace.clone(),
            tag: TagDto::from(tag),
            files: *files,
            share: *share,
            bits: *bits,
        }
    }
}

/// One term of a generated group label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabelTermDto {
    pub tag: TagDto,
    /// c-TF-IDF weight.
    pub weight: f64,
    /// Files in the selection carrying it.
    pub files: i64,
    /// Live files in the whole index carrying it.
    pub corpus_files: i64,
}

impl From<&LabelTerm> for LabelTermDto {
    fn from(term: &LabelTerm) -> Self {
        let LabelTerm {
            tag,
            weight,
            files,
            corpus_files,
        } = term;
        Self {
            tag: TagDto::from(tag),
            weight: *weight,
            files: *files,
            corpus_files: *corpus_files,
        }
    }
}

// ── The view ────────────────────────────────────────────────────────────────

/// [`BrowseView`], plus the existence check the core leaves to its caller.
///
/// Field-for-field the core view, with one addition: [`BrowseViewDto::dropped`].
/// See [`BrowseViewDto::from_view`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowseViewDto {
    /// The selection, deduplicated, in the order given.
    pub selected: Vec<TagDto>,
    /// Live files matching all of it. An index-side count and therefore an
    /// **upper bound**: only the previewed rows below have been checked against
    /// the filesystem, so a UI has to present this as an estimate.
    pub matched: i64,
    /// Live files in the whole index — the denominator of the corpus.
    pub corpus: i64,
    /// Machine-generated label for the selection, best term first.
    pub label: Vec<LabelTermDto>,
    /// Terms the label was chosen from, before `label_terms` cut it. A label
    /// showing 5 of 5 and one showing 5 of 900 deserve different trust.
    pub label_vocabulary: usize,
    /// Tags every file in the selection carries, excluding the selection
    /// itself. Not steps — choosing one returns the same group.
    pub universal: Vec<TagDto>,
    /// Ranked axes, best first.
    pub axes: Vec<FacetAxisDto>,
    /// Namespaces present anywhere in the selection.
    pub axes_total: usize,
    /// How many of those have at least one value that would narrow the
    /// selection.
    pub axes_refining: usize,
    /// The single value worth adding next. `null` when nothing is on screen to
    /// take.
    pub recommended: Option<NextStepDto>,
    /// First files of the selection, `file_id` order — **only those that still
    /// exist on disk**.
    pub preview: Vec<FileRowDto>,
    /// Previewed rows whose path no longer exists, in the same order they would
    /// have appeared in `preview`.
    ///
    /// Not a log line and not an `Option`: a webview cannot read stderr, so the
    /// only way "your index is stale, and here is the proof" reaches a person is
    /// as data. Empty is the ordinary case and means the check ran and found
    /// nothing — which is why it is a plain `Vec` that is always present rather
    /// than a field that appears when something went wrong.
    pub dropped: Vec<FileRowDto>,
    /// Freshness of the layer all of the above was read from.
    pub snapshot: TagLayerSnapshotDto,
}

impl BrowseViewDto {
    /// Convert a core view, existence-checking its preview rows.
    ///
    /// ## This touches the filesystem, and has to
    ///
    /// `docs/browse.md` §6: `browse()` reads the database and nothing else, and
    /// checking the previewed paths is explicitly the caller's job — the CLI
    /// does it in `sagasu-cli::browse`, and in the UI stack this layer is that
    /// caller. Deletion is the one change no delta source can report
    /// (design.md §5), so a row that is gone is indistinguishable from a live
    /// one until something calls `exists()` on it. The cost is bounded by the
    /// page, not by the corpus: at the default `preview` of 5, five `stat`s.
    ///
    /// The rows that fail land in [`BrowseViewDto::dropped`] rather than being
    /// filtered away, because "5 files, 2 of which are ghosts" and "3 files" are
    /// different answers and the difference is the freshness of the index.
    pub fn from_view(view: BrowseView) -> Self {
        let BrowseView {
            selected,
            matched,
            corpus,
            label,
            label_vocabulary,
            universal,
            axes,
            axes_total,
            axes_refining,
            recommended,
            preview,
            snapshot,
        } = view;
        // The same partition the CLI applies, from the same function, so the
        // terminal and the UI cannot disagree about which rows are real.
        let (present, gone) = tagindex::partition_existing(preview);
        Self {
            selected: tags(&selected),
            matched,
            corpus,
            label: label.iter().map(LabelTermDto::from).collect(),
            label_vocabulary,
            universal: tags(&universal),
            axes: axes.iter().map(FacetAxisDto::from).collect(),
            axes_total,
            axes_refining,
            recommended: recommended.as_ref().map(NextStepDto::from),
            preview: rows(&present),
            dropped: rows(&gone),
            snapshot: TagLayerSnapshotDto::from(&snapshot),
        }
    }

    /// Whether the selection is empty — the root of the hierarchy.
    pub fn is_root(&self) -> bool {
        self.selected.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_from_the_webview_needs_only_its_selection() {
        let dto: BrowseQueryDto = serde_json::from_str(r#"{"selected": ["kind:image"]}"#).unwrap();
        let query = dto.to_core().unwrap();
        assert_eq!(query.selected.len(), 1);
        assert_eq!(query.selected[0].to_string(), "kind:image");
        // …and the rest is the core's own defaults, not a second set copied
        // into this crate.
        assert_eq!(query.max_axes, browse::DEFAULT_MAX_AXES);
        assert_eq!(query.max_values, browse::DEFAULT_MAX_VALUES);
        assert_eq!(query.label_terms, browse::DEFAULT_LABEL_TERMS);
        assert_eq!(query.preview, browse::DEFAULT_PREVIEW);
        // An empty object is the root view.
        let root: BrowseQueryDto = serde_json::from_str("{}").unwrap();
        assert!(root.selected.is_empty());
        assert_eq!(root, BrowseQueryDto::default());
    }

    #[test]
    fn a_misspelt_field_is_rejected_rather_than_silently_defaulted() {
        // The failure this guards against: the caller believes it asked for 50
        // preview rows and gets 5, with nothing anywhere saying why.
        let err = serde_json::from_str::<BrowseQueryDto>(r#"{"previews": 50}"#).unwrap_err();
        assert!(err.to_string().contains("previews"), "{err}");
        // The correctly spelled field does what it says.
        let ok: BrowseQueryDto = serde_json::from_str(r#"{"preview": 50}"#).unwrap();
        assert_eq!(ok.to_core().unwrap().preview, 50);
    }

    #[test]
    fn a_malformed_tag_names_itself_in_the_error() {
        let dto = BrowseQueryDto::new(vec!["kind:image".into(), "not a tag".into()]);
        let err = dto.to_core().unwrap_err();
        assert!(err.to_string().contains("not a tag"), "{err}");
    }

    #[test]
    fn a_nanosecond_timestamp_survives_the_boundary_that_javascript_would_round() {
        // A real mtime: 2025-06-01T00:00:00Z in nanoseconds, well past 2^53.
        let ns = 1_748_736_000_000_000_000i64;
        assert!(ns > 2i64.pow(53));
        let row = FileRow {
            file_id: 7,
            path: "/tmp/a.txt".into(),
            ext: Some("txt".into()),
            size: 12,
            mtime_ns: ns,
            ctime_ns: ns,
            magic: Some(vec![0x89, 0x50, 0x4e, 0x47]),
            blake3: None,
            fs_id: None,
            last_seen_scan: 3,
            deleted_at: None,
        };
        let dto = FileRowDto::from(&row);
        assert_eq!(dto.mtime_ns, "1748736000000000000");
        assert_eq!(dto.magic.as_deref(), Some("89504e47"));
        assert_eq!(dto.blake3, None);
        // And the JSON really is a string, so a `JSON.parse` cannot round it.
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["mtime_ns"], serde_json::json!("1748736000000000000"));
        assert_eq!(json["size"], serde_json::json!(12));
    }

    #[test]
    fn the_snapshot_reports_never_built_without_asking_the_reader_to_read_a_null() {
        let never = TagLayerSnapshotDto::from(&TagLayerSnapshot {
            tag_scan_generation: None,
            scan_generation: 4,
        });
        assert!(!never.built);
        assert_eq!(never.behind, 0);
        assert_eq!(never.tag_scan_generation, None);

        let stale = TagLayerSnapshotDto::from(&TagLayerSnapshot {
            tag_scan_generation: Some(1),
            scan_generation: 4,
        });
        assert!(stale.built);
        assert_eq!(stale.behind, 3);
    }
}
