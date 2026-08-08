//! Keeping the Viterbi lattice Lindera builds over a body bounded (issue #52).
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
//! `warn!` line, so the tail of such a document vanishes from the index — not
//! only its phrases, but its individual words. It was found as a 2.25% phrase
//! shortfall (issue #52) and turned out to be silent data loss.
//!
//! ## The mitigation
//!
//! Two independent layers, because they fail differently:
//!
//! 1. [`bound_lattice_runs`] guarantees, before the text ever reaches Lindera,
//!    that no delimiter-free run is longer than [`MAX_LATTICE_RUN_BYTES`]. A
//!    lattice that small cannot saturate, and a token can never be longer than
//!    the sentence it came from — so `MAX_TOKEN_LEN` also becomes structurally
//!    unreachable rather than merely unlikely.
//! 2. [`LongTokenGuard`] sits at the end of the analyzer chain and drops
//!    anything that is over the limit anyway, **counting it** so it shows up in
//!    the build summary instead of in a log nobody reads. With layer 1 in place
//!    this counter should stay at zero; if it ever moves, the assumption above
//!    has broken and the number says so.
//!
//! Layer 2 is not redundant. Lindera is not the only way to get an enormous
//! token: an unbroken run of hiragana is emitted as a single unknown-word token
//! of the full run length, so a 120 KB `のののの…` file produces a 114 KB token
//! with no saturation involved at all. Layer 1 bounds that case too, which is
//! the reason its limit is set well below `MAX_TOKEN_LEN` rather than just
//! below the saturation point.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use tantivy::tokenizer::{Token, TokenFilter, TokenStream, Tokenizer};

/// Longest delimiter-free run of text handed to Lindera, in bytes.
///
/// Two constraints, and the smaller one wins:
///
/// * **Under `MAX_TOKEN_LEN` (65,530).** A token is a substring of one
///   sentence, so a run this size cannot produce a token tantivy would drop.
/// * **Far under the saturation scale.** The measured break is around 135,000
///   lattice nodes (~950 KB of mixed Japanese/English), so 32 KiB leaves a
///   factor of ~30 of headroom for content whose per-node costs run higher.
///
/// Raising this trades phrase fidelity (a phrase spanning an inserted break is
/// not matched) against nothing — there is no benefit to a longer run. Lowering
/// it costs phrase fidelity for no gain either. 32 KiB is the point where both
/// constraints are met with room to spare.
pub const MAX_LATTICE_RUN_BYTES: usize = 32 * 1024;

/// True for the characters Lindera 5 treats as sentence delimiters.
///
/// Kept as a function rather than inlined so the one place that has to track
/// Lindera's behaviour is visible and testable. If Lindera ever splits on
/// something else, this is the line to change.
pub fn is_lattice_delimiter(c: char) -> bool {
    matches!(c, '\n' | '\t' | '。' | '、')
}

/// Ensure no run of `body` between Lindera sentence delimiters is longer than
/// `max_run` bytes, by introducing `\n` breaks.
///
/// Returns `None` when the body already satisfies the bound — the overwhelmingly
/// common case, and the reason this takes `&str` and hands back an owned
/// `String` only when it actually had to change something. Otherwise returns the
/// rewritten body and the number of breaks introduced.
///
/// A break prefers to **replace** an existing space (or `\r`) rather than be
/// inserted, so for ordinary space-separated prose the body's length and its
/// visible content are unchanged: one space becomes one newline. Only text with
/// no whitespace at all in a whole 32 KiB window — a single unbroken run of
/// Japanese — gets a character it did not have.
///
/// # Panics
///
/// Never. The break offsets are always UTF-8 character boundaries of `body`.
pub fn bound_lattice_runs(body: &str, max_run: usize) -> Option<(String, u32)> {
    let max_run = max_run.max(16);
    let bytes = body.as_bytes();

    // (offset, replaces_the_byte_at_offset)
    let mut breaks: Vec<(usize, bool)> = Vec::new();
    let mut run_start = 0usize;
    let mut space: Option<usize> = None;
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];

        // Mirror of Lindera's own delimiter scan: `\n` / `\t`, plus the two
        // three-byte sequences `。` (E3 80 82) and `、` (E3 80 81), recognised
        // from their trailing byte the way Lindera recognises them.
        let is_delim = if b == b'\n' || b == b'\t' {
            true
        } else if (b == 0x81 || b == 0x82) && i >= 2 {
            let lead = &bytes[i - 2..=i];
            lead == "。".as_bytes() || lead == "、".as_bytes()
        } else {
            false
        };

        if is_delim {
            run_start = i + 1;
            space = None;
            i += 1;
            continue;
        }

        if b == b' ' || b == b'\r' {
            space = Some(i);
        }

        if i - run_start >= max_run {
            match space.filter(|s| *s > run_start) {
                // Turn a space into a newline: same length, same words.
                Some(at) => {
                    breaks.push((at, true));
                    run_start = at + 1;
                }
                // No whitespace in the whole window — insert at the character
                // boundary at or before here. `i` is at least `max_run` past
                // `run_start` and a boundary is never more than three bytes
                // back, so this always makes progress.
                None => {
                    let mut at = i;
                    while at > run_start && !body.is_char_boundary(at) {
                        at -= 1;
                    }
                    breaks.push((at, false));
                    run_start = at;
                }
            }
            space = None;
        }

        i += 1;
    }

    if breaks.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(body.len() + breaks.len());
    let mut prev = 0usize;
    for (at, replaces) in &breaks {
        out.push_str(&body[prev..*at]);
        out.push('\n');
        prev = if *replaces { at + 1 } else { *at };
    }
    out.push_str(&body[prev..]);

    Some((out, breaks.len() as u32))
}

// ── Long-token guard ────────────────────────────────────────────────────────

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
    /// Tokens dropped for exceeding the limit. **This should be zero**: the
    /// run bound makes an over-long token unreachable, so a non-zero count is a
    /// report that the bound no longer holds.
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

    /// A body already within the bound is handed back untouched — no allocation
    /// and, more importantly, no change to what gets stored.
    #[test]
    fn short_body_is_left_alone() {
        assert!(bound_lattice_runs("hello world", 32).is_none());
        assert!(bound_lattice_runs("", 32).is_none());
    }

    /// Existing delimiters reset the run, so ordinary prose never triggers a
    /// break however long the document is.
    #[test]
    fn existing_delimiters_reset_the_run() {
        let line = "a".repeat(20);
        let body = std::iter::repeat_n(line.as_str(), 100)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.len() > 2000);
        assert!(bound_lattice_runs(&body, 64).is_none());

        // …and the Japanese ones count too.
        let ja = std::iter::repeat_n("あいうえお", 100)
            .collect::<Vec<_>>()
            .join("。");
        assert!(bound_lattice_runs(&ja, 64).is_none());
    }

    /// The bound actually holds after the rewrite, for every delimiter-free
    /// window, and nothing but whitespace was introduced.
    #[test]
    fn breaks_bound_every_run() {
        let body = std::iter::repeat_n("word", 4000)
            .collect::<Vec<_>>()
            .join(" ");
        let (out, breaks) = bound_lattice_runs(&body, 256).expect("should need breaks");
        assert!(breaks > 0);
        assert!(longest_run(&out) <= 256 + 8, "run {}", longest_run(&out));
        // A space became a newline, so the body did not grow at all.
        assert_eq!(out.len(), body.len());
        assert_eq!(out.replace('\n', " "), body);
    }

    /// Text with no whitespace anywhere still gets bounded; here the newline is
    /// an insertion rather than a replacement, so the body grows by exactly one
    /// byte per break and the original characters all survive in order.
    #[test]
    fn whitespace_free_text_is_still_bounded() {
        let body = "あ".repeat(4000);
        let (out, breaks) = bound_lattice_runs(&body, 256).expect("should need breaks");
        assert!(breaks > 0);
        assert!(longest_run(&out) <= 256 + 8, "run {}", longest_run(&out));
        assert_eq!(out.len(), body.len() + breaks as usize);
        assert_eq!(out.replace('\n', ""), body);
    }

    /// The reason the limit is what it is: after bounding, no token can reach
    /// tantivy's `MAX_TOKEN_LEN`, because no sentence does.
    #[test]
    fn default_bound_is_under_tantivys_token_limit() {
        const { assert!(MAX_LATTICE_RUN_BYTES + 8 < tantivy::tokenizer::MAX_TOKEN_LEN) };
    }

    /// Longest run of bytes between Lindera delimiters.
    fn longest_run(s: &str) -> usize {
        s.split(is_lattice_delimiter)
            .map(|part| part.len())
            .max()
            .unwrap_or(0)
    }

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
}
