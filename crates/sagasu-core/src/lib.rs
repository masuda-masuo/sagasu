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
//! - [`fulltext`]: tantivy index build + search, keyed by the schema-v0
//!   `file_id` so full-text hits resolve back to metadata rows.
//! - [`delta`]: the search-time delta sources (USN Journal on Windows, `stat`
//!   walk everywhere else) and the point-in-time marker that anchors them.
//! - [`fresh`]: the delta merge — the index result overlaid with a live scan of
//!   whatever changed since the marker, so a stale index still gives a fresh
//!   answer (design.md §5).

pub mod delta;
pub mod fresh;
pub mod fulltext;
pub mod store;
pub mod text;
#[cfg(windows)]
pub mod usn;
pub mod walk;

pub use delta::{DeltaCache, DeltaSet, DeltaSource, DeltaStatus, ScanMarker};
pub use fresh::{FreshConfig, FreshHit, FreshOutcome};
pub use fulltext::{FulltextConfig, FulltextSummary, SearchConfig, SearchHit, SearchOutcome};
pub use store::Store;
pub use walk::{crawl, hash_backfill, CrawlConfig, CrawlSummary, ExcludeSet, HashSummary};
