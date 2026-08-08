//! sagasu-core: parallel metadata crawl + SQLite index (schema v0) and the
//! tantivy/Lindera full-text index built on top of it.
//!
//! This crate provides:
//! - [`Store`]: SQLite-backed file metadata index with stable file IDs,
//!   tombstone semantics, and content-hash columns.
//! - [`walk`]: parallel filesystem walker (via the `ignore` crate) with
//!   built-in directory exclusion and rescan-diff against an existing store.
//! - [`text`]: body-extraction target decision (extension allowlist as an
//!   entrance, content sniffing as the fallback) and decoding.
//! - [`docmeta`]: the document formats that need a parser — `docx` / `xlsx` /
//!   `pptx` / `pdf` bodies, and the embedded metadata (OOXML properties, PDF
//!   info, EXIF) that feeds the tag engine (issue #40).
//! - [`fulltext`]: tantivy index build + search, keyed by the schema-v0
//!   `file_id` so full-text hits resolve back to metadata rows.
//! - [`delta`]: the search-time delta sources (USN Journal on Windows, `stat`
//!   walk everywhere else) and the point-in-time marker that anchors them.
//! - [`fresh`]: the delta merge — the index result overlaid with a live scan of
//!   whatever changed since the marker, so a stale index still gives a fresh
//!   answer (design.md §5).
//! - [`tags`]: the rule-based semantic layer — a *pure* generator of
//!   deterministic `namespace:value` tags from format, path and naming
//!   conventions (design.md §6).
//! - [`tagindex`]: the stateful half of that layer — building it into SQLite,
//!   measuring its coverage, and querying it back.
//! - [`tagrules`]: the declarative user rule file that feeds [`tags`].
//! - [`config`]: the one config file, `sagasu.toml` — the `[text]` section
//!   feeding [`text`] and the `[[tags.rule]]` tables feeding [`tagrules`]
//!   (issue #6, docs/cli.md §5).
//! - [`browse`]: the facet drill-down over that layer — given a tag selection,
//!   the next axes worth looking at, ranked by expected bits, with a c-TF-IDF
//!   label for the group (design.md §6, issue #5). This is the interface the
//!   M4 Tauri UI is meant to call directly; the CLI is a printer for it.

pub mod browse;
pub mod config;
pub mod delta;
pub mod docmeta;
pub mod fresh;
pub mod fulltext;
pub mod lattice;
pub mod store;
pub mod tagindex;
pub mod tagrules;
pub mod tags;
pub mod text;
#[cfg(windows)]
pub mod usn;
pub mod walk;

pub use browse::{BrowseQuery, BrowseView, FacetAxis, FacetValue, LabelTerm, NextStep};
pub use config::{Config, ConfigOrigin};
pub use delta::{DeltaCache, DeltaSet, DeltaSource, DeltaStatus, ScanMarker};
pub use docmeta::{BodyFormat, EmbeddedMeta, MetaFormat};
pub use fresh::{FreshConfig, FreshHit, FreshOutcome};
pub use fulltext::{FulltextConfig, FulltextSummary, SearchConfig, SearchHit, SearchOutcome};
pub use store::Store;
pub use tagindex::{TagConfig, TagSummary};
pub use tagrules::RuleSet;
pub use tags::{Tag, TagSet, TagSource};
pub use walk::{crawl, hash_backfill, CrawlConfig, CrawlSummary, ExcludeSet, HashSummary};
