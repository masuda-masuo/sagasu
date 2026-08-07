//! proto-fulltext: M1(tantivy + Lindera 全文索引)と
//! M2 フォールバック経路(mtime 差分 + ライブスキャン + マージ)の検証。
//!
//! - `index`        : テキスト系ファイルを走査して索引を作る。時点マーカーを記録
//! - `search`       : 索引のみを検索(古い結果が返り得る)
//! - `fresh-search` : 索引検索 + mtime 差分のライブスキャンをマージ。
//!   索引を作り直さずに追加・変更・削除が正しく反映されることを実証する

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Instant, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use ignore::{WalkBuilder, WalkState};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED, STRING};
use tantivy::{doc, Index, TantivyDocument};

const TEXT_EXTS: &[&str] = &[
    "txt", "md", "rst", "adoc", "csv", "tsv", "log", "ini", "cfg", "conf", "json", "yaml", "yml",
    "toml", "xml", "html", "css", "rs", "py", "js", "ts", "go", "java", "c", "h", "cpp", "hpp",
    "cs", "rb", "sh", "ps1", "bat", "sql", "tex",
];

#[derive(Parser)]
#[command(about = "tantivy + Lindera 全文索引と鮮度マージのプロトタイプ")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// テキスト系ファイルを索引する(index_dir は毎回作り直す)
    Index {
        root: PathBuf,
        #[arg(long, default_value = "ft-index")]
        index_dir: PathBuf,
        /// 本文抽出のサイズ上限(bytes)
        #[arg(long, default_value_t = 2 * 1024 * 1024)]
        max_size: u64,
    },
    /// 索引のみを検索する
    Search {
        query: String,
        #[arg(long, default_value = "ft-index")]
        index_dir: PathBuf,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// 索引 + mtime 差分ライブスキャンのマージ検索(再索引なしで最新状態を返す)
    FreshSearch {
        query: String,
        #[arg(long, default_value = "ft-index")]
        index_dir: PathBuf,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long, default_value_t = 2 * 1024 * 1024)]
        max_size: u64,
    },
}

fn main() -> Result<()> {
    match Args::parse().cmd {
        Cmd::Index { root, index_dir, max_size } => cmd_index(&root, &index_dir, max_size),
        Cmd::Search { query, index_dir, limit } => cmd_search(&query, &index_dir, limit),
        Cmd::FreshSearch { query, index_dir, limit, max_size } => {
            cmd_fresh_search(&query, &index_dir, limit, max_size)
        }
    }
}

fn build_schema() -> Schema {
    let mut b = Schema::builder();
    b.add_text_field("path", STRING | STORED);
    b.add_i64_field("mtime_ns", STORED);
    b.add_text_field(
        "body",
        TextOptions::default().set_stored().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("lang_ja")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        ),
    );
    b.build()
}

fn register_ja_tokenizer(index: &Index) -> Result<()> {
    use lindera::dictionary::{load_embedded_dictionary, DictionaryKind};
    use lindera::mode::Mode;
    use lindera::segmenter::Segmenter;
    use lindera_tantivy::tokenizer::LinderaTokenizer;

    let dictionary = load_embedded_dictionary(DictionaryKind::IPADIC)
        .map_err(|e| anyhow::anyhow!("IPADIC 読み込み失敗: {e}"))?;
    let segmenter = Segmenter::new(Mode::Normal, dictionary, None);
    index.tokenizers().register("lang_ja", LinderaTokenizer::from_segmenter(segmenter));
    Ok(())
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

/// 索引時点マーカー(unix ns)と root を index_dir の隣に記録する
fn meta_path(index_dir: &Path) -> PathBuf {
    index_dir.join("sagasu-meta.txt")
}

fn write_meta(index_dir: &Path, marker_ns: i64, root: &Path) -> Result<()> {
    std::fs::write(meta_path(index_dir), format!("marker_ns={marker_ns}\nroot={}\n", root.display()))?;
    Ok(())
}

fn read_meta(index_dir: &Path) -> Result<(i64, PathBuf)> {
    let s = std::fs::read_to_string(meta_path(index_dir)).context("sagasu-meta.txt がない(index を先に実行)")?;
    let mut marker = None;
    let mut root = None;
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("marker_ns=") {
            marker = v.parse::<i64>().ok();
        } else if let Some(v) = line.strip_prefix("root=") {
            root = Some(PathBuf::from(v));
        }
    }
    match (marker, root) {
        (Some(m), Some(r)) => Ok((m, r)),
        _ => bail!("sagasu-meta.txt の形式が不正"),
    }
}

fn cmd_index(root: &Path, index_dir: &Path, max_size: u64) -> Result<()> {
    let root = root.canonicalize()?;
    let _ = std::fs::remove_dir_all(index_dir);
    std::fs::create_dir_all(index_dir)?;

    let schema = build_schema();
    let index = Index::create_in_dir(index_dir, schema.clone())?;
    register_ja_tokenizer(&index)?;
    let path_f = schema.get_field("path")?;
    let mtime_f = schema.get_field("mtime_ns")?;
    let body_f = schema.get_field("body")?;

    // マーカーは走査「開始」時点。走査中の変更は次回 fresh-search の差分側に倒れる(安全側)
    let marker_ns = std::time::SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as i64;

    let t0 = Instant::now();
    let (tx, rx) = mpsc::channel::<(String, i64, String)>();
    let walker = WalkBuilder::new(&root)
        .threads(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
        .build_parallel();

    let mut n: u64 = 0;
    let mut total_bytes: u64 = 0;
    std::thread::scope(|s| -> Result<()> {
        s.spawn(move || {
            walker.run(|| {
                let tx = tx.clone();
                Box::new(move |entry| {
                    let Ok(entry) = entry else { return WalkState::Continue };
                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        return WalkState::Continue;
                    }
                    let Ok(meta) = entry.metadata() else { return WalkState::Continue };
                    if !is_text_target(entry.path(), meta.len(), max_size) {
                        return WalkState::Continue;
                    }
                    if let Ok(bytes) = std::fs::read(entry.path()) {
                        let body = String::from_utf8_lossy(&bytes).into_owned();
                        let _ = tx.send((
                            entry.path().to_string_lossy().into_owned(),
                            mtime_ns_of(&meta),
                            body,
                        ));
                    }
                    WalkState::Continue
                })
            });
        });

        let mut writer = index.writer(128 * 1024 * 1024)?;
        for (path, mtime_ns, body) in rx {
            total_bytes += body.len() as u64;
            writer.add_document(doc!(path_f => path, mtime_f => mtime_ns, body_f => body))?;
            n += 1;
        }
        writer.commit()?;
        Ok(())
    })?;

    write_meta(index_dir, marker_ns, &root)?;
    let dir_size: u64 = std::fs::read_dir(index_dir)?
        .filter_map(|e| e.ok().and_then(|e| e.metadata().ok()).map(|m| m.len()))
        .sum();
    println!("indexed files : {n}");
    println!("text bytes    : {:.1} MiB", total_bytes as f64 / 1048576.0);
    println!("elapsed       : {:.3}s", t0.elapsed().as_secs_f64());
    println!("index size    : {:.1} MiB ({})", dir_size as f64 / 1048576.0, index_dir.display());
    println!("marker_ns     : {marker_ns}");
    Ok(())
}

fn open_index(index_dir: &Path) -> Result<(Index, Schema)> {
    let index = Index::open_in_dir(index_dir).context("索引が開けない(index を先に実行)")?;
    register_ja_tokenizer(&index)?;
    let schema = index.schema();
    Ok((index, schema))
}

fn cmd_search(query_str: &str, index_dir: &Path, limit: usize) -> Result<()> {
    let (index, schema) = open_index(index_dir)?;
    let path_f = schema.get_field("path")?;
    let body_f = schema.get_field("body")?;

    let t0 = Instant::now();
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let query = QueryParser::for_index(&index, vec![body_f]).parse_query(query_str)?;
    let top = searcher.search(&query, &TopDocs::with_limit(limit).order_by_score())?;
    let elapsed = t0.elapsed();

    println!("query   : {query_str}");
    println!("hits    : {} (top {limit}, {:.1}ms)", top.len(), elapsed.as_secs_f64() * 1000.0);
    for (score, addr) in top {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let path = doc.get_first(path_f).and_then(|v| v.as_str()).unwrap_or("?");
        println!("  {score:>7.3}  {path}");
    }
    Ok(())
}

/// 差分ライブスキャン: marker 以降に変更されたテキストファイルを列挙し、素朴な部分一致 grep をかける
fn live_scan(
    root: &Path,
    marker_ns: i64,
    query: &str,
    max_size: u64,
) -> (Vec<(String, usize)>, HashSet<String>, f64, f64) {
    let query_lower = query.to_lowercase();

    // 1) 差分検出(mtime フォールバック: 全走査だが stat のみなので速い)
    let t0 = Instant::now();
    let (tx, rx) = mpsc::channel::<String>();
    let walker = WalkBuilder::new(root)
        .threads(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
        .build_parallel();
    std::thread::scope(|s| {
        s.spawn(move || {
            walker.run(|| {
                let tx = tx.clone();
                Box::new(move |entry| {
                    let Ok(entry) = entry else { return WalkState::Continue };
                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        return WalkState::Continue;
                    }
                    let Ok(meta) = entry.metadata() else { return WalkState::Continue };
                    if !is_text_target(entry.path(), meta.len(), max_size) {
                        return WalkState::Continue;
                    }
                    if mtime_ns_of(&meta) > marker_ns {
                        let _ = tx.send(entry.path().to_string_lossy().into_owned());
                    }
                    WalkState::Continue
                })
            });
        });
    });
    let changed: HashSet<String> = rx.into_iter().collect();
    let delta_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // 2) 差分集合のみライブ grep(プロトタイプでは大文字小文字無視の部分一致)
    let t1 = Instant::now();
    let mut live_hits = Vec::new();
    for path in &changed {
        if let Ok(bytes) = std::fs::read(path) {
            let text = String::from_utf8_lossy(&bytes);
            let count = text.to_lowercase().matches(&query_lower).count();
            if count > 0 {
                live_hits.push((path.clone(), count));
            }
        }
    }
    let grep_ms = t1.elapsed().as_secs_f64() * 1000.0;

    (live_hits, changed, delta_ms, grep_ms)
}

fn cmd_fresh_search(query_str: &str, index_dir: &Path, limit: usize, max_size: u64) -> Result<()> {
    let (index, schema) = open_index(index_dir)?;
    let path_f = schema.get_field("path")?;
    let body_f = schema.get_field("body")?;
    let (marker_ns, root) = read_meta(index_dir)?;

    // 1) 索引検索
    let t0 = Instant::now();
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let query = QueryParser::for_index(&index, vec![body_f]).parse_query(query_str)?;
    let top = searcher.search(&query, &TopDocs::with_limit(limit).order_by_score())?;
    let index_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // 2) 差分ライブスキャン
    let (live_hits, changed, delta_ms, grep_ms) = live_scan(&root, marker_ns, query_str, max_size);

    // 3) マージ: 変更済み(=ライブ側が正)と削除済みを索引結果から落とす
    let t2 = Instant::now();
    let mut merged: Vec<(String, String)> = Vec::new(); // (tag, line)
    let mut dropped_changed = 0u32;
    let mut dropped_deleted = 0u32;
    for (score, addr) in top {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let Some(path) = doc.get_first(path_f).and_then(|v| v.as_str()) else { continue };
        if changed.contains(path) {
            dropped_changed += 1; // ライブ結果で置き換わる
            continue;
        }
        if !Path::new(path).exists() {
            dropped_deleted += 1; // 索引後に削除された
            continue;
        }
        merged.push(("index".into(), format!("{score:>7.3}  {path}")));
    }
    for (path, count) in &live_hits {
        merged.push(("live ".into(), format!("{count:>4}hit  {path}")));
    }
    let merge_ms = t2.elapsed().as_secs_f64() * 1000.0;

    println!("query   : {query_str}");
    println!("timing  : index {index_ms:.1}ms | delta-walk {delta_ms:.1}ms | live-grep {grep_ms:.1}ms | merge {merge_ms:.1}ms");
    println!(
        "delta   : changed={} / live-hit={} / dropped(changed)={} / dropped(deleted)={}",
        changed.len(),
        live_hits.len(),
        dropped_changed,
        dropped_deleted
    );
    for (tag, line) in &merged {
        println!("  [{tag}] {line}");
    }
    Ok(())
}
