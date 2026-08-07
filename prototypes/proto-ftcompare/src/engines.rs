//! The three engines under comparison.
//!
//! | engine          | index                | tokenisation                                   |
//! |-----------------|----------------------|------------------------------------------------|
//! | `tantivy`       | tantivy 0.25         | Lindera (IPADIC, Normal) + `LowerCaser`         |
//! | `fts5-lindera`  | SQLite FTS5          | the *same* analyzer, joined with spaces, fed to |
//! |                 |                      | FTS5's `unicode61`                              |
//! | `fts5-trigram`  | SQLite FTS5          | `trigram` (substring matching, no segmentation) |
//!
//! `fts5-lindera` deliberately reuses the tantivy `TextAnalyzer` rather than
//! calling Lindera directly: the two engines then see byte-identical token
//! streams, so any difference in results is the engine, not the tokeniser.
//!
//! Storage is *not* symmetric and the report must say so: the tantivy schema
//! stores the body (`STORED`, as the product schema does), while both FTS5
//! tables are `content=''` (contentless — index only, no copy of the text).
//! `dir_size` therefore reports tantivy's `.store` subtotal separately so an
//! index-only comparison is possible.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;
use tantivy::collector::DocSetCollector;
use tantivy::query::QueryParser;
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED, STRING,
};
use tantivy::tokenizer::{LowerCaser, TextAnalyzer, TokenStream};
use tantivy::{doc, DocAddress, Index, TantivyDocument, Term};

use crate::corpus::Doc;

/// Name the Lindera analyzer is registered under, matching the product schema.
pub const JA_TOKENIZER: &str = "lang_ja";

/// Writer heap for tantivy, matching `proto-fulltext`.
const WRITER_HEAP: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Engine {
    Tantivy,
    Fts5Trigram,
    Fts5Lindera,
}

impl Engine {
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Tantivy => "tantivy",
            Engine::Fts5Trigram => "fts5-trigram",
            Engine::Fts5Lindera => "fts5-lindera",
        }
    }

    fn is_fts5(self) -> bool {
        !matches!(self, Engine::Tantivy)
    }
}

impl std::str::FromStr for Engine {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "tantivy" => Ok(Engine::Tantivy),
            "fts5-trigram" => Ok(Engine::Fts5Trigram),
            "fts5-lindera" => Ok(Engine::Fts5Lindera),
            other => Err(format!(
                "unknown engine `{other}` (tantivy | fts5-trigram | fts5-lindera)"
            )),
        }
    }
}

// ── Shared analyzer ────────────────────────────────────────────────────────

/// Lindera (embedded IPADIC, Normal mode) with `LowerCaser` chained behind it.
///
/// Identical to `sagasu_core::fulltext::register_ja_tokenizer`.
pub fn ja_analyzer() -> Result<TextAnalyzer> {
    use lindera::dictionary::{load_embedded_dictionary, DictionaryKind};
    use lindera::mode::Mode;
    use lindera::segmenter::Segmenter;
    use lindera_tantivy::tokenizer::LinderaTokenizer;

    let dictionary = load_embedded_dictionary(DictionaryKind::IPADIC)
        .map_err(|e| anyhow!("failed to load the embedded IPADIC dictionary: {e}"))?;
    let segmenter = Segmenter::new(Mode::Normal, dictionary, None);
    Ok(
        TextAnalyzer::builder(LinderaTokenizer::from_segmenter(segmenter))
            .filter(LowerCaser)
            .build(),
    )
}

/// Run `text` through the analyzer and join the tokens with single spaces.
///
/// This is what turns the Lindera token stream into something FTS5's
/// `unicode61` tokenizer can consume unchanged.
pub fn segment(analyzer: &mut TextAnalyzer, text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut stream = analyzer.token_stream(text);
    while stream.advance() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&stream.token().text);
    }
    out
}

// ── Index layout ───────────────────────────────────────────────────────────

/// Every engine gets its own directory; FTS5 keeps a single file inside it.
pub fn db_path(index_dir: &Path) -> PathBuf {
    index_dir.join("fts5.db")
}

/// Total on-disk bytes of an index, and the part of it that is a stored copy of
/// the text rather than index proper.
pub fn index_size(engine: Engine, index_dir: &Path) -> Result<(u64, u64)> {
    if engine.is_fts5() {
        let mut total = 0;
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let p = PathBuf::from(format!("{}{suffix}", db_path(index_dir).display()));
            if let Ok(meta) = std::fs::metadata(&p) {
                total += meta.len();
            }
        }
        // Contentless FTS5 keeps no copy of the text.
        Ok((total, 0))
    } else {
        crate::corpus::dir_size(index_dir)
    }
}

// ── Writers ────────────────────────────────────────────────────────────────

pub enum Writer {
    Tantivy(TantivyWriter),
    Fts5(Fts5Writer),
}

impl Writer {
    pub fn create(engine: Engine, index_dir: &Path) -> Result<Self> {
        let _ = std::fs::remove_dir_all(index_dir);
        std::fs::create_dir_all(index_dir)?;
        match engine {
            Engine::Tantivy => Ok(Writer::Tantivy(TantivyWriter::create(index_dir)?)),
            e => Ok(Writer::Fts5(Fts5Writer::create(e, index_dir)?)),
        }
    }

    pub fn add(&mut self, doc: Doc) -> Result<()> {
        match self {
            Writer::Tantivy(w) => w.add(doc),
            Writer::Fts5(w) => w.add(doc),
        }
    }

    pub fn commit(&mut self) -> Result<()> {
        match self {
            Writer::Tantivy(w) => w.commit(),
            Writer::Fts5(w) => w.commit(),
        }
    }
}

pub struct TantivyWriter {
    writer: tantivy::IndexWriter,
    path_f: Field,
    mtime_f: Field,
    body_f: Field,
}

/// The product schema minus `file_id` (this prototype has no crawl DB to link
/// against); field options and tokenizer are otherwise identical.
pub fn build_schema() -> Schema {
    let mut b = Schema::builder();
    b.add_text_field("path", STRING | STORED);
    b.add_i64_field("mtime_ns", STORED);
    b.add_text_field(
        "body",
        TextOptions::default().set_stored().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(JA_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        ),
    );
    b.build()
}

fn register(index: &Index) -> Result<()> {
    index.tokenizers().register(JA_TOKENIZER, ja_analyzer()?);
    Ok(())
}

impl TantivyWriter {
    fn create(index_dir: &Path) -> Result<Self> {
        let schema = build_schema();
        let index = Index::create_in_dir(index_dir, schema.clone())
            .with_context(|| format!("failed to create a tantivy index in {}", index_dir.display()))?;
        register(&index)?;
        Ok(Self {
            writer: index.writer(WRITER_HEAP)?,
            path_f: schema.get_field("path")?,
            mtime_f: schema.get_field("mtime_ns")?,
            body_f: schema.get_field("body")?,
        })
    }

    fn add(&mut self, d: Doc) -> Result<()> {
        self.writer.add_document(
            doc!(self.path_f => d.path, self.mtime_f => d.mtime_ns, self.body_f => d.body),
        )?;
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.writer.commit()?;
        Ok(())
    }
}

pub struct Fts5Writer {
    conn: Connection,
    /// `Some` only for `fts5-lindera`; its presence *is* the engine choice on
    /// the write path, since a trigram table takes the raw (lowercased) text.
    analyzer: Option<TextAnalyzer>,
    next_id: i64,
}

fn fts5_tokenizer(engine: Engine) -> &'static str {
    match engine {
        Engine::Fts5Trigram => "trigram",
        // The body arrives pre-segmented, so unicode61 only has to split on the
        // spaces this prototype inserted.
        _ => "unicode61",
    }
}

fn create_fts5_schema(conn: &Connection, engine: Engine) -> Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE paths(id INTEGER PRIMARY KEY, path TEXT NOT NULL, mtime_ns INTEGER NOT NULL);
         CREATE INDEX paths_path ON paths(path);
         CREATE VIRTUAL TABLE docs USING fts5(
             body,
             tokenize='{}',
             content='',
             contentless_delete=1
         );",
        fts5_tokenizer(engine)
    ))?;
    Ok(())
}

impl Fts5Writer {
    fn create(engine: Engine, index_dir: &Path) -> Result<Self> {
        let conn = Connection::open(db_path(index_dir))?;
        create_fts5_schema(&conn, engine)?;
        // One transaction around the whole build: the equivalent of tantivy's
        // single commit, and the only pragma-shaped choice made here.
        conn.execute_batch("BEGIN")?;
        Ok(Self {
            conn,
            analyzer: match engine {
                Engine::Fts5Lindera => Some(ja_analyzer()?),
                _ => None,
            },
            next_id: 1,
        })
    }

    fn body_for(&mut self, body: &str) -> String {
        match self.analyzer.as_mut() {
            Some(a) => segment(a, body),
            // trigram folds case itself, but lowercasing here makes the
            // "English is case-folded" condition identical across engines
            // instead of relying on each engine's default.
            None => body.to_lowercase(),
        }
    }

    fn add(&mut self, d: Doc) -> Result<()> {
        let id = self.next_id;
        self.next_id += 1;
        let indexed = self.body_for(&d.body);
        self.conn.execute(
            "INSERT INTO paths(id, path, mtime_ns) VALUES(?1, ?2, ?3)",
            rusqlite::params![id, d.path, d.mtime_ns],
        )?;
        self.conn.execute(
            "INSERT INTO docs(rowid, body) VALUES(?1, ?2)",
            rusqlite::params![id, indexed],
        )?;
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        // Not timed as part of the build: an optimize pass is a policy choice,
        // and leaving it out keeps the build numbers comparable to tantivy's
        // single commit without a merge.
        Ok(())
    }
}

// ── Searchers ──────────────────────────────────────────────────────────────

pub struct Hits {
    pub count: usize,
    pub elapsed: Duration,
}

pub enum Reader {
    Tantivy(TantivyReader),
    Fts5(Fts5Reader),
}

impl Reader {
    /// Open the index. The returned duration is the fixed cost (index open,
    /// dictionary load) that a fresh process would pay before any query work.
    pub fn open(engine: Engine, index_dir: &Path) -> Result<(Self, Duration)> {
        let t0 = Instant::now();
        let r = match engine {
            Engine::Tantivy => Reader::Tantivy(TantivyReader::open(index_dir)?),
            e => Reader::Fts5(Fts5Reader::open(e, index_dir)?),
        };
        Ok((r, t0.elapsed()))
    }

    /// Run one query, collecting the full matching doc set (no stored-field
    /// access on either side, so the two engines do the same amount of work).
    pub fn search(&mut self, query: &str) -> Result<Hits> {
        match self {
            Reader::Tantivy(r) => r.search(query),
            Reader::Fts5(r) => r.search(query),
        }
    }

    /// Paths of the last `search`. Not timed — this is for the recall check.
    pub fn last_paths(&self) -> Result<Vec<String>> {
        match self {
            Reader::Tantivy(r) => r.last_paths(),
            Reader::Fts5(r) => r.last_paths(),
        }
    }

    pub fn doc_count(&self) -> Result<u64> {
        match self {
            Reader::Tantivy(r) => Ok(r.searcher.num_docs()),
            Reader::Fts5(r) => Ok(r
                .conn
                .query_row("SELECT count(*) FROM paths", [], |row| row.get::<_, i64>(0))?
                as u64),
        }
    }
}

pub struct TantivyReader {
    index: Index,
    searcher: tantivy::Searcher,
    path_f: Field,
    body_f: Field,
    last: Vec<DocAddress>,
}

impl TantivyReader {
    fn open(index_dir: &Path) -> Result<Self> {
        let index = Index::open_in_dir(index_dir)
            .with_context(|| format!("cannot open the tantivy index at {}", index_dir.display()))?;
        register(&index)?;
        let schema = index.schema();
        let reader = index.reader()?;
        let searcher = reader.searcher();
        Ok(Self {
            path_f: schema.get_field("path")?,
            body_f: schema.get_field("body")?,
            index,
            searcher,
            last: Vec::new(),
        })
    }

    fn search(&mut self, query_str: &str) -> Result<Hits> {
        let t0 = Instant::now();
        // Phrase query on both engines: FTS5 has no other way to express
        // "these tokens, adjacent", and an OR-of-terms would not be the same
        // question.
        let parsed = QueryParser::for_index(&self.index, vec![self.body_f])
            .parse_query(&format!("\"{}\"", query_str.replace('"', "\\\"")))?;
        let hits = self.searcher.search(&parsed, &DocSetCollector)?;
        let elapsed = t0.elapsed();
        self.last = hits.into_iter().collect();
        Ok(Hits {
            count: self.last.len(),
            elapsed,
        })
    }

    fn last_paths(&self) -> Result<Vec<String>> {
        let mut out = Vec::with_capacity(self.last.len());
        for addr in &self.last {
            let doc: TantivyDocument = self.searcher.doc(*addr)?;
            out.push(
                doc.get_first(self.path_f)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            );
        }
        Ok(out)
    }
}

pub struct Fts5Reader {
    conn: Connection,
    analyzer: Option<TextAnalyzer>,
    last: Vec<i64>,
}

impl Fts5Reader {
    fn open(engine: Engine, index_dir: &Path) -> Result<Self> {
        let path = db_path(index_dir);
        if !path.exists() {
            return Err(anyhow!("cannot open the FTS5 index at {}", path.display()));
        }
        Ok(Self {
            conn: Connection::open(&path)?,
            analyzer: match engine {
                Engine::Fts5Lindera => Some(ja_analyzer()?),
                _ => None,
            },
            last: Vec::new(),
        })
    }

    fn match_expr(&mut self, query: &str) -> String {
        let prepared = match self.analyzer.as_mut() {
            Some(a) => segment(a, query),
            None => query.to_lowercase(),
        };
        format!("\"{}\"", prepared.replace('"', "\"\""))
    }

    fn search(&mut self, query: &str) -> Result<Hits> {
        let t0 = Instant::now();
        let expr = self.match_expr(query);
        let mut stmt = self.conn.prepare_cached("SELECT rowid FROM docs WHERE docs MATCH ?1")?;
        let rows = stmt.query_map([&expr], |row| row.get::<_, i64>(0))?;
        let mut ids = Vec::new();
        for r in rows {
            ids.push(r?);
        }
        let elapsed = t0.elapsed();
        self.last = ids;
        Ok(Hits {
            count: self.last.len(),
            elapsed,
        })
    }

    fn last_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM paths WHERE id = ?1")?;
        let mut out = Vec::with_capacity(self.last.len());
        for id in &self.last {
            out.push(stmt.query_row([id], |row| row.get::<_, String>(0))?);
        }
        Ok(out)
    }
}

// ── Single-document update (the M2 delta-merge cost question) ──────────────

/// Replace one document in place and return the time the replacement took.
///
/// Open cost is excluded; what is measured is delete + insert + commit, which
/// is what a search-time delta merge (issue #3) would have to pay per changed
/// file.
pub fn update_one(engine: Engine, index_dir: &Path, doc: Doc) -> Result<Duration> {
    match engine {
        Engine::Tantivy => {
            let index = Index::open_in_dir(index_dir)?;
            register(&index)?;
            let schema = index.schema();
            let path_f = schema.get_field("path")?;
            let mtime_f = schema.get_field("mtime_ns")?;
            let body_f = schema.get_field("body")?;
            let mut writer: tantivy::IndexWriter = index.writer(WRITER_HEAP)?;
            let t0 = Instant::now();
            writer.delete_term(Term::from_field_text(path_f, &doc.path));
            writer.add_document(
                doc!(path_f => doc.path, mtime_f => doc.mtime_ns, body_f => doc.body),
            )?;
            writer.commit()?;
            Ok(t0.elapsed())
        }
        e => {
            let conn = Connection::open(db_path(index_dir))?;
            let mut analyzer = match e {
                Engine::Fts5Lindera => Some(ja_analyzer()?),
                _ => None,
            };
            let id: i64 = conn.query_row(
                "SELECT id FROM paths WHERE path = ?1",
                [&doc.path],
                |row| row.get(0),
            )?;
            let indexed = match analyzer.as_mut() {
                Some(a) => segment(a, &doc.body),
                None => doc.body.to_lowercase(),
            };
            let t0 = Instant::now();
            conn.execute_batch("BEGIN")?;
            conn.execute("DELETE FROM docs WHERE rowid = ?1", [id])?;
            conn.execute(
                "INSERT INTO docs(rowid, body) VALUES(?1, ?2)",
                rusqlite::params![id, indexed],
            )?;
            conn.execute(
                "UPDATE paths SET mtime_ns = ?1 WHERE id = ?2",
                rusqlite::params![doc.mtime_ns, id],
            )?;
            conn.execute_batch("COMMIT")?;
            Ok(t0.elapsed())
        }
    }
}
