//! Shared corpus walk and body extraction.
//!
//! Every engine in this prototype is fed by *this* function and nothing else,
//! so "same corpus, same file set, same extracted body" is true by
//! construction rather than by convention.

use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use ignore::{WalkBuilder, WalkState};

/// Extensions treated as text. Same list as `proto-fulltext`.
pub const TEXT_EXTS: &[&str] = &[
    "txt", "md", "rst", "adoc", "csv", "tsv", "log", "ini", "cfg", "conf", "json", "yaml", "yml",
    "toml", "xml", "html", "css", "rs", "py", "js", "ts", "go", "java", "c", "h", "cpp", "hpp",
    "cs", "rb", "sh", "ps1", "bat", "sql", "tex",
];

/// Default body-extraction size cap, matching `sagasu_core::fulltext::DEFAULT_MAX_SIZE`.
pub const DEFAULT_MAX_SIZE: u64 = 2 * 1024 * 1024;

pub struct Doc {
    pub path: String,
    pub mtime_ns: i64,
    pub body: String,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct WalkStats {
    pub files: u64,
    pub body_bytes: u64,
}

fn is_text_target(path: &Path, size: u64, max_size: u64) -> bool {
    if size > max_size {
        return false;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| TEXT_EXTS.contains(&e.to_lowercase().as_str()))
}

fn mtime_ns_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Walk `root` in parallel and hand every extracted document to `f`.
///
/// `standard_filters(false)`: no gitignore, no hidden-file rules. The generated
/// bench tree has neither, and turning them off keeps the file set a pure
/// function of the tree.
pub fn walk_docs<F>(root: &Path, max_size: u64, mut f: F) -> Result<WalkStats>
where
    F: FnMut(Doc) -> Result<()>,
{
    let root = root.canonicalize()?;
    let (tx, rx) = sync_channel::<Doc>(64);
    let walker = WalkBuilder::new(&root)
        .standard_filters(false)
        .threads(
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
        )
        .build_parallel();

    let mut stats = WalkStats::default();
    let mut first_err: Option<anyhow::Error> = None;

    std::thread::scope(|s| {
        s.spawn(move || {
            walker.run(|| {
                let tx = tx.clone();
                Box::new(move |entry| {
                    let Ok(entry) = entry else {
                        return WalkState::Continue;
                    };
                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        return WalkState::Continue;
                    }
                    let Ok(meta) = entry.metadata() else {
                        return WalkState::Continue;
                    };
                    if !is_text_target(entry.path(), meta.len(), max_size) {
                        return WalkState::Continue;
                    }
                    if let Ok(bytes) = std::fs::read(entry.path()) {
                        let _ = tx.send(Doc {
                            path: entry.path().to_string_lossy().into_owned(),
                            mtime_ns: mtime_ns_of(&meta),
                            body: String::from_utf8_lossy(&bytes).into_owned(),
                        });
                    }
                    WalkState::Continue
                })
            });
        });

        // The receiver is always drained to completion: bailing out early would
        // leave the walker threads blocked on a full channel forever.
        for doc in rx {
            if first_err.is_some() {
                continue;
            }
            stats.files += 1;
            stats.body_bytes += doc.body.len() as u64;
            if let Err(e) = f(doc) {
                first_err = Some(e);
            }
        }
    });

    match first_err {
        Some(e) => Err(e),
        None => Ok(stats),
    }
}

/// Recursive byte size of a directory, plus the subtotal of tantivy's `.store`
/// files (the stored-body copy, which the FTS5 side does not keep).
pub fn dir_size(dir: &Path) -> Result<(u64, u64)> {
    let mut total = 0u64;
    let mut store = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
                if entry.path().extension().and_then(|e| e.to_str()) == Some("store") {
                    store += meta.len();
                }
            }
        }
    }
    Ok((total, store))
}
