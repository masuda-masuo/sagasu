//! proto-crawl: M0 の速度リスク検証。
//! `ignore` の並列走査でメタデータ(+任意で BLAKE3)を収集し、SQLite に一括投入する。
//! 計測対象: 走査スループット、ハッシュのコスト、SQLite 投入速度。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Instant, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use ignore::{WalkBuilder, WalkState};
use rusqlite::Connection;

/// 既定でスキップするディレクトリ名(`--full-volume` 指定時は無効)。
/// basename で判定し、大文字小文字は区別しない。ボリューム全体を舐め尽くす挙動
/// (Defender の ML 誤検知の一因、issue #10)を避けるための最小セット:
/// OS 本体・インストール済みアプリ・ごみ箱・システム復元ポイント・ユーザーごとの
/// キャッシュ/一時ファイル。ファイル検索インデックスがそもそも対象にすべきでない領域。
const DEFAULT_SKIP_DIR_NAMES: &[&str] = &[
    "Windows",                    // OS 本体
    "Program Files",              // インストール済みアプリ(64-bit)
    "Program Files (x86)",        // インストール済みアプリ(32-bit)
    "$Recycle.Bin",                // ごみ箱
    "System Volume Information",  // NTFS システム領域(復元ポイント等)
    "AppData",                     // ユーザーごとのキャッシュ/設定/一時ファイル
];

fn is_default_skip_dir(name: &str) -> bool {
    DEFAULT_SKIP_DIR_NAMES.iter().any(|s| s.eq_ignore_ascii_case(name))
}

#[derive(Parser)]
#[command(about = "並列クロール + SQLite メタデータ索引のプロトタイプ")]
struct Args {
    /// 走査対象ルート
    root: PathBuf,
    /// 出力 SQLite ファイル(毎回作り直す)
    #[arg(long, default_value = "crawl.db")]
    db: PathBuf,
    /// BLAKE3 ハッシュも計算する(hash_max_size 以下のファイルのみ)
    #[arg(long)]
    hash: bool,
    /// ハッシュ対象のサイズ上限(bytes)
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    hash_max_size: u64,
    /// 走査スレッド数(0 = CPU数)
    #[arg(long, default_value_t = 0)]
    threads: usize,
    /// .gitignore / hidden を尊重せず全ファイルを走査する
    #[arg(long)]
    no_ignore: bool,
    /// 既定のシステムディレクトリ・スキップ(Windows / Program Files /
    /// Program Files (x86) / $Recycle.Bin / System Volume Information / AppData)を
    /// 無効化し、以前どおりのフルボリューム走査にする(--hash-max-size によるハッシュ
    /// 対象サイズ上限はこのフラグと無関係に有効なまま)
    #[arg(long)]
    full_volume: bool,
}

struct Row {
    path: String,
    size: i64,
    mtime_ns: i64,
    ext: Option<String>,
    hash: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let root = args
        .root
        .canonicalize()
        .with_context(|| format!("root が見つからない: {}", args.root.display()))?;

    let _ = std::fs::remove_file(&args.db);
    let mut conn = Connection::open(&args.db)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(
        "CREATE TABLE files (
            path     TEXT PRIMARY KEY,
            size     INTEGER NOT NULL,
            mtime_ns INTEGER NOT NULL,
            ext      TEXT,
            blake3   TEXT
        );",
    )?;

    let (tx, rx) = mpsc::channel::<Row>();
    let do_hash = args.hash;
    let hash_max = args.hash_max_size;
    let skip_defaults = !args.full_volume;
    let skipped_dirs = Arc::new(AtomicU64::new(0));

    let mut builder = WalkBuilder::new(&root);
    builder.threads(if args.threads == 0 {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
    } else {
        args.threads
    });
    if args.no_ignore {
        builder.hidden(false).ignore(false).git_ignore(false).git_global(false).git_exclude(false);
    }
    let walker = builder.build_parallel();

    let t0 = Instant::now();
    let mut n: u64 = 0;
    let mut total_bytes: i64 = 0;
    let mut hashed: u64 = 0;

    // 走査スレッド群がメタデータ取得とハッシュまで済ませ、
    // main スレッドは並行して SQLite への投入に専念する。
    let skipped_dirs_walk = skipped_dirs.clone();
    std::thread::scope(|s| -> Result<()> {
        s.spawn(move || {
            walker.run(|| {
                let tx = tx.clone();
                let skipped_dirs = skipped_dirs_walk.clone();
                Box::new(move |entry| {
                    let Ok(entry) = entry else { return WalkState::Continue };
                    if skip_defaults
                        && entry.depth() > 0
                        && entry.file_type().is_some_and(|t| t.is_dir())
                        && entry.file_name().to_str().is_some_and(is_default_skip_dir)
                    {
                        skipped_dirs.fetch_add(1, Ordering::Relaxed);
                        return WalkState::Skip;
                    }
                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        return WalkState::Continue;
                    }
                    let Ok(meta) = entry.metadata() else { return WalkState::Continue };
                    let size = meta.len();
                    let mtime_ns = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);
                    let path = entry.path();
                    let hash = if do_hash && size <= hash_max {
                        std::fs::read(path).ok().map(|b| blake3::hash(&b).to_hex().to_string())
                    } else {
                        None
                    };
                    let row = Row {
                        path: path.to_string_lossy().into_owned(),
                        size: size as i64,
                        mtime_ns,
                        ext: path.extension().map(|e| e.to_string_lossy().to_lowercase()),
                        hash,
                    };
                    let _ = tx.send(row);
                    WalkState::Continue
                })
            });
            // tx はこのスレッドに move 済みなので、走査完了とともに閉じる
        });

        let tx_db = conn.transaction()?;
        {
            let mut stmt = tx_db.prepare(
                "INSERT OR REPLACE INTO files (path, size, mtime_ns, ext, blake3) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for row in rx {
                stmt.execute(rusqlite::params![row.path, row.size, row.mtime_ns, row.ext, row.hash])?;
                n += 1;
                total_bytes += row.size;
                if row.hash.is_some() {
                    hashed += 1;
                }
            }
        }
        tx_db.commit()?;
        Ok(())
    })?;
    let elapsed = t0.elapsed();
    // WAL に溜まった分を本体に反映してから実サイズを測る
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

    let db_size = std::fs::metadata(&args.db).map(|m| m.len()).unwrap_or(0);
    println!("root          : {}", root.display());
    println!("files         : {n}");
    println!("total size    : {:.1} MiB", total_bytes as f64 / 1048576.0);
    println!("hashed        : {hashed} (--hash={do_hash}, max {hash_max} bytes)");
    println!(
        "skipped dirs  : {} (default skip {})",
        skipped_dirs.load(Ordering::Relaxed),
        if skip_defaults { "on" } else { "off (--full-volume)" }
    );
    println!("elapsed       : {:.3}s", elapsed.as_secs_f64());
    println!("throughput    : {:.0} files/s", n as f64 / elapsed.as_secs_f64());
    println!("db size       : {:.1} MiB ({})", db_size as f64 / 1048576.0, args.db.display());
    Ok(())
}
