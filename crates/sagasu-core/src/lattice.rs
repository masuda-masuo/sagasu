//! Detecting a lindera lattice regression instead of shipping it silently
//! (issue #52).
//!
//! ## The defect this module exists for
//!
//! Lindera 5 splits a text into *sentences* before it runs Viterbi, and the
//! only characters it splits on are `\n`, `\t`, `。` and `、` — a space is
//! **not** a delimiter (`lindera::segmenter::Segmenter::segment_with_lattice`).
//! A document with none of those four characters therefore becomes a single
//! lattice over the whole body. Past roughly 135,000 nodes the accumulated
//! `path_cost` saturates at `i32::MAX`, `total_cost < best_cost` stops being
//! satisfiable, and every remaining edge is discarded: the segmenter returns
//! **the entire rest of the document as one token** (lindera/lindera#871).
//!
//! Downstream, tantivy drops any token longer than
//! [`tantivy::tokenizer::MAX_TOKEN_LEN`] (65,530 bytes) with nothing but a
//! `warn!` line, so the tail of such a document vanishes from the index —
//! not only its phrases, but its individual words. It was found as a 2.25%
//! phrase shortfall (issue #52) and turned out to be silent data loss.
//!
//! ## The standing instrument
//!
//! [`LongTokenGuard`] sits at the end of the analyzer chain and drops anything
//! that is over the limit anyway, **counting it** so it shows up in the build
//! summary instead of in a log nobody reads. The summary fields
//! `dropped_long_tokens` and `longest_token_bytes` are the regression
//! instrument: `dropped_long_tokens` must always be zero, and
//! `longest_token_bytes` says how close a corpus runs to the limit.
//!
//! Lindera 5.3.0 bounds the sentence length internally (lindera PR #872), so
//! the pre-tokenization split sagasu used to apply is no longer needed and has
//! been retired. The guard stays: if a future lindera regression reintroduces
//! the unbounded run, the count moves and the failure is visible rather than
//! silent.
//!
//! The guard is not only a lindera regression detector. An unbroken run of
//! hiragana is emitted as a single unknown-word token of the full run length,
//! so a 120 KB `のののの…` file produces a 114 KB token with no lattice
//! saturation involved at all; the guard catches that case too, which is why
//! its limit is `MAX_TOKEN_LEN` rather than something smaller.
//!
//! [`upstream_fix_bounds_delimiter_free_run_without_pre_split`] is the
//! lock-swap sentinel that proves the bound is in effect upstream; see its doc
//! comment.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use tantivy::tokenizer::{Token, TokenFilter, TokenStream, Tokenizer};

// ── Long-token guard ──────────────────────────────────────────────────────────

/// What the analyzer saw that tantivy would have thrown away.
///
/// Shared with the indexing threads through an `Arc`; every field is read once,
/// after the writer has been committed.
#[derive(Debug, Default)]
pub struct TokenStats {
    dropped: AtomicU64,
    longest: AtomicUsize,
}

impl TokenStats {
    /// Tokens dropped for exceeding the limit. **This should be zero**: lindera
    /// 5.3.0 bounds the sentence length internally, so an over-long token is
    /// unreachable, and a non-zero count is a report that the bound no longer
    /// holds.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Longest token seen, in bytes — including the ones that were dropped.
    /// Useful on its own: it is the number that says how close a corpus runs to
    /// the limit.
    pub fn longest(&self) -> usize {
        self.longest.load(Ordering::Relaxed)
    }

    fn observe(&self, len: usize) {
        self.longest.fetch_max(len, Ordering::Relaxed);
    }

    fn drop_one(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Token filter that drops tokens tantivy would drop anyway — but counts them.
///
/// tantivy's own check lives in `postings_writer.rs` and emits a `warn!`, which
/// in a CLI with no logger configured is indistinguishable from silence. That
/// silence is what made issue #52 take a comparative benchmark against SQLite
/// FTS5 to notice at all. Dropping the token here instead makes the same
/// decision *visible*, and keeps the index clean of a term nobody can query.
#[derive(Clone)]
pub struct LongTokenGuard {
    limit: usize,
    stats: Arc<TokenStats>,
}

impl LongTokenGuard {
    /// Guard rejecting tokens of `limit` bytes or more, reporting into `stats`.
    pub fn new(limit: usize, stats: Arc<TokenStats>) -> Self {
        Self { limit, stats }
    }
}

impl TokenFilter for LongTokenGuard {
    type Tokenizer<T: Tokenizer> = LongTokenGuardWrapper<T>;

    fn transform<T: Tokenizer>(self, tokenizer: T) -> LongTokenGuardWrapper<T> {
        LongTokenGuardWrapper {
            limit: self.limit,
            stats: self.stats,
            inner: tokenizer,
        }
    }
}

/// The [`Tokenizer`] [`LongTokenGuard`] wraps around.
#[derive(Clone)]
pub struct LongTokenGuardWrapper<T: Tokenizer> {
    limit: usize,
    stats: Arc<TokenStats>,
    inner: T,
}

impl<T: Tokenizer> Tokenizer for LongTokenGuardWrapper<T> {
    type TokenStream<'a> = LongTokenGuardStream<T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        LongTokenGuardStream {
            limit: self.limit,
            stats: Arc::clone(&self.stats),
            tail: self.inner.token_stream(text),
        }
    }
}

/// The [`TokenStream`] [`LongTokenGuard`] wraps around.
pub struct LongTokenGuardStream<T> {
    limit: usize,
    stats: Arc<TokenStats>,
    tail: T,
}

impl<T: TokenStream> TokenStream for LongTokenGuardStream<T> {
    fn advance(&mut self) -> bool {
        while self.tail.advance() {
            let len = self.tail.token().text.len();
            self.stats.observe(len);
            if len < self.limit {
                return true;
            }
            self.stats.drop_one();
        }
        false
    }

    fn token(&self) -> &Token {
        self.tail.token()
    }

    fn token_mut(&mut self) -> &mut Token {
        self.tail.token_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_drops_and_counts_over_long_tokens() {
        use tantivy::tokenizer::{TextAnalyzer, WhitespaceTokenizer};

        let stats = Arc::new(TokenStats::default());
        let mut analyzer = TextAnalyzer::builder(WhitespaceTokenizer::default())
            .filter(LongTokenGuard::new(8, Arc::clone(&stats)))
            .build();

        let mut stream = analyzer.token_stream("short waytoolongtoken ok");
        let mut kept = Vec::new();
        while stream.advance() {
            kept.push(stream.token().text.clone());
        }

        assert_eq!(kept, vec!["short".to_string(), "ok".to_string()]);
        assert_eq!(stats.dropped(), 1);
        assert_eq!(stats.longest(), "waytoolongtoken".len());
    }

    /// Evidence test for the upstream lindera fix (lindera PR #872,
    /// "fix(segmenter): bound sentence length for delimiter-free input",
    /// merged 2026-08-08, shipped in lindera >= 5.0.2).
    ///
    /// Before the fix, the segmenter treated an entire delimiter-free stretch
    /// (none of `\n` `\t` `。` `、`) as ONE sentence, so a long katakana run
    /// tokenized as a single unknown-word token of the run's full byte length —
    /// far past `MAX_TOKEN_LEN` (65,530), which tantivy silently drops, so the
    /// document tail vanished from the index. That is the failure mode
    /// [`LongTokenGuard`] exists to make visible, and the regression that the
    /// upstream fix closes.
    ///
    /// This body is 12 units of (30,000 × `ヲ` + `x`) = 1,080,012 bytes,
    /// delimiter-free and mixed-script. Each 90,000-byte katakana run would
    /// have come back as one ≥90,000-byte token on lindera 5.0.1, failing the
    /// assertion below; on >= 5.0.2 the segmenter's internal sentence bound
    /// caps every emitted token (measured 32,769 bytes max on 5.3.0), so the
    /// test passes — the standing proof that an unbounded delimiter-free run
    /// no longer reaches the indexer.
    ///
    /// Discriminating power was verified by lock-swap, not asserted: with
    /// `Cargo.lock` restored to the pre-fix world (`git show HEAD:Cargo.lock`,
    /// all lindera crates at 5.0.1) this test FAILS, and with the bumped lock
    /// it passes (2026-08-22). To re-run that check, swap the lockfile the same
    /// way and use `cargo test --locked`.
    ///
    /// The raw path is exercised here: NO pre-split, just the
    /// segmenter/tokenizer exactly as lindera hands it to us.
    #[test]
    fn upstream_fix_bounds_delimiter_free_run_without_pre_split() {
        use lindera::dictionary::{load_embedded_dictionary, DictionaryKind};
        use lindera::mode::Mode;
        use lindera::segmenter::Segmenter;
        use lindera_tantivy::tokenizer::LinderaTokenizer;
        use tantivy::tokenizer::{MAX_TOKEN_LEN, Tokenizer};

        // 30,000 katakana chars (90,000 bytes) per run — well past MAX_TOKEN_LEN
        // as a single unknown-word token — joined by a single ASCII `x` for the
        // mixed-script shape. No sentence delimiters anywhere.
        let unit = format!("{}x", "ヲ".repeat(30_000));
        let body: String = unit.repeat(12);
        assert!(
            body.len() >= 1_000_000,
            "constructed body unexpectedly small: {} bytes",
            body.len()
        );
        assert!(
            !body.contains(['\n', '\t', '。', '、']),
            "body must stay delimiter-free to reproduce the unbounded-sentence shape"
        );

        let dictionary = load_embedded_dictionary(DictionaryKind::IPADIC)
            .expect("embedded IPADIC dictionary must load");
        let segmenter = Segmenter::new(Mode::Normal, dictionary, None);
        let mut tokenizer = LinderaTokenizer::from_segmenter(segmenter);

        // Raw lindera path: no pre-split.
        let mut stream = tokenizer.token_stream(&body);
        let mut longest = 0usize;
        while stream.advance() {
            let len = stream.token().text.len();
            assert!(
                len < MAX_TOKEN_LEN,
                "lindera emitted a {len}-byte token (>= MAX_TOKEN_LEN {MAX_TOKEN_LEN}); \
                 the upstream sentence bound is not in effect"
            );
            longest = longest.max(len);
        }
        assert!(longest > 0, "expected at least one token from the body");
    }

    /// The production analyzer (LinderaTokenizer + LowerCaser + [`LongTokenGuard`])
    /// run over the *unmodified* body must drop zero tokens even on a
    /// delimiter-free stretch that, before lindera 5.3.0, would have saturated
    /// the lattice. This is the guarantee the retired pre-split used to provide
    /// by rewriting the body; now it is lindera's own sentence bound that keeps
    /// the guard quiet, and the guard still proves it is counting (longest token
    /// seen is positive).
    #[test]
    fn production_path_guard_stays_quiet_without_pre_split() {
        use lindera::dictionary::{load_embedded_dictionary, DictionaryKind};
        use lindera::mode::Mode;
        use lindera::segmenter::Segmenter;
        use lindera_tantivy::tokenizer::LinderaTokenizer;
        use tantivy::tokenizer::{LowerCaser, MAX_TOKEN_LEN, TextAnalyzer};

        let unit = "あいうえおabcde";
        let repeats = 1_048_576 / unit.len();
        let body: String = std::iter::repeat(unit).take(repeats).collect();
        assert!(
            !body.contains(['\n', '\t', '。', '、']),
            "body must stay delimiter-free to reproduce the unbounded-sentence shape"
        );

        let stats = Arc::new(TokenStats::default());
        let dictionary = load_embedded_dictionary(DictionaryKind::IPADIC)
            .expect("embedded IPADIC dictionary must load");
        let segmenter = Segmenter::new(Mode::Normal, dictionary, None);
        let mut analyzer = TextAnalyzer::builder(LinderaTokenizer::from_segmenter(segmenter))
            .filter(LowerCaser)
            .filter(LongTokenGuard::new(MAX_TOKEN_LEN, Arc::clone(&stats)))
            .build();

        let mut stream = analyzer.token_stream(&body);
        while stream.advance() {}

        assert_eq!(
            stats.dropped(),
            0,
            "LongTokenGuard dropped {} tokens on the unmodified body; \
             the production analyzer must keep it at zero",
            stats.dropped()
        );
        assert!(
            stats.longest() > 0,
            "the guard should have counted at least one token"
        );
    }
}
