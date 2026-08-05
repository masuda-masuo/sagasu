//! sagasu-core: parallel metadata crawl + SQLite index (schema v0).
//!
//! This crate provides:
//! - [`Store`]: SQLite-backed file metadata index with stable file IDs,
//!   tombstone semantics, and content-hash columns.
//! - [`walk`]: parallel filesystem walker (via the `ignore` crate) with
//!   built-in directory exclusion and rescan-diff against an existing store.

pub mod store;
pub mod walk;

pub use store::Store;
pub use walk::{crawl, CrawlConfig, CrawlSummary, hash_backfill, HashSummary};
