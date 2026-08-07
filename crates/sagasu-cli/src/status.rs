//! `sagasu status` — what the index currently holds, and what it does not.
//!
//! A read-only report over the metadata database, printed in the order the
//! pipeline builds things: the crawl, then the freshness marker every search
//! replays against, then the full-text index, then the tag layer. The last two
//! each name the scan generation they were built from, because a derived index
//! that is a crawl or two behind answers as if the files added since do not
//! exist, and nothing else in this output would reveal that.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use sagasu_core::Store;

use crate::output::mib;

#[derive(Parser)]
pub struct StatusArgs {
    /// Path to the SQLite database file.
    #[arg(long, default_value = "index.db")]
    db: PathBuf,
}

pub fn cmd_status(args: StatusArgs) -> Result<()> {
    let store = Store::open(&args.db)?;
    let stats = store.get_stats()?;

    println!(
        "root path      : {}",
        stats.root_path.as_deref().unwrap_or("(none)")
    );
    println!("schema version : {}", stats.schema_version);

    if let Some(marker_ns) = stats.scan_marker_ns {
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let age_secs = ((now_ns - marker_ns) as f64) / 1e9;
        println!("scan marker    : {:.1}s ago", age_secs);
    } else {
        println!("scan marker    : (never scanned)");
    }

    // The freshness marker is what every search replays against. Which kind it
    // is decides how the delta is read back — and, for USN, whether it is still
    // valid at all (issue #16).
    match &stats.delta_marker {
        Some(marker @ sagasu_core::ScanMarker::Usn { volume, .. }) => {
            println!("delta marker   : usn on {volume}");
            // A live estimate needs the journal's current NextUsn, which means
            // opening the volume; `sagasu status` stays read-only and prints the
            // stored inputs so the estimate is at least reproducible by hand.
            if let sagasu_core::ScanMarker::Usn {
                maximum_size,
                next_usn,
                ..
            } = marker
            {
                println!("  journal size : {:.1} MiB", mib(*maximum_size));
                println!("  marker usn   : {next_usn}");
            }
        }
        Some(sagasu_core::ScanMarker::Mtime { .. }) => {
            println!("delta marker   : mtime (wall clock; never expires)");
        }
        None => println!("delta marker   : (none — searches cannot merge changes)"),
    }

    println!("scan generation: {}", stats.scan_generation);
    println!("live files     : {}", stats.live_count);
    println!("tombstones     : {}", stats.tombstone_count);
    println!("NULL hashes    : {}", stats.null_hash_count);

    // Full-text index state. Showing the generation it was built from makes a
    // stale full-text index visible instead of leaving it to guesswork.
    match stats.fulltext_dir {
        Some(dir) => {
            println!("full-text index: {dir}");
            println!("  documents    : {}", stats.fulltext_docs.unwrap_or(0));
            let ft_gen = stats.fulltext_scan_generation.unwrap_or(0);
            let behind = stats.scan_generation - ft_gen;
            if behind > 0 {
                println!(
                    "  built at gen : {ft_gen} ({behind} scan(s) behind — re-run `sagasu fulltext`)"
                );
            } else {
                println!("  built at gen : {ft_gen} (current)");
            }
        }
        None => println!("full-text index: (not built)"),
    }

    // Tag layer state. Same reasoning as the full-text index above: a tag layer
    // built two crawls ago answers as if the files added since do not exist,
    // and nothing else in the output would reveal that.
    match stats.tag_scan_generation {
        Some(tag_gen) => {
            println!("tags           : {} rows", stats.tag_rows);
            println!("  tagged files : {}", stats.tag_files.unwrap_or(0));
            println!("  distinct     : {}", stats.distinct_tags);
            let behind = stats.scan_generation - tag_gen;
            if behind > 0 {
                println!(
                    "  built at gen : {tag_gen} ({behind} scan(s) behind — re-run `sagasu tag`)"
                );
            } else {
                println!("  built at gen : {tag_gen} (current)");
            }
            match stats.tag_rules.as_deref() {
                Some(rules) if !rules.is_empty() => println!("  rules        : {rules}"),
                _ => println!("  rules        : (none)"),
            }
        }
        None => println!("tags           : (not built)"),
    }

    Ok(())
}
