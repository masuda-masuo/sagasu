//! proto-ftcompare — tantivy vs SQLite FTS5, head to head (issue #35).
//!
//! A throwaway prototype whose only job is to answer the docs/design.md §11
//! open question with numbers. It is deliberately *not* in `crates/`: nothing
//! here is meant to survive the decision.
//!
//! Three engines (`--engine tantivy | fts5-lindera | fts5-trigram`) share one
//! corpus walk, one body-extraction rule and one analyzer, so the only
//! difference between two runs is the engine itself. See `engines.rs` for the
//! fairness conditions and the one asymmetry that could not be removed
//! (stored bodies).
//!
//! Every subcommand prints `key: value` lines so a run log can be pasted into
//! a report without post-processing.

mod corpus;
mod engines;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use corpus::{walk_docs, Doc, DEFAULT_MAX_SIZE};
use engines::{Engine, Reader, Writer};

#[derive(Parser)]
#[command(about = "tantivy vs SQLite FTS5 comparison harness (issue #35)")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build an index from scratch and report build time and on-disk size.
    Index {
        root: PathBuf,
        #[arg(long)]
        engine: Engine,
        #[arg(long, default_value = "ft-index")]
        index_dir: PathBuf,
        #[arg(long, default_value_t = DEFAULT_MAX_SIZE)]
        max_size: u64,
    },
    /// Query an existing index; reports open cost and per-query latency.
    Search {
        query: String,
        #[arg(long)]
        engine: Engine,
        #[arg(long, default_value = "ft-index")]
        index_dir: PathBuf,
        /// Query repetitions inside one process (warm, p50/p95).
        #[arg(long, default_value_t = 20)]
        repeat: usize,
        /// Print the first N matching paths.
        #[arg(long, default_value_t = 0)]
        show: usize,
    },
    /// Compare an engine's hits against a literal substring scan of the same
    /// extracted corpus (the ground truth for recall).
    Recall {
        root: PathBuf,
        query: String,
        #[arg(long)]
        engine: Engine,
        #[arg(long, default_value = "ft-index")]
        index_dir: PathBuf,
        #[arg(long, default_value_t = DEFAULT_MAX_SIZE)]
        max_size: u64,
        /// How many missing / spurious paths to print.
        #[arg(long, default_value_t = 3)]
        examples: usize,
    },
    /// Ground truth on its own: literal case-folded substring scan.
    Grep {
        root: PathBuf,
        query: String,
        #[arg(long, default_value_t = DEFAULT_MAX_SIZE)]
        max_size: u64,
    },
    /// Token-stream introspection: how many tokens a file produces and at
    /// which positions a given token lands. Used to pin down the tantivy
    /// phrase-position anomaly reported in the issue #35 write-up.
    Tokens {
        file: PathBuf,
        /// Report the positions of this token (already-lowercased form).
        #[arg(long)]
        needle: Option<String>,
    },
    /// Replace a single document in an existing index (delta-update cost).
    Update {
        #[arg(long)]
        engine: Engine,
        #[arg(long, default_value = "ft-index")]
        index_dir: PathBuf,
        /// File to re-index; must already be in the index.
        #[arg(long)]
        path: PathBuf,
        #[arg(long, default_value_t = 10)]
        repeat: usize,
    },
}

fn main() -> Result<()> {
    match Args::parse().cmd {
        Cmd::Index {
            root,
            engine,
            index_dir,
            max_size,
        } => cmd_index(&root, engine, &index_dir, max_size),
        Cmd::Search {
            query,
            engine,
            index_dir,
            repeat,
            show,
        } => cmd_search(&query, engine, &index_dir, repeat, show),
        Cmd::Recall {
            root,
            query,
            engine,
            index_dir,
            max_size,
            examples,
        } => cmd_recall(&root, &query, engine, &index_dir, max_size, examples),
        Cmd::Grep {
            root,
            query,
            max_size,
        } => cmd_grep(&root, &query, max_size),
        Cmd::Tokens { file, needle } => cmd_tokens(&file, needle.as_deref()),
        Cmd::Update {
            engine,
            index_dir,
            path,
            repeat,
        } => cmd_update(engine, &index_dir, &path, repeat),
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Nearest-rank percentile over a sorted slice.
fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn cmd_index(root: &Path, engine: Engine, index_dir: &Path, max_size: u64) -> Result<()> {
    let mut writer = Writer::create(engine, index_dir)?;
    let t0 = Instant::now();
    let stats = walk_docs(root, max_size, |doc| writer.add(doc))?;
    let walked = t0.elapsed();
    writer.commit()?;
    let elapsed = t0.elapsed();

    let (total, store) = engines::index_size(engine, index_dir)?;
    println!("engine        : {}", engine.as_str());
    println!("root          : {}", root.display());
    println!("indexed files : {}", stats.files);
    println!("body bytes    : {} ({:.1} MiB)", stats.body_bytes, mib(stats.body_bytes));
    println!("build secs    : {:.3}", elapsed.as_secs_f64());
    println!("  add secs    : {:.3}", walked.as_secs_f64());
    println!("  commit secs : {:.3}", (elapsed - walked).as_secs_f64());
    println!("index bytes   : {} ({:.1} MiB)", total, mib(total));
    println!("  stored body : {} ({:.1} MiB)", store, mib(store));
    println!(
        "  index only  : {} ({:.1} MiB)",
        total - store,
        mib(total - store)
    );
    if stats.body_bytes > 0 {
        println!(
            "index/body    : {:.3} (index only: {:.3})",
            total as f64 / stats.body_bytes as f64,
            (total - store) as f64 / stats.body_bytes as f64
        );
    }
    Ok(())
}

fn cmd_search(
    query: &str,
    engine: Engine,
    index_dir: &Path,
    repeat: usize,
    show: usize,
) -> Result<()> {
    let (mut reader, open) = Reader::open(engine, index_dir)?;
    let docs = reader.doc_count()?;

    let mut cold = None;
    let mut warm: Vec<Duration> = Vec::new();
    let mut count = 0;
    for i in 0..repeat.max(1) {
        let hits = reader.search(query)?;
        count = hits.count;
        if i == 0 {
            cold = Some(hits.elapsed);
        } else {
            warm.push(hits.elapsed);
        }
    }
    warm.sort();

    println!("engine        : {}", engine.as_str());
    println!("query         : {query}");
    println!("docs in index : {docs}");
    println!("hits          : {count}");
    println!("open ms       : {:.3}", ms(open));
    println!("query ms cold : {:.3}", ms(cold.unwrap_or_default()));
    if !warm.is_empty() {
        println!("query ms p50  : {:.3}", ms(percentile(&warm, 50.0)));
        println!("query ms p95  : {:.3}", ms(percentile(&warm, 95.0)));
        println!("query ms min  : {:.3}", ms(warm[0]));
        println!("query ms max  : {:.3}", ms(warm[warm.len() - 1]));
    }
    println!("repeat        : {}", repeat.max(1));
    if show > 0 {
        for p in reader.last_paths()?.into_iter().take(show) {
            println!("hit           : {p}");
        }
    }
    Ok(())
}

/// Literal, case-folded substring scan over the same extraction the engines
/// were fed. This is the definition of "should have matched".
fn grep_truth(root: &Path, query: &str, max_size: u64) -> Result<(BTreeSet<String>, u64, Duration)> {
    let needle = query.to_lowercase();
    let mut hits = BTreeSet::new();
    let t0 = Instant::now();
    let stats = walk_docs(root, max_size, |doc: Doc| {
        if doc.body.to_lowercase().contains(&needle) {
            hits.insert(doc.path);
        }
        Ok(())
    })?;
    Ok((hits, stats.files, t0.elapsed()))
}

fn cmd_grep(root: &Path, query: &str, max_size: u64) -> Result<()> {
    let (hits, files, elapsed) = grep_truth(root, query, max_size)?;
    println!("query         : {query}");
    println!("scanned files : {files}");
    println!("truth hits    : {}", hits.len());
    println!("scan secs     : {:.3}", elapsed.as_secs_f64());
    Ok(())
}

fn cmd_recall(
    root: &Path,
    query: &str,
    engine: Engine,
    index_dir: &Path,
    max_size: u64,
    examples: usize,
) -> Result<()> {
    let (mut reader, _) = Reader::open(engine, index_dir)?;
    let hits = reader.search(query)?;
    let found: BTreeSet<String> = reader.last_paths()?.into_iter().collect();
    let (truth, files, _) = grep_truth(root, query, max_size)?;

    let missing: Vec<&String> = truth.difference(&found).collect();
    let spurious: Vec<&String> = found.difference(&truth).collect();
    let hit = truth.len() - missing.len();

    println!("engine        : {}", engine.as_str());
    println!("query         : {query}");
    println!("scanned files : {files}");
    println!("truth hits    : {}", truth.len());
    println!("engine hits   : {}", hits.count);
    println!("true positives: {hit}");
    println!("missing       : {}", missing.len());
    println!("spurious      : {}", spurious.len());
    if !truth.is_empty() {
        println!(
            "recall        : {:.4}",
            hit as f64 / truth.len() as f64
        );
    }
    if !found.is_empty() {
        println!(
            "precision     : {:.4}",
            hit as f64 / found.len() as f64
        );
    }
    for p in missing.iter().take(examples) {
        println!("missing path  : {p}");
    }
    for p in spurious.iter().take(examples) {
        println!("spurious path : {p}");
    }
    Ok(())
}

fn cmd_tokens(file: &Path, needle: Option<&str>) -> Result<()> {
    use tantivy::tokenizer::TokenStream;

    let bytes = std::fs::read(file)?;
    let body = String::from_utf8_lossy(&bytes).into_owned();
    let mut analyzer = engines::ja_analyzer()?;
    let mut stream = analyzer.token_stream(&body);

    let mut count: u64 = 0;
    let mut max_position: usize = 0;
    let mut non_monotonic: u64 = 0;
    let mut last_position: Option<usize> = None;
    let mut needle_positions: Vec<usize> = Vec::new();
    let mut following: Vec<Vec<String>> = Vec::new();
    while stream.advance() {
        let token = stream.token();
        count += 1;
        max_position = max_position.max(token.position);
        if last_position.is_some_and(|p| token.position <= p) {
            non_monotonic += 1;
        }
        last_position = Some(token.position);
        // Record the two tokens after each of the first few needle hits: a
        // phrase query that fails while the term still matches means
        // adjacency, not presence, is what broke.
        for f in following.iter_mut() {
            if f.len() < 2 {
                f.push(format!("{}@{}", token.text, token.position));
            }
        }
        if needle.is_some_and(|n| token.text == n) {
            needle_positions.push(token.position);
            if following.len() < 5 {
                following.push(Vec::new());
            }
        }
    }

    println!("file          : {}", file.display());
    println!("bytes         : {}", body.len());
    println!("tokens        : {count}");
    println!("max position  : {max_position}");
    println!("non-monotonic : {non_monotonic}");
    if let Some(n) = needle {
        println!("needle        : {n}");
        println!("needle count  : {}", needle_positions.len());
        for (p, f) in needle_positions.iter().zip(following.iter()).take(5) {
            println!("needle pos    : {p} -> {}", f.join(", "));
        }
    }
    Ok(())
}

fn cmd_update(engine: Engine, index_dir: &Path, path: &Path, repeat: usize) -> Result<()> {
    let path = path
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    let meta = std::fs::metadata(&path)?;
    let bytes = std::fs::read(&path)?;
    let body = String::from_utf8_lossy(&bytes).into_owned();
    let body_len = body.len();

    let mut times: Vec<Duration> = Vec::new();
    for _ in 0..repeat.max(1) {
        let doc = Doc {
            path: path.to_string_lossy().into_owned(),
            mtime_ns: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0),
            body: body.clone(),
        };
        times.push(engines::update_one(engine, index_dir, doc)?);
    }
    times.sort();

    println!("engine        : {}", engine.as_str());
    println!("path          : {}", path.display());
    println!("body bytes    : {body_len}");
    println!("repeat        : {}", repeat.max(1));
    println!("update ms p50 : {:.3}", ms(percentile(&times, 50.0)));
    println!("update ms p95 : {:.3}", ms(percentile(&times, 95.0)));
    println!("update ms min : {:.3}", ms(times[0]));
    println!("update ms max : {:.3}", ms(times[times.len() - 1]));
    Ok(())
}
