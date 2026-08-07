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

pub mod fulltext;
pub mod store;
pub mod text;
pub mod walk;

pub use fulltext::{FulltextConfig, FulltextSummary, SearchConfig, SearchHit, SearchOutcome};
pub use store::Store;
pub use walk::{crawl, hash_backfill, CrawlConfig, CrawlSummary, HashSummary};
