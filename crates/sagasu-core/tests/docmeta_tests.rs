//! Integration tests for issue #40: document body extraction and embedded
//! metadata tags.
//!
//! Every fixture is a **real file** — a genuine deflated ZIP, a PDF with a
//! correct cross-reference table, a JPEG with a real EXIF APP1 block — built by
//! `common/docmeta_fixtures.rs`. A test that fed the parsers hand-written XML
//! would prove the XML walk works and nothing about the containers, which is
//! where the formats actually differ.
//!
//! Two properties are load-bearing and each has its own test rather than being
//! implied by the others:
//!
//! 1. **The round trip is Japanese.** An ASCII fixture passes over a broken
//!    UTF-16 or ToUnicode path without noticing.
//! 2. **A broken file costs one row.** The scan continues, the ledger still
//!    balances, and the reason is recorded.

#![cfg(any(feature = "office", feature = "pdf", feature = "exif"))]

use std::fs;
use std::path::{Path, PathBuf};

use sagasu_core::fulltext::{self, FulltextConfig, FulltextSummary, SkipReason};
use sagasu_core::tagindex::{self, TagConfig};
use sagasu_core::tagrules::RuleSet;
use sagasu_core::walk::{self, CrawlConfig};

#[path = "common/docmeta_fixtures.rs"]
mod fixtures;

// ── helpers ─────────────────────────────────────────────────────────────────

fn tmp_dirs(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("sagasu_doc_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let (data, db, index) = (base.join("data"), base.join("db"), base.join("ft"));
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&db).unwrap();
    (data, db, index)
}

fn db_path(db_dir: &Path) -> PathBuf {
    db_dir.join("test.db")
}

fn crawl(data: &Path, db_dir: &Path) {
    walk::crawl(CrawlConfig {
        root: data.to_path_buf(),
        db_path: db_path(db_dir),
        exclude: vec![],
        no_default_excludes: false,
        hidden: Default::default(),
        use_gitignore: false,
        threads: 1,
    })
    .unwrap();
}

fn index_all(data: &Path, db_dir: &Path, index_dir: &Path) -> FulltextSummary {
    crawl(data, db_dir);
    fulltext::build(&FulltextConfig {
        db_path: db_path(db_dir),
        index_dir: index_dir.to_path_buf(),
        max_size: fulltext::DEFAULT_MAX_SIZE,
        text_policy: Default::default(),
        no_sniff: false,
        threads: 2,
        heap_bytes: 16 * 1024 * 1024,
    })
    .unwrap()
}

fn search_hits(index_dir: &Path, query: &str) -> Vec<String> {
    fulltext::search(&fulltext::SearchConfig::new(index_dir, query))
        .unwrap()
        .hits
        .iter()
        .map(|h| {
            Path::new(h.display_path())
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect()
}

/// The tags of one file, as `namespace:value` strings, straight from the
/// engine's own explain path (which is what reads the embedded metadata).
fn tags_of(path: &Path) -> Vec<String> {
    tagindex::explain(path, None, &RuleSet::empty(), true)
        .unwrap()
        .tags
        .iter()
        .map(|(tag, _)| tag.to_string())
        .collect()
}

/// Assert the ledger design.md §4-2 promises still balances.
fn assert_ledger(summary: &FulltextSummary) {
    assert_eq!(
        summary.candidates,
        summary.indexed + summary.skipped_total(),
        "every candidate must be either indexed or explained: {summary:?}"
    );
    assert_eq!(
        summary.indexed,
        summary.accepted_by_ext + summary.accepted_by_sniff + summary.accepted_by_extract,
        "the three acceptance routes must add up to `indexed`: {summary:?}"
    );
}

// ── Office bodies ───────────────────────────────────────────────────────────

#[cfg(feature = "office")]
#[test]
fn a_docx_body_reaches_the_full_text_index_in_japanese() {
    let (data, db, index) = tmp_dirs("docx_body");
    fixtures::write_docx(
        &data.join("報告書.docx"),
        &["四半期の売上は前年比で増加した。", "詳細は別紙を参照。"],
        None,
    );

    let summary = index_all(&data, &db, &index);
    assert_eq!(summary.indexed, 1, "{summary:?}");
    assert_eq!(summary.accepted_by_extract, 1, "{summary:?}");
    assert_ledger(&summary);

    assert_eq!(search_hits(&index, "売上"), ["報告書.docx"]);
    // The second paragraph proves the whole part was walked, not just the
    // first run the parser happened to see.
    assert_eq!(search_hits(&index, "別紙"), ["報告書.docx"]);
}

#[cfg(feature = "office")]
#[test]
fn an_xlsx_reads_shared_and_inline_cells_alike() {
    let (data, db, index) = tmp_dirs("xlsx_body");
    use fixtures::Cell;
    fixtures::write_xlsx(
        &data.join("売上表.xlsx"),
        &["共有された見出し", "四月"],
        &[
            vec![Cell::Shared(0), Cell::Inline("直接埋め込まれた値")],
            vec![Cell::Shared(1), Cell::Number("12345")],
        ],
        None,
    );

    let summary = index_all(&data, &db, &index);
    assert_eq!(summary.accepted_by_extract, 1, "{summary:?}");
    assert_ledger(&summary);

    assert_eq!(search_hits(&index, "共有された見出し"), ["売上表.xlsx"]);
    // The inline path never touches the shared string table. A reader that only
    // resolves shared indices comes back empty here, which looks exactly like a
    // spreadsheet with no text in it.
    assert_eq!(search_hits(&index, "直接埋め込まれた値"), ["売上表.xlsx"]);
    assert_eq!(search_hits(&index, "12345"), ["売上表.xlsx"]);
}

#[cfg(feature = "office")]
#[test]
fn a_pptx_is_read_in_slide_order_not_archive_order() {
    let (data, db, index) = tmp_dirs("pptx_body");
    // The fixture writes the slides into the ZIP backwards, and there are ten
    // of them so that lexicographic naming (slide10 < slide2) disagrees with
    // numeric order too.
    let slides: Vec<String> = (1..=10).map(|i| format!("{i}枚目の内容")).collect();
    let refs: Vec<&str> = slides.iter().map(String::as_str).collect();
    fixtures::write_pptx(&data.join("発表.pptx"), &refs, None);

    let summary = index_all(&data, &db, &index);
    assert_eq!(summary.accepted_by_extract, 1, "{summary:?}");
    assert_eq!(search_hits(&index, "10枚目の内容"), ["発表.pptx"]);

    let body = sagasu_core::docmeta::extract_body(
        &data.join("発表.pptx"),
        sagasu_core::docmeta::BodyFormat::Pptx,
        1 << 20,
    )
    .unwrap();
    let first = body.find("1枚目").unwrap();
    let second = body.find("2枚目").unwrap();
    let tenth = body.find("10枚目").unwrap();
    assert!(
        first < second && second < tenth,
        "slide order lost:\n{body}"
    );
}

// ── PDF body ────────────────────────────────────────────────────────────────

#[cfg(feature = "pdf")]
#[test]
fn a_pdf_with_a_tounicode_cmap_round_trips_japanese() {
    let (data, db, index) = tmp_dirs("pdf_body");
    fs::write(
        data.join("設計書.pdf"),
        fixtures::minimal_pdf("日本語の本文", None),
    )
    .unwrap();

    let summary = index_all(&data, &db, &index);
    assert_eq!(summary.indexed, 1, "{summary:?}");
    assert_eq!(summary.accepted_by_extract, 1, "{summary:?}");
    assert_ledger(&summary);
    assert_eq!(search_hits(&index, "日本語"), ["設計書.pdf"]);
}

// ── Embedded metadata → tags ────────────────────────────────────────────────

#[cfg(feature = "office")]
#[test]
fn office_document_properties_become_tags() {
    let (data, _db, _index) = tmp_dirs("ooxml_tags");
    let path = data.join("提案.docx");
    fixtures::write_docx(
        &path,
        &["本文"],
        Some(fixtures::core_props(
            "四半期レポート",
            "増田 太郎",
            "山田 花子",
            "2024-03-15T01:02:03Z",
        )),
    );

    let tags = tags_of(&path);
    assert!(tags.contains(&"author:増田 太郎".to_string()), "{tags:?}");
    // `cp:lastModifiedBy` feeds the same namespace: "a document 山田 touched"
    // is one question, and two namespaces would split the answer.
    assert!(tags.contains(&"author:山田 花子".to_string()), "{tags:?}");
    assert!(
        tags.contains(&"title:四半期レポート".to_string()),
        "{tags:?}"
    );
    assert!(tags.contains(&"date:2024".to_string()), "{tags:?}");
    assert!(tags.contains(&"date:2024-03".to_string()), "{tags:?}");
    // The day is deliberately not a bucket.
    assert!(!tags.iter().any(|t| t == "date:2024-03-15"), "{tags:?}");
}

#[cfg(feature = "pdf")]
#[test]
fn a_pdf_info_dictionary_becomes_tags() {
    let (data, _db, _index) = tmp_dirs("pdf_tags");
    let path = data.join("仕様.pdf");
    fs::write(
        &path,
        fixtures::minimal_pdf(
            "本文",
            Some(("増田 太郎", "設計仕様書", "D:20240315101112+09'00'")),
        ),
    )
    .unwrap();

    let tags = tags_of(&path);
    assert!(tags.contains(&"author:増田 太郎".to_string()), "{tags:?}");
    assert!(tags.contains(&"title:設計仕様書".to_string()), "{tags:?}");
    assert!(tags.contains(&"date:2024-03".to_string()), "{tags:?}");
}

#[cfg(feature = "exif")]
#[test]
fn exif_becomes_camera_and_date_tags() {
    let (data, _db, _index) = tmp_dirs("exif_tags");
    let path = data.join("写真.jpg");
    fs::write(
        &path,
        fixtures::minimal_jpeg_with_exif("NIKON CORPORATION", "NIKON D750", "2024:03:15 10:11:12"),
    )
    .unwrap();

    let tags = tags_of(&path);
    // Make and Model overlap; one camera must be one bucket.
    assert!(tags.contains(&"camera:nikon d750".to_string()), "{tags:?}");
    assert!(tags.contains(&"date:2024".to_string()), "{tags:?}");
    assert!(tags.contains(&"date:2024-03".to_string()), "{tags:?}");
}

#[cfg(feature = "exif")]
#[test]
fn an_image_without_exif_is_not_a_failure() {
    let (data, db, _index) = tmp_dirs("exif_absent");
    // A two-byte JPEG: valid enough to open, carrying nothing.
    fs::write(data.join("screenshot.jpg"), [0xFF, 0xD8, 0xFF, 0xD9]).unwrap();
    crawl(&data, &db);

    let summary = tagindex::build(&TagConfig::new(db_path(&db))).unwrap();
    assert_eq!(summary.embedded_candidates, 1, "{summary:?}");
    assert_eq!(summary.embedded_read, 0, "{summary:?}");
    // Most images on a real disk have no EXIF. Counting them as failures would
    // bury the handful of genuinely broken files under tens of thousands.
    assert_eq!(summary.embedded_failed, 0, "{summary:?}");
}

#[cfg(feature = "office")]
#[test]
fn the_tag_pass_reports_its_embedded_coverage() {
    let (data, db, _index) = tmp_dirs("embedded_coverage");
    fixtures::write_docx(
        &data.join("a.docx"),
        &["本文"],
        Some(fixtures::core_props(
            "題",
            "増田",
            "増田",
            "2024-03-15T00:00:00Z",
        )),
    );
    fixtures::write_docx(&data.join("b.docx"), &["本文"], None);
    fs::write(data.join("broken.docx"), b"not a zip at all").unwrap();
    crawl(&data, &db);

    let summary = tagindex::build(&TagConfig::new(db_path(&db))).unwrap();
    assert_eq!(summary.embedded_candidates, 3, "{summary:?}");
    assert_eq!(summary.embedded_read, 1, "{summary:?}");
    assert_eq!(summary.embedded_failed, 1, "{summary:?}");
    assert_eq!(summary.embedded_errors.len(), 1, "{summary:?}");
    assert!(
        summary.embedded_errors[0].0.ends_with("broken.docx"),
        "{summary:?}"
    );
}

// ── Failure is per file, never per scan ─────────────────────────────────────

#[test]
fn a_corrupt_document_costs_one_row_and_the_scan_continues() {
    let (data, db, index) = tmp_dirs("corrupt");
    fs::write(data.join("survivor.md"), "生き残る本文\n").unwrap();
    #[cfg(feature = "office")]
    {
        // Right extension, wrong bytes: a truncated ZIP and a ZIP with no
        // `word/document.xml` fail at two different points in the reader.
        fs::write(data.join("truncated.docx"), b"PK\x03\x04\x00\x00broken").unwrap();
        fixtures::write_zip(&data.join("wrong-parts.docx"), &[("hello.txt", b"hi")]);
    }
    #[cfg(feature = "pdf")]
    {
        fs::write(data.join("garbage.pdf"), b"%PDF-1.7\nnot actually a pdf\n").unwrap();
    }

    let summary = index_all(&data, &db, &index);

    // The point of the test: the plain-text file next to the broken ones is
    // still in the index.
    assert_eq!(search_hits(&index, "生き残る"), ["survivor.md"]);
    assert_ledger(&summary);

    let broken = summary
        .skipped
        .get(&SkipReason::ExtractFailed)
        .copied()
        .unwrap_or(0);
    let expected = cfg!(feature = "office") as u64 * 2 + cfg!(feature = "pdf") as u64;
    assert_eq!(broken, expected, "{summary:?}");
    assert_eq!(
        summary.extract_errors.len(),
        expected as usize,
        "each failure keeps its reason: {:?}",
        summary.extract_errors
    );
}

#[cfg(feature = "pdf")]
#[test]
fn a_pdf_without_a_tounicode_map_fails_visibly_rather_than_silently() {
    let (data, db, index) = tmp_dirs("no_tounicode");
    fs::write(data.join("古い.pdf"), fixtures::pdf_without_tounicode()).unwrap();
    fs::write(data.join("other.md"), "隣のファイル\n").unwrap();

    let summary = index_all(&data, &db, &index);
    assert_ledger(&summary);

    // Whichever way it lands — no decodable text (`empty`) or a parser refusal
    // (`document extraction failed`) — it must be *accounted for* and must not
    // be counted as an indexed document. A PDF nobody can read that reports as
    // indexed is the failure this project treats as the worst one.
    assert!(
        !summary
            .extract_errors
            .iter()
            .any(|(p, _)| p.contains("other.md")),
        "{summary:?}"
    );
    assert_eq!(summary.indexed, 1, "only the .md has a body: {summary:?}");
    assert_eq!(search_hits(&index, "隣"), ["other.md"]);
}

// ── Determinism ─────────────────────────────────────────────────────────────

#[cfg(feature = "office")]
#[test]
fn extraction_is_byte_identical_across_runs() {
    let (data, _db, _index) = tmp_dirs("determinism");
    let path = data.join("同じ.docx");
    fixtures::write_docx(&path, &["一段落目", "二段落目", "三段落目"], None);

    let once =
        sagasu_core::docmeta::extract_body(&path, sagasu_core::docmeta::BodyFormat::Docx, 1 << 20)
            .unwrap();
    let twice =
        sagasu_core::docmeta::extract_body(&path, sagasu_core::docmeta::BodyFormat::Docx, 1 << 20)
            .unwrap();
    assert_eq!(once, twice);
    assert_eq!(once, "一段落目\n二段落目\n三段落目");
}

#[cfg(feature = "office")]
#[test]
fn the_size_budget_truncates_rather_than_failing() {
    let (data, _db, _index) = tmp_dirs("budget");
    let path = data.join("長い.docx");
    let long: Vec<String> = (0..500).map(|i| format!("段落{i}の本文")).collect();
    let refs: Vec<&str> = long.iter().map(String::as_str).collect();
    fixtures::write_docx(&path, &refs, None);

    // A partial body beats no document at all, and it is the same trade-off the
    // plain-text path already makes by refusing files over the limit outright.
    let body =
        sagasu_core::docmeta::extract_body(&path, sagasu_core::docmeta::BodyFormat::Docx, 64)
            .unwrap();
    assert!(body.len() <= 64, "{} bytes", body.len());
    assert!(body.starts_with("段落0の本文"), "{body}");
}
