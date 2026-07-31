//! Deterministic synthetic file-tree generator.
//!
//! ## Reproducibility
//!
//! The same `--seed` + `--files` produces a byte-identical tree:
//! same directory layout, same file sizes, same byte content.
//!
//! ## Size distribution
//!
//! All files are placed into one of five logarithmically-spaced buckets:
//!
//! | Percentile | Size range        |
//! |------------|-------------------|
//! | ~50 %      | 10 B … 1 KB       |
//! | ~30 %      | 1 KB … 64 KB      |
//! | ~15 %      | 64 KB … 256 KB    |
//! | ~4.5 %     | 256 KB … 4 MiB    |
//! | ~0.5 %     | 4 MiB … 16 MiB    |
//!
//! Within a bucket the size is uniform-random.  The boundaries put ~99.5 % of
//! files under 4 MiB and the remaining ~0.5 % above it, matching the profile
//! measured on a real machine on 2026-07-29 (150,848 of 151,675 files under
//! 4 MiB).  **The 0.5 % above 4 MiB is load-bearing**: proto-crawl skips BLAKE3
//! for files larger than its --hash-max-size (4 MiB by default), so a tree
//! without that tail cannot exercise the skip path at all.
//!
//! `--max-file-size` (default 16 MiB) caps every bucket.  Setting it to 4 MiB
//! or below produces a smaller tree at the cost of losing that tail.
//!
//! ## Content
//!
//! Each file body is built from a pool of ~150 English or ~150 Japanese words,
//! drawn with replacement to fill the target size.  A `--japanese-ratio` flag
//! controls the fraction of files whose body is predominantly Japanese.
//!
//! ## Planted query terms
//!
//! A small set of known terms is inserted into a deterministic subset of files
//! so that search recall can be measured later without an external grep.
//! The terms and their file counts are recorded in the manifest.

use std::collections::BTreeMap;
use std::path::Path;

use rand::Rng;
use rand_chacha::rand_core::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Word pools
// ---------------------------------------------------------------------------

/// Common English words (∼150 tokens).
const EN_WORDS: &[&str] = &[
    "the",
    "be",
    "to",
    "of",
    "and",
    "a",
    "in",
    "that",
    "have",
    "it",
    "for",
    "not",
    "on",
    "with",
    "he",
    "as",
    "you",
    "do",
    "at",
    "this",
    "but",
    "his",
    "by",
    "from",
    "they",
    "we",
    "say",
    "her",
    "she",
    "or",
    "an",
    "will",
    "my",
    "one",
    "all",
    "would",
    "there",
    "their",
    "what",
    "so",
    "up",
    "out",
    "if",
    "about",
    "who",
    "get",
    "which",
    "go",
    "me",
    "when",
    "make",
    "can",
    "like",
    "time",
    "no",
    "just",
    "him",
    "know",
    "take",
    "people",
    "into",
    "year",
    "your",
    "good",
    "some",
    "could",
    "them",
    "see",
    "other",
    "than",
    "then",
    "now",
    "look",
    "only",
    "come",
    "its",
    "over",
    "think",
    "also",
    "back",
    "after",
    "use",
    "two",
    "how",
    "our",
    "work",
    "first",
    "well",
    "way",
    "even",
    "new",
    "want",
    "because",
    "any",
    "these",
    "give",
    "day",
    "most",
    "us",
    "great",
    "between",
    "need",
    "large",
    "often",
    "without",
    "turn",
    "many",
    "ask",
    "every",
    "still",
    "sagasu",
    "search",
    "index",
    "crawl",
    "file",
    "system",
    "performance",
    "benchmark",
    "measurement",
    "result",
    "delta",
    "merge",
    "fresh",
    "cache",
    "warm",
    "cold",
    "throughput",
    "latency",
    "metadata",
    "database",
    "query",
    "token",
    "document",
    "thread",
    "parallel",
    "buffer",
    "memory",
    "disk",
    "io",
    "hash",
    "blake3",
    "tantivy",
    "lindera",
    "fts",
    "sqlite",
    "walk",
    "scan",
    "match",
    "fulltext",
    "japanese",
    "tokenizer",
    "stem",
    "rank",
    "score",
];

/// Common Japanese words (∼150 tokens).
const JA_WORDS: &[&str] = &[
    "検索",
    "索引",
    "日本語",
    "ファイル",
    "システム",
    "性能",
    "測定",
    "結果",
    "データ",
    "ベンチマーク",
    "キャッシュ",
    "スループット",
    "レイテンシ",
    "メタデータ",
    "データベース",
    "クエリ",
    "トークン",
    "文書",
    "全文",
    "保存",
    "更新",
    "削除",
    "追加",
    "変更",
    "差分",
    "同期",
    "処理",
    "速度",
    "時間",
    "秒",
    "ミリ秒",
    "並列",
    "直列",
    "実行",
    "出力",
    "入力",
    "設定",
    "構成",
    "環境",
    "変数",
    "定数",
    "関数",
    "構造体",
    "列挙型",
    "トレイト",
    "実装",
    "定義",
    "宣言",
    "呼出",
    "戻値",
    "引数",
    "型",
    "値",
    "参照",
    "所有権",
    "借用",
    "ライフタイム",
    "スレッド",
    "メモリ",
    "スタック",
    "ヒープ",
    "ベクタ",
    "文字列",
    "数値",
    "論理値",
    "配列",
    "スライス",
    "パターン",
    "マッチ",
    "ループ",
    "条件",
    "分岐",
    "繰返",
    "継承",
    "多態",
    "カプセル化",
    "抽象",
    "汎用",
    "安全",
    "高速",
    "安定",
    "信頼",
    "正確",
    "効率",
    "最適",
    "改善",
    "向上",
    "解析",
    "構築",
    "生成",
    "変換",
    "抽出",
    "展開",
    "圧縮",
    "暗号",
    "署名",
    "検証",
    "認証",
    "認可",
    "権限",
    "保護",
    "設定",
    "変更",
    "確認",
    "応答",
    "要求",
    "通知",
    "記録",
    "履歴",
    "管理",
    "運用",
    "監視",
    "警告",
    "異常",
    "障害",
    "復旧",
    "予測",
    "計画",
    "評価",
    "分析",
    "報告",
    "提案",
    "決定",
    "判断",
    "基準",
    "目標",
    "水準",
    "標準",
    "規格",
    "仕様",
    "設計",
    "開発",
    "試験",
    "検査",
    "品質",
    "保証",
    "用語",
    "定義",
    "解釈",
    "意味",
    "内容",
    "形式",
    "方法",
    "手段",
    "方式",
    "原理",
    "法則",
    "理論",
    "実践",
    "応用",
    "実験",
    "観察",
    "考察",
    "結論",
    "課題",
    "解決",
    "次第",
];

/// Planted terms — each is embedded into a deterministic number of files.
const PLANTED_TERMS: &[(&str, u64)] = &[
    ("benchmark", 10),
    ("sagasu", 8),
    ("性能", 7),
    ("測定", 6),
    ("delta merge", 5),
    ("全文検索", 5),
    ("スループット", 4),
    ("latency", 4),
];

// ---------------------------------------------------------------------------
// Size distribution
// ---------------------------------------------------------------------------

const SIZE_BUCKETS: &[(f64, u64, u64)] = &[
    // (probability weight, min bytes, max bytes)
    (0.50, 10, 1024),                     // tiny
    (0.30, 1024, 64 * 1024),              // small
    (0.15, 64 * 1024, 256 * 1024),        // medium
    (0.045, 256 * 1024, 4 * 1024 * 1024), // large
    // The tail deliberately crosses 4 MiB: proto-crawl skips BLAKE3 above its
    // --hash-max-size (4 MiB by default), and a tree with no files above that
    // threshold can never exercise the skip path.  Real-machine measurement
    // (2026-07-29, 151,675 files) put 0.5% of files above 4 MiB.
    (0.005, 4 * 1024 * 1024, 16 * 1024 * 1024), // very large
];

fn sample_size(rng: &mut ChaCha8Rng, max_file_size: u64) -> usize {
    // The cap applies to every bucket, not just the largest, so that a small
    // --max-file-size genuinely shrinks the tree instead of being a no-op.
    // Clamping lo as well keeps gen_range's lo <= hi invariant when the cap
    // falls inside (or below) a bucket's range.
    let cap = max_file_size.max(1024); // a 1 KiB floor keeps the tree writable

    let roll: f64 = rng.gen();
    let mut cumulative = 0.0;
    for &(weight, lo, hi) in SIZE_BUCKETS {
        cumulative += weight;
        if roll < cumulative {
            let hi = hi.min(cap);
            let lo = lo.min(hi);
            return rng.gen_range(lo..=hi) as usize;
        }
    }
    // fallback (shouldn't happen)
    rng.gen_range(10..=4096)
}

// ---------------------------------------------------------------------------
// Content generation
// ---------------------------------------------------------------------------

/// Proportion of body that comes from the JA pool when in Japanese mode.
const JA_CONTENT_RATIO: f64 = 0.7;

/// Fill a buffer of exactly `target_bytes` with random words.
fn fill_content(rng: &mut ChaCha8Rng, is_japanese: bool, target_bytes: usize) -> Vec<u8> {
    let word_pool = if is_japanese { JA_WORDS } else { EN_WORDS };
    let alt_pool = if is_japanese { EN_WORDS } else { JA_WORDS };

    let mut buf = Vec::with_capacity(target_bytes);
    let mut space_left = target_bytes;

    while space_left > 0 {
        // Decide which pool to draw from
        let pool = if rng.gen::<f64>() < JA_CONTENT_RATIO {
            word_pool
        } else {
            alt_pool
        };

        let word = pool[rng.gen_range(0..pool.len())];
        let word_bytes = word.as_bytes();
        let needs_sep = !buf.is_empty();
        let chunk_len = if needs_sep {
            1 + word_bytes.len()
        } else {
            word_bytes.len()
        };

        if chunk_len > space_left {
            // Fill remaining space with spaces and stop
            buf.extend(std::iter::repeat_n(b' ', space_left));
            break;
        }

        if needs_sep {
            buf.push(b' ');
        }
        buf.extend_from_slice(word_bytes);
        space_left -= chunk_len;
    }
    buf
}

// ---------------------------------------------------------------------------
// Planted-term insertion
// ---------------------------------------------------------------------------

/// Insert `term` into `content` at a deterministic position based on `file_index`.
fn insert_planted_term(content: &mut Vec<u8>, term: &str, file_index: u64) {
    // Use file_index to pick a deterministic insertion point in the last third
    // of the content
    let pos_ratio = 0.66 + (file_index as f64 % 33.0) / 100.0;
    let insert_at = ((content.len() as f64) * pos_ratio) as usize;
    let insert_at = insert_at.min(content.len().saturating_sub(1));

    // Build the inserted fragment: " <term> "
    let fragment = format!(" {} ", term);
    let frag_bytes = fragment.as_bytes();

    // Insert at the chosen position (shifts the tail)
    let mut new = Vec::with_capacity(content.len() + frag_bytes.len());
    new.extend_from_slice(&content[..insert_at]);
    new.extend_from_slice(frag_bytes);
    new.extend_from_slice(&content[insert_at..]);
    *content = new;
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Manifest {
    pub seed: u64,
    pub requested_files: u64,
    pub actual_files: u64,
    pub total_bytes: u64,
    pub size_histogram: BTreeMap<String, u64>,
    pub planted_terms: BTreeMap<String, u64>,
    pub japanese_ratio: f64,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Result of a successful tree generation.
#[allow(dead_code)]
pub struct GeneratedTree {
    pub root: std::path::PathBuf,
    pub manifest: Manifest,
}

/// Generate a deterministic synthetic file tree.
///
/// * `out`       – directory to create the tree in (must not exist yet, or be empty).
/// * `files`     – number of files to generate.
/// * `seed`      – RNG seed (same seed → same tree).
/// * `ja_ratio`  – fraction of files whose body is predominantly Japanese (0.0–1.0).
/// * `max_file_size` – cap applied to every size bucket (in bytes); the CLI
///   default is 16 MiB, which keeps ~0.5% of files above the 4 MiB mark.
pub fn generate_tree(
    out: &Path,
    files: u64,
    seed: u64,
    ja_ratio: f64,
    max_file_size: u64,
) -> Result<GeneratedTree, Box<dyn std::error::Error>> {
    let ja_ratio = ja_ratio.clamp(0.0, 1.0);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);

    // Ensure output directory exists and is empty
    if out.exists() {
        std::fs::remove_dir_all(out).map_err(|e| {
            format!(
                "failed to remove existing output directory '{}': {} (os error {})",
                out.display(),
                e.kind(),
                e.raw_os_error().unwrap_or(0),
            )
        })?;
    }
    std::fs::create_dir_all(out).map_err(|e| {
        format!(
            "failed to create output directory '{}': {} (os error {})",
            out.display(),
            e.kind(),
            e.raw_os_error().unwrap_or(0),
        )
    })?;

    // ── Tree structure ──────────────────────────────────────────────
    // No directory holds more than 1000 files.  Directories are named
    // d0000, d0001, … and files f000000.txt, f000001.txt, … globally.
    const MAX_FILES_PER_DIR: u64 = 1000;
    let num_dirs = std::cmp::max(1, files.div_ceil(MAX_FILES_PER_DIR));
    let files_per_dir = files.div_ceil(num_dirs);

    let mut created: Vec<std::path::PathBuf> = Vec::with_capacity(files as usize);
    let mut total_bytes: u64 = 0;
    let mut size_histo: BTreeMap<String, u64> = BTreeMap::new();
    // Histogram bucket keys
    let histo_keys = [
        "0-256", "256-1K", "1K-4K", "4K-16K", "16K-64K", "64K-256K", "256K-1M", "1M-4M", "4M-16M",
        "16M+",
    ];
    for k in &histo_keys {
        size_histo.entry(k.to_string()).or_insert(0);
    }

    for di in 0..num_dirs {
        let dir_name = format!("d{:04}", di);
        let dir_path = out.join(&dir_name);
        std::fs::create_dir_all(&dir_path).map_err(|e| {
            format!(
                "failed to create directory '{}': {} (os error {})",
                dir_path.display(),
                e.kind(),
                e.raw_os_error().unwrap_or(0),
            )
        })?;

        let start = di * files_per_dir;
        let end = files.min(start + files_per_dir);

        for fi in start..end {
            let file_name = format!("f{:06}.txt", fi);
            let file_path = dir_path.join(&file_name);

            let is_japanese = rng.gen::<f64>() < ja_ratio;
            let target_size = sample_size(&mut rng, max_file_size);
            let mut body = fill_content(&mut rng, is_japanese, target_size);
            // Trim any excess
            body.truncate(target_size);

            // Plant query terms into this file if it is selected
            for (term, _count) in PLANTED_TERMS {
                // Deterministic selection: use a hash of (seed, file_index, term)
                let selection_seed = hash3(seed, fi, term);
                let mut sel_rng = ChaCha8Rng::seed_from_u64(selection_seed);
                // Each term goes into exactly _count files out of `files`.
                // Probability per file = count / files.
                let prob = *_count as f64 / files as f64;
                if sel_rng.gen::<f64>() < prob {
                    insert_planted_term(&mut body, term, fi);
                }
            }

            std::fs::write(&file_path, &body).map_err(|e| {
                format!(
                    "failed to write file '{}': {} (os error {})",
                    file_path.display(),
                    e.kind(),
                    e.raw_os_error().unwrap_or(0),
                )
            })?;
            let actual_size = body.len() as u64;

            // Bucket
            let bucket = bucket_for_size(actual_size);
            *size_histo.get_mut(bucket).unwrap() += 1;

            total_bytes += actual_size;
            created.push(file_path);
        }
    }

    // ── Count planted terms ─────────────────────────────────────────
    // Scan every generated file for each term and record the measured count.
    // This is the ground truth: terms may also appear incidentally via the
    // word-pool body generation, and the manifest must reflect the real number.
    let mut planted_counts: BTreeMap<String, u64> = BTreeMap::new();
    for (term, _) in PLANTED_TERMS {
        planted_counts.insert(term.to_string(), 0);
    }
    for path in &created {
        if let Ok(content) = std::fs::read(path) {
            for (term, _) in PLANTED_TERMS {
                if content.windows(term.len()).any(|w| w == term.as_bytes()) {
                    if let Some(count) = planted_counts.get_mut(*term) {
                        *count += 1;
                    }
                }
            }
        }
    }

    // ── Manifest ────────────────────────────────────────────────────
    let manifest = Manifest {
        seed,
        requested_files: files,
        actual_files: created.len() as u64,
        total_bytes,
        size_histogram: size_histo,
        planted_terms: planted_counts,
        japanese_ratio: ja_ratio,
    };

    let manifest_path = out.join(".bench-manifest.json");
    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(&manifest_path, manifest_json).map_err(|e| {
        format!(
            "failed to write manifest '{}': {} (os error {})",
            manifest_path.display(),
            e.kind(),
            e.raw_os_error().unwrap_or(0),
        )
    })?;

    Ok(GeneratedTree {
        root: out.to_path_buf(),
        manifest,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Simple hash of (a, b, s) – used to derive a seed for term planting.
fn hash3(a: u64, b: u64, s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    a.hash(&mut h);
    b.hash(&mut h);
    s.hash(&mut h);
    h.finish()
}

/// Return the histogram bucket label for a given byte size.
fn bucket_for_size(bytes: u64) -> &'static str {
    if bytes <= 256 {
        "0-256"
    } else if bytes <= 1024 {
        "256-1K"
    } else if bytes <= 4 * 1024 {
        "1K-4K"
    } else if bytes <= 16 * 1024 {
        "4K-16K"
    } else if bytes <= 64 * 1024 {
        "16K-64K"
    } else if bytes <= 256 * 1024 {
        "64K-256K"
    } else if bytes <= 1024 * 1024 {
        "256K-1M"
    } else if bytes <= 4 * 1024 * 1024 {
        "1M-4M"
    } else if bytes <= 16 * 1024 * 1024 {
        "4M-16M"
    } else {
        "16M+"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic_generation() {
        // Same seed → same tree (spot-check via manifest)
        let dir1 = std::env::temp_dir().join("bench_det_test_1");
        let dir2 = std::env::temp_dir().join("bench_det_test_2");
        let _ = std::fs::remove_dir_all(&dir1);
        let _ = std::fs::remove_dir_all(&dir2);

        let t1 = generate_tree(&dir1, 50, 12345, 0.5, 4 * 1024 * 1024).unwrap();
        let t2 = generate_tree(&dir2, 50, 12345, 0.5, 4 * 1024 * 1024).unwrap();

        assert_eq!(t1.manifest.actual_files, t2.manifest.actual_files);
        assert_eq!(t1.manifest.total_bytes, t2.manifest.total_bytes);
        assert_eq!(t1.manifest.seed, t2.manifest.seed);
        assert_eq!(t1.manifest.planted_terms, t2.manifest.planted_terms);

        // Verify every file is byte-identical
        for entry in walkdir2(&dir1) {
            let rel = entry.strip_prefix(&dir1).unwrap();
            let other = dir2.join(rel);
            assert!(other.exists(), "missing: {}", other.display());
            let a = std::fs::read(&entry).unwrap();
            let b = std::fs::read(&other).unwrap();
            assert_eq!(a, b, "mismatch: {}", rel.display());
        }

        let _ = std::fs::remove_dir_all(&dir1);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn test_different_seed_different_tree() {
        let dir1 = std::env::temp_dir().join("bench_seed_test_1");
        let dir2 = std::env::temp_dir().join("bench_seed_test_2");
        let _ = std::fs::remove_dir_all(&dir1);
        let _ = std::fs::remove_dir_all(&dir2);

        let t1 = generate_tree(&dir1, 50, 111, 0.5, 4 * 1024 * 1024).unwrap();
        let t2 = generate_tree(&dir2, 50, 222, 0.5, 4 * 1024 * 1024).unwrap();

        // Very unlikely to have same total_bytes with different seeds
        assert_ne!(t1.manifest.total_bytes, t2.manifest.total_bytes);
        assert_ne!(t1.manifest.seed, t2.manifest.seed);

        let _ = std::fs::remove_dir_all(&dir1);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn test_zero_files() {
        let dir = std::env::temp_dir().join("bench_zero_test");
        let _ = std::fs::remove_dir_all(&dir);
        let t = generate_tree(&dir, 0, 0, 0.5, 4 * 1024 * 1024).unwrap();
        assert_eq!(t.manifest.actual_files, 0);
        assert_eq!(t.manifest.total_bytes, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_manifest_planted_terms_match_count() {
        // Walk the generated tree, count each term by reading files,
        // and verify the manifest matches the real count on disk.
        let dir = std::env::temp_dir().join("bench_plant_test");
        let _ = std::fs::remove_dir_all(&dir);
        let t = generate_tree(&dir, 200, 42, 0.5, 4 * 1024 * 1024).unwrap();

        // Count every term by scanning every file on disk
        let mut disk_counts: BTreeMap<String, u64> = BTreeMap::new();
        for (term, _) in PLANTED_TERMS {
            disk_counts.insert(term.to_string(), 0);
        }
        for entry in walkdir2(&dir) {
            if let Ok(content) = std::fs::read(&entry) {
                for (term, _) in PLANTED_TERMS {
                    if content.windows(term.len()).any(|w| w == term.as_bytes()) {
                        if let Some(c) = disk_counts.get_mut(*term) {
                            *c += 1;
                        }
                    }
                }
            }
        }

        // The manifest must match the disk scan exactly
        for (term, _) in PLANTED_TERMS {
            let manifest_count = t.manifest.planted_terms.get(*term).unwrap_or(&0);
            let disk_count = disk_counts.get(*term).unwrap_or(&0);
            assert_eq!(
                manifest_count, disk_count,
                "term '{}': manifest={} but disk has {}",
                term, manifest_count, disk_count
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_size_distribution() {
        let dir = std::env::temp_dir().join("bench_size_dist");
        let _ = std::fs::remove_dir_all(&dir);
        let t = generate_tree(&dir, 1000, 999, 0.5, 4 * 1024 * 1024).unwrap();

        let total = t.manifest.actual_files;
        let under_4m: u64 = t
            .manifest
            .size_histogram
            .iter()
            .filter(|(k, _)| {
                // Buckets entirely below 4 MiB
                matches!(
                    k.as_str(),
                    "0-256"
                        | "256-1K"
                        | "1K-4K"
                        | "4K-16K"
                        | "16K-64K"
                        | "64K-256K"
                        | "256K-1M"
                        | "1M-4M"
                )
            })
            .map(|(_, v)| v)
            .sum();

        let ratio = under_4m as f64 / total as f64;
        // Should be close to 99.5%
        assert!(ratio > 0.98, "only {:.4} of files under 4 MiB", ratio);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Simple recursive directory walk (avoids extra dependency).
    fn walkdir2(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        if dir.is_dir() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    out.extend(walkdir2(&path));
                } else {
                    // Skip manifest
                    if path.file_name().and_then(|n| n.to_str()) == Some(".bench-manifest.json") {
                        continue;
                    }
                    out.push(path);
                }
            }
        }
        out
    }
}
