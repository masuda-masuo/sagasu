//! What a file *name* can be read to mean: dates, version markers, naming
//! conventions.
//!
//! These three generators share a shape that nothing else in the engine has —
//! each is a guess about human intent, so each is tuned for *precision* and each
//! carries the measurement that set its threshold. That is the reason they sit
//! together in one file: the next person to loosen one of these rules needs the
//! `/usr` regressions recorded below in front of them.
//!
//! The name's plain word tokens deliberately stay out of the tag set (see the
//! module docs of [`super`]); what is extracted here are *interpretations* of
//! the name rather than a repeat of its text.

use super::TOKEN_SEPARATORS;

// ── Date extraction ─────────────────────────────────────────────────────────

/// Earliest year accepted as a date. Below this a four-digit run is far more
/// likely to be an identifier than a year.
const MIN_YEAR: u32 = 1970;
/// Latest year accepted as a date.
const MAX_YEAR: u32 = 2099;

/// Date tags found in a name: `YYYY` always, `YYYY-MM` when a month is present.
///
/// The day is deliberately dropped. A facet axis with one bucket per day is not
/// navigable, and `date:2024-03` is what someone actually recalls about a file.
///
/// Matching works on *maximal* digit runs, so a year is never taken out of the
/// middle of a longer number: `20240315` is a date, `1234567890` is not.
pub fn date_values(s: &str) -> Vec<String> {
    let runs = digit_runs(s);
    let mut out: Vec<String> = Vec::new();

    let push = |year: u32, month: Option<u32>, out: &mut Vec<String>| {
        let y = year.to_string();
        if !out.contains(&y) {
            out.push(y);
        }
        if let Some(m) = month {
            let ym = format!("{year:04}-{m:02}");
            if !out.contains(&ym) {
                out.push(ym);
            }
        }
    };

    // Single runs: YYYYMMDD, YYYYMM, YYYY.
    for run in &runs {
        match run.digits.len() {
            8 => {
                let (y, m, d) = (
                    num(&run.digits[0..4]),
                    num(&run.digits[4..6]),
                    num(&run.digits[6..8]),
                );
                if valid_ymd(y, m, d) {
                    push(y, Some(m), &mut out);
                }
            }
            6 => {
                let (y, m) = (num(&run.digits[0..4]), num(&run.digits[4..6]));
                if valid_ymd(y, m, 1) {
                    push(y, Some(m), &mut out);
                }
            }
            4 => {
                let y = num(&run.digits);
                if (MIN_YEAR..=MAX_YEAR).contains(&y) {
                    push(y, None, &mut out);
                }
            }
            _ => {}
        }
    }

    // Separated runs: YYYY-MM and YYYY-MM-DD (any of `-`, `_`, `.`, `/`).
    for pair in runs.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if a.digits.len() == 4 && b.digits.len() == 2 && single_date_separator(s, a.end, b.start) {
            let (y, m) = (num(&a.digits), num(&b.digits));
            if valid_ymd(y, m, 1) {
                push(y, Some(m), &mut out);
            }
        }
    }

    out
}

struct DigitRun {
    digits: String,
    start: usize,
    end: usize,
}

/// Maximal runs of ASCII digits, with their byte offsets.
fn digit_runs(s: &str) -> Vec<DigitRun> {
    let mut runs = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (i, ch) in s.char_indices() {
        if ch.is_ascii_digit() {
            current.get_or_insert_with(|| (i, String::new())).1.push(ch);
        } else if let Some((start, digits)) = current.take() {
            let end = start + digits.len();
            runs.push(DigitRun { digits, start, end });
        }
    }
    if let Some((start, digits)) = current {
        let end = start + digits.len();
        runs.push(DigitRun { digits, start, end });
    }
    runs
}

/// Whether exactly one date separator sits between two digit runs.
fn single_date_separator(s: &str, from: usize, to: usize) -> bool {
    to == from + 1 && matches!(&s[from..to], "-" | "_" | "." | "/")
}

fn num(digits: &str) -> u32 {
    digits.parse().unwrap_or(0)
}

fn valid_ymd(y: u32, m: u32, d: u32) -> bool {
    (MIN_YEAR..=MAX_YEAR).contains(&y) && (1..=12).contains(&m) && (1..=31).contains(&d)
}

// ── Version extraction ──────────────────────────────────────────────────────

/// ASCII keywords recognised as version markers, as whole tokens.
const VERSION_KEYWORDS: &[(&str, &str)] = &[
    ("final", "final"),
    ("draft", "draft"),
    ("old", "old"),
    ("latest", "latest"),
    ("wip", "wip"),
];

/// Japanese version markers. These are matched as substrings because Japanese
/// file names have no separators to tokenise on; each string is long and
/// specific enough that a chance occurrence is not a realistic worry.
const VERSION_SUBSTRINGS_JA: &[(&str, &str)] =
    &[("最終", "final"), ("最新", "latest"), ("下書き", "draft")];

/// Suffixes of the OS "duplicate of another file" convention, exactly as the
/// shells write them: `report - Copy.txt`, `報告書 のコピー.txt`.
///
/// Anything looser was measured to be wrong. A bare `copy` token matched
/// `copy.bc`, `geqo_copy.bc` and `COPY.7` (PostgreSQL's `COPY` statement);
/// dropping the space and accepting `-copy` matched `edit-copy.svg` and
/// `folder-copy.svg` (icon names). A version namespace that fires on subject
/// matter is a namespace users learn to ignore.
const COPY_SUFFIXES: &[&str] = &["- copy", "のコピー"];

/// Version / revision markers in a file name stem.
pub fn version_values(stem: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |v: String, out: &mut Vec<String>| {
        if !out.contains(&v) {
            out.push(v);
        }
    };

    for token in stem.split(TOKEN_SEPARATORS).filter(|s| !s.is_empty()) {
        let lower = token.to_lowercase();
        if let Some((_, tag)) = VERSION_KEYWORDS.iter().find(|(k, _)| *k == lower) {
            push(tag.to_string(), &mut out);
            continue;
        }
        for (prefix, label) in [("v", "v"), ("ver", "v"), ("rev", "rev"), ("r", "rev")] {
            let Some(rest) = lower.strip_prefix(prefix) else {
                continue;
            };
            // `r` alone would fire on far too much; require it to be the whole
            // token followed by digits, and keep the number short so an id like
            // `v20240315` is read as a date, not as version 20,240,315.
            if rest.is_empty() || rest.len() > 3 || !rest.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let n: u32 = rest.parse().unwrap_or(0);
            push(format!("{label}{n}"), &mut out);
            break;
        }
    }

    for (needle, tag) in VERSION_SUBSTRINGS_JA {
        if stem.contains(needle) {
            push((*tag).to_string(), &mut out);
        }
    }

    // Windows' duplicate-file conventions: `report (3).xlsx`, `report - Copy`,
    // `報告書 のコピー`. `report - Copy (2)` satisfies the first test as well.
    let trimmed = stem.trim_end().to_lowercase();
    if trailing_paren_number(stem).is_some() || COPY_SUFFIXES.iter().any(|s| trimmed.ends_with(s)) {
        push("copy".to_string(), &mut out);
    }

    // `旧` is a strong marker only at the start of a name.
    if stem.starts_with('旧') {
        push("old".to_string(), &mut out);
    }

    out
}

/// `Some(n)` when the stem ends with ` (n)` / `(n)` for a decimal `n`.
fn trailing_paren_number(stem: &str) -> Option<u32> {
    let trimmed = stem.trim_end();
    let inner = trimmed.strip_suffix(')')?;
    let open = inner.rfind('(')?;
    let digits = &inner[open + 1..];
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

// ── Naming patterns ─────────────────────────────────────────────────────────

/// Stem prefixes that identify a screenshot, across the common OS conventions.
const SCREENSHOT_PREFIXES: &[&str] = &[
    "screenshot",
    "screen shot",
    "screen_shot",
    "screen-shot",
    "screencapture",
    "scr-",
    "scr_",
    "snipaste_",
    "スクリーンショット",
    "画面キャプチャ",
];

/// Camera / phone naming prefixes, each followed by at least four digits.
const CAMERA_PREFIXES: &[&str] = &[
    "img_", "img-", "img", "dsc", "dscf", "dscn", "pxl_", "mvi_", "gopr", "dji_", "vid_", "pano_",
];

/// Extensions that mean "this file is a backup of another one".
///
/// The extension (or a trailing `~`) is the whole of the backup signal. The word
/// "backup" *inside* a name is a topic, not a status: measured against `/usr` it
/// claimed `pg_basebackup`, `dpkg-db-backup.service` and `Debconf/DbDriver/
/// Backup.pm`, none of which is a backup of anything. `report_backup.xlsx` is
/// therefore deliberately not claimed either — precision is worth more here,
/// because that file is still one `sagasu find backup` away.
const BACKUP_EXTS: &[&str] = &["bak", "bkp", "old", "orig", "backup", "save", "sav"];

/// Extensions that mean "this file is mid-write or mid-download".
const TEMP_EXTS: &[&str] = &[
    "tmp",
    "temp",
    "swp",
    "swo",
    "swn",
    "part",
    "partial",
    "crdownload",
];

/// Naming conventions recognised in a file name.
pub fn pattern_values(file_name: &str, stem: &str, ext: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |v: &str, out: &mut Vec<String>| {
        if !out.iter().any(|e| e == v) {
            out.push(v.to_string());
        }
    };

    let lower_stem = stem.to_lowercase();
    let lower_name = file_name.to_lowercase();

    // The prefix has to end at a boundary: a separator, a digit, or the end of
    // the stem. Measured against `/usr`, a bare `starts_with` claimed
    // LibreOffice's `screenshotannotationdialog.ui` — a UI definition that
    // merely begins with the word.
    if SCREENSHOT_PREFIXES
        .iter()
        .any(|p| starts_with_at_boundary(&lower_stem, p))
    {
        push("screenshot", &mut out);
    }

    if is_camera_name(&lower_stem) {
        push("camera", &mut out);
    }

    let ext_is =
        |list: &[&str]| ext.is_some_and(|e| list.iter().any(|x| x.eq_ignore_ascii_case(e)));

    if ext_is(BACKUP_EXTS) || lower_name.ends_with('~') || stem.ends_with("バックアップ") {
        push("backup", &mut out);
    }

    if ext_is(TEMP_EXTS)
        || lower_name.starts_with("~$")
        || lower_name.starts_with(".~lock.")
        || lower_name.starts_with('~')
    {
        push("temp", &mut out);
    }

    // A dot-prefixed name is a naming convention, not a filesystem attribute,
    // so it is portable to say so — unlike the Windows hidden bit, which the
    // crawl does not read.
    if file_name.starts_with('.') && file_name.len() > 1 {
        push("dotfile", &mut out);
    }

    out
}

/// Whether `s` starts with `prefix` *and* the prefix ends at a word boundary.
///
/// A boundary is the end of the string, one of the usual name separators, or a
/// digit — which covers every real form of these conventions (`Screenshot
/// 2024-…`, `Screenshot_2024…`, `SCR-20240315`, plain `screenshot.png`) without
/// matching a longer word that merely begins the same way.
fn starts_with_at_boundary(s: &str, prefix: &str) -> bool {
    let Some(rest) = s.strip_prefix(prefix) else {
        return false;
    };
    match rest.chars().next() {
        None => true,
        Some(c) => c.is_ascii_digit() || matches!(c, ' ' | '_' | '-' | '.' | '(' | '　'),
    }
}

/// Whether a lowercased stem follows a camera / phone naming convention.
fn is_camera_name(stem: &str) -> bool {
    // `P1010123`: Panasonic / Olympus use `P` plus exactly seven digits.
    if let Some(rest) = stem.strip_prefix('p') {
        if rest.len() == 7 && rest.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    for prefix in CAMERA_PREFIXES {
        let Some(rest) = stem.strip_prefix(prefix) else {
            continue;
        };
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits < 4 {
            continue;
        }
        let tail = &rest[digits..];
        if tail.is_empty() || tail.starts_with(['_', '-', '.']) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_come_from_maximal_digit_runs_only() {
        assert_eq!(date_values("report-2024-03-15"), vec!["2024", "2024-03"]);
        assert_eq!(date_values("IMG_20240315_120000"), vec!["2024", "2024-03"]);
        assert_eq!(date_values("archive/2024"), vec!["2024"]);
        // A ten-digit id must not yield a year from its first four digits.
        assert!(date_values("id1234567890").is_empty());
        // Out-of-range year: nothing at all.
        assert!(date_values("18990101").is_empty());
        // Impossible month: the month is dropped, the year survives on its own
        // four-digit run. Losing the year too would be the wrong trade — a
        // directory called `2024-13` is still about 2024.
        assert_eq!(date_values("2024-13"), vec!["2024"]);
    }

    #[test]
    fn version_markers_are_whole_tokens() {
        assert_eq!(version_values("plan_v2_final"), vec!["v2", "final"]);
        assert_eq!(version_values("提案書_最終"), vec!["final"]);
        // `v` inside a word must not fire.
        assert!(version_values("service").is_empty());
        // A long digit run after `v` is a date or an id, not a version.
        assert!(version_values("v20240315").is_empty());
    }

    #[test]
    fn the_duplicate_convention_is_a_suffix_not_the_word_copy() {
        assert_eq!(version_values("report (3)"), vec!["copy"]);
        assert_eq!(version_values("report - Copy"), vec!["copy"]);
        assert_eq!(version_values("report - Copy (2)"), vec!["copy"]);
        assert_eq!(version_values("報告書 のコピー"), vec!["copy"]);
        // Regressions from the `/usr` measurement: PostgreSQL's `COPY`
        // statement, and icon names, both matched looser rules.
        assert!(version_values("copy").is_empty());
        assert!(version_values("geqo_copy").is_empty());
        assert!(version_values("edit-copy").is_empty());
        assert!(version_values("folder-copy").is_empty());
    }

    #[test]
    fn backup_comes_from_the_extension_not_from_the_word() {
        assert_eq!(
            pattern_values("report.xlsx.bak", "report.xlsx", Some("bak")),
            vec!["backup"]
        );
        assert_eq!(
            pattern_values("notes.txt~", "notes.txt~", None),
            vec!["backup"]
        );
        // Regressions from the `/usr` measurement: programs *about* backups.
        assert!(pattern_values("pg_basebackup", "pg_basebackup", None).is_empty());
        assert!(pattern_values("Backup.pm", "Backup", Some("pm")).is_empty());
        assert!(pattern_values("045_backup.t", "045_backup", Some("t")).is_empty());
    }

    #[test]
    fn a_screenshot_prefix_has_to_end_at_a_boundary() {
        for name in [
            "Screenshot 2024-03-15",
            "Screenshot_2024-03-15",
            "SCR-20240315",
            "screenshot",
            "スクリーンショット 2024-03-15",
        ] {
            assert_eq!(
                pattern_values(name, name, None),
                vec!["screenshot"],
                "{name} should be a screenshot"
            );
        }
        // Regression from the `/usr` measurement: a LibreOffice UI definition.
        assert!(pattern_values(
            "screenshotannotationdialog.ui",
            "screenshotannotationdialog",
            Some("ui")
        )
        .is_empty());
    }

    #[test]
    fn naming_patterns_recognise_the_common_conventions() {
        assert_eq!(
            pattern_values(
                "Screenshot 2024-03-15.png",
                "Screenshot 2024-03-15",
                Some("png")
            ),
            vec!["screenshot"]
        );
        assert_eq!(
            pattern_values("IMG_4821.JPG", "IMG_4821", Some("jpg")),
            vec!["camera"]
        );
        assert_eq!(
            pattern_values("~$plan.docx", "~$plan", Some("docx")),
            vec!["temp"]
        );
        assert_eq!(
            pattern_values(".gitignore", ".gitignore", None),
            vec!["dotfile"]
        );
        // `image1.png` starts with `img`-ish text but has no digit run there.
        assert!(pattern_values("image1.png", "image1", Some("png")).is_empty());
    }
}
