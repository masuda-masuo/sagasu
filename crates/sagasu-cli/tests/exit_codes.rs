//! Exit-code contract (issue #49, docs/cli.md §6): 0 = the command ran
//! correctly and the answer is non-empty, 1 = a read command ran correctly and
//! the answer is empty, 2 = every error and every unusable setup.
//!
//! The real binary is spawned against a fixture directory under the system
//! temp dir, so the whole pipeline (`index` → `fulltext` → `tag`) is exercised
//! end to end — a unit test could not prove that the exit code a script sees
//! matches the one the docs promise.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Path to the compiled `sagasu` binary, supplied by Cargo to integration
/// tests of a `[[bin]]` crate (`CARGO_BIN_EXE_<name>`).
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sagasu")
}

/// Run the real binary; returns (exit code, stdout, stderr).
fn run(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(bin())
        .args(args)
        .output()
        .expect("failed to spawn sagasu");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Run the real binary with extra environment variables.
///
/// Used to pin the delta source: since #58 the marker `sagasu index` writes is
/// platform-dependent (USN on Windows, mtime elsewhere), so a test that wants
/// one specific marker has to ask for it rather than assume the platform's.
fn run_with_env(args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let mut cmd = Command::new(bin());
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("failed to spawn sagasu");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sagasu-exitcodes-{tag}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A fixture: one crawl root with one text file, and the db / fulltext-index
/// paths next to it (outside the crawl tree).
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    db: PathBuf,
    ft: PathBuf,
}

impl Fixture {
    fn new(tag: &str, file: &str, content: &str) -> Self {
        let dir = TempDir::new(tag);
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(file), content).unwrap();
        Fixture {
            db: dir.path().join("index.db"),
            ft: dir.path().join("fulltext-index"),
            root,
            _dir: dir,
        }
    }

    fn index(&self) -> i32 {
        run(&["index", self.root.to_str().unwrap(), "--db", self.db.to_str().unwrap()]).0
    }

    fn fulltext(&self) -> i32 {
        run(&[
            "fulltext",
            "--db",
            self.db.to_str().unwrap(),
            "--index-dir",
            self.ft.to_str().unwrap(),
        ])
        .0
    }

    fn tag(&self) -> i32 {
        run(&["tag", "--db", self.db.to_str().unwrap()]).0
    }

    fn find(&self, query: &str) -> i32 {
        run(&["find", query, "--db", self.db.to_str().unwrap()]).0
    }

    fn search(&self, query: &str) -> i32 {
        run(&[
            "search",
            query,
            "--db",
            self.db.to_str().unwrap(),
            "--index-dir",
            self.ft.to_str().unwrap(),
        ])
        .0
    }

    fn tags(&self, args: &[&str]) -> i32 {
        let mut full = vec!["tags"];
        full.extend_from_slice(args);
        full.push("--db");
        full.push(self.db.to_str().unwrap());
        run(&full).0
    }

    fn browse(&self, args: &[&str]) -> i32 {
        let mut full = vec!["browse"];
        full.extend_from_slice(args);
        full.push("--db");
        full.push(self.db.to_str().unwrap());
        run(&full).0
    }
}

// ── find ─────────────────────────────────────────────────────────────────────

#[test]
fn find_hit_exits_0() {
    let fx = Fixture::new("find-hit", "a.txt", "needle\n");
    assert_eq!(fx.index(), 0);
    assert_eq!(fx.find("a.txt"), 0, "a path substring that exists must exit 0");
}

#[test]
fn find_miss_exits_1() {
    let fx = Fixture::new("find-miss", "a.txt", "needle\n");
    assert_eq!(fx.index(), 0);
    assert_eq!(
        fx.find("zzz-no-such-file"),
        1,
        "a usable index with zero results must exit 1"
    );
}

#[test]
fn find_with_unusable_db_exits_2() {
    let fx = Fixture::new("find-bad-db", "a.txt", "needle\n");
    let missing = fx._dir.path().join("no-such-dir").join("x.db");
    let (code, _, stderr) = run(&["find", "x", "--db", missing.to_str().unwrap()]);
    assert_eq!(code, 2, "a DB that cannot be opened is an error, not an empty answer");
    assert!(
        stderr.contains("error: failed to open metadata index"),
        "the error must still be reported on stderr: {stderr}"
    );
}

#[test]
fn find_on_empty_index_exits_2() {
    // The §7 #3 case: an empty metadata index is an unusable setup, not a
    // legitimate "no match" (the old contract could not tell them apart).
    let fx = Fixture::new("find-empty-index", "a.txt", "needle\n");
    assert_eq!(fx.index(), 0);
    // Delete the file and re-crawl: indexing nothing exits 2, but the crawl
    // still leaves a valid DB whose live count is zero.
    std::fs::remove_file(fx.root.join("a.txt")).unwrap();
    assert_eq!(fx.index(), 2);
    let (code, _, stderr) = run(&["find", "a", "--db", fx.db.to_str().unwrap()]);
    assert_eq!(code, 2, "an empty metadata index must exit 2, not 1");
    assert!(
        stderr.contains("WARNING: the metadata index contains zero live files"),
        "the §7 #3 warning must still be on stderr: {stderr}"
    );
}

// ── search ───────────────────────────────────────────────────────────────────

#[test]
fn search_hit_exits_0_and_miss_exits_1() {
    let fx = Fixture::new("search", "a.txt", "needle\n");
    assert_eq!(fx.index(), 0);
    assert_eq!(fx.fulltext(), 0);
    assert_eq!(fx.search("needle"), 0, "a hit in a built index must exit 0");
    assert_eq!(
        fx.search("zzz-no-such-term"),
        1,
        "a usable index with zero hits must exit 1"
    );
}

#[test]
fn search_without_fulltext_index_exits_2() {
    let fx = Fixture::new("search-no-ft", "a.txt", "needle\n");
    assert_eq!(fx.index(), 0);
    // No `fulltext` was run: the index directory does not exist. With and
    // without the metadata DB the setup is unusable and must exit 2.
    let (code, _, stderr) = run(&[
        "search",
        "needle",
        "--no-db",
        "--index-dir",
        fx.ft.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "a missing full-text index must exit 2");
    assert!(
        stderr.contains("error:"),
        "the error must still be reported on stderr: {stderr}"
    );
    assert_eq!(fx.search("needle"), 2, "same with a DB present");
}

#[test]
fn search_on_empty_fulltext_index_exits_2() {
    // `total_docs == 0` is an unusable setup, not a legitimate "no match":
    // the old contract could not tell "indexed, found nothing" from "nothing
    // is indexed", which is exactly the confusion #49 exists to remove.
    let fx = Fixture::new("search-empty-ft", "a.txt", "needle\n");
    assert_eq!(fx.index(), 0);
    // A DB with zero live files builds a full-text index with zero documents.
    std::fs::remove_file(fx.root.join("a.txt")).unwrap();
    assert_eq!(fx.index(), 2);
    assert_eq!(fx.fulltext(), 2, "zero documents indexed is itself exit 2");
    let (code, _, stderr) = run(&[
        "search",
        "needle",
        "--db",
        fx.db.to_str().unwrap(),
        "--index-dir",
        fx.ft.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "an empty full-text index must exit 2");
    assert!(
        stderr.contains("WARNING: the full-text index is empty"),
        "the empty-index warning must still be on stderr: {stderr}"
    );
}

// ── index / hash / fulltext / tag / status (write and report side) ──────────

#[test]
fn index_with_files_exits_0_and_empty_dir_exits_2() {
    let fx = Fixture::new("index-files", "a.txt", "needle\n");
    assert_eq!(fx.index(), 0, "indexing a directory with files must exit 0");

    let empty = TempDir::new("index-empty");
    let db = empty.path().join("empty.db");
    let (code, _, stderr) = run(&["index", empty.path().to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert_eq!(code, 2, "indexing a directory with nothing to index must exit 2");
    assert!(
        stderr.contains("WARNING: zero files indexed"),
        "the zero-files warning must still be on stderr: {stderr}"
    );
}

#[test]
fn fulltext_with_zero_documents_exits_2() {
    let fx = Fixture::new("ft-zero", "a.txt", "needle\n");
    assert_eq!(fx.index(), 0);
    std::fs::remove_file(fx.root.join("a.txt")).unwrap();
    assert_eq!(fx.index(), 2);
    assert_eq!(fx.fulltext(), 2, "zero documents indexed must exit 2");
}

#[test]
fn tag_with_zero_live_files_exits_2() {
    let fx = Fixture::new("tag-zero", "a.txt", "needle\n");
    assert_eq!(fx.index(), 0);
    std::fs::remove_file(fx.root.join("a.txt")).unwrap();
    assert_eq!(fx.index(), 2);
    assert_eq!(fx.tag(), 2, "no live files to tag must exit 2");
}

#[test]
fn status_exits_0_and_anyhow_errors_exit_2() {
    let fx = Fixture::new("status", "a.txt", "needle\n");
    assert_eq!(fx.index(), 0);
    assert_eq!(
        run(&["status", "--db", fx.db.to_str().unwrap()]).0,
        0,
        "a report is always an answer"
    );
    // An `anyhow` error path: the crawl root does not exist.
    let (code, _, stderr) = run(&["index", "/no-such-root-xyz", "--db", fx.db.to_str().unwrap()]);
    assert_eq!(code, 2, "an anyhow error must exit 2");
    assert!(
        stderr.contains("error: root not found"),
        "the error must still be reported on stderr: {stderr}"
    );
}

// ── tags / browse ────────────────────────────────────────────────────────────

#[test]
fn tags_and_browse_without_tag_layer_exit_2() {
    let fx = Fixture::new("no-layer", "a.txt", "needle\n");
    assert_eq!(fx.index(), 0);
    assert_eq!(
        fx.tags(&[]),
        2,
        "no tag layer means there is no tree to browse: exit 2"
    );
    assert_eq!(
        fx.tags(&["ext:zzz"]),
        2,
        "a filter against an index without a tag layer is still exit 2"
    );
    assert_eq!(fx.browse(&[]), 2, "browse without a tag layer must exit 2");
}

#[test]
fn tags_and_browse_with_tag_layer() {
    let fx = Fixture::new("with-layer", "a.txt", "needle\n");
    assert_eq!(fx.index(), 0);
    assert_eq!(fx.tag(), 0, "building the tag layer over a live file must exit 0");
    assert_eq!(fx.tags(&[]), 0, "the namespace overview is non-empty");
    assert_eq!(fx.tags(&["ext:txt"]), 0, "a value that exists is non-empty");
    assert_eq!(fx.tags(&["ext:zzz"]), 1, "a value that does not exist is the empty answer");
    assert_eq!(fx.browse(&[]), 0, "the root view over a non-empty corpus");
    assert_eq!(fx.browse(&["ext:zzz"]), 1, "a selection matching no file is the empty answer");
}

// ── --json ───────────────────────────────────────────────────────────────────

#[test]
fn json_flag_does_not_change_exit_codes() {
    let fx = Fixture::new("json", "a.txt", "needle\n");
    assert_eq!(fx.index(), 0);
    assert_eq!(fx.fulltext(), 0);
    assert_eq!(fx.tag(), 0);

    let cases: &[(&str, Vec<&str>, i32)] = &[
        ("find", vec!["find", "a.txt"], 0),
        ("find miss", vec!["find", "zzz-no-such-file"], 1),
        ("search", vec!["search", "needle"], 0),
        ("search miss", vec!["search", "zzz-no-such-term"], 1),
        ("tags hit", vec!["tags", "ext:txt"], 0),
        ("tags miss", vec!["tags", "ext:zzz"], 1),
        ("browse", vec!["browse"], 0),
        ("browse miss", vec!["browse", "ext:zzz"], 1),
    ];
    for (label, args, expected) in cases {
        let plain = {
            let mut a = args.clone();
            a.push("--db");
            a.push(fx.db.to_str().unwrap());
            if a[0] == "search" {
                a.push("--index-dir");
                a.push(fx.ft.to_str().unwrap());
            }
            run(&a).0
        };
        let json = {
            let mut a = args.clone();
            a.push("--json");
            a.push("--db");
            a.push(fx.db.to_str().unwrap());
            if a[0] == "search" {
                a.push("--index-dir");
                a.push(fx.ft.to_str().unwrap());
            }
            run(&a).0
        };
        assert_eq!(plain, *expected, "{label}: human mode");
        assert_eq!(json, *expected, "{label}: --json must not change the exit code");
    }
}

// ── status --check-journal (issue #60, docs/cli.md §9-1) ────────────────────

#[test]
fn status_help_lists_the_journal_flags_on_every_platform() {
    // The flags must exist everywhere: a script that runs on Linux and
    // Windows would get a clap usage error (exit 2) on one of them otherwise.
    let (code, stdout, _) = run(&["status", "--help"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("--check-journal"),
        "the flag must be documented: {stdout}"
    );
    assert!(
        stdout.contains("--journal-warn-hours"),
        "the flag must be documented: {stdout}"
    );
}

#[test]
fn status_with_check_journal_on_an_mtime_marker_reports_not_checked_and_exits_0() {
    // The marker is pinned to mtime rather than assumed: since #58 `sagasu
    // index` writes a USN marker on Windows, so a test that assumed "this
    // fixture has an mtime marker" passed on Linux and failed on the Windows
    // CI runner — which is an administrator and therefore really can read the
    // journal.  `status` is a report: one unavailable line is not a failed
    // command, so the exit code stays 0 either way.
    let fx = Fixture::new("status-journal", "a.txt", "needle\n");
    let mtime = [("SAGASU_DELTA_SOURCE", "mtime")];
    assert_eq!(
        run_with_env(
            &["index", fx.root.to_str().unwrap(), "--db", fx.db.to_str().unwrap()],
            &mtime,
        )
        .0,
        0
    );

    let (code, stdout, _) = run_with_env(
        &[
            "status",
            "--db",
            fx.db.to_str().unwrap(),
            "--check-journal",
            "--journal-warn-hours",
            "6",
        ],
        &mtime,
    );
    assert_eq!(code, 0, "a report is always an answer");
    assert!(
        stdout.contains("not checked —"),
        "the human report must say the check did not run: {stdout}"
    );
    assert!(
        stdout.contains("mtime marker"),
        "the fixture's marker is an mtime marker and the reason must name it: {stdout}"
    );

    let (code, stdout, _) = run_with_env(
        &[
            "status",
            "--db",
            fx.db.to_str().unwrap(),
            "--check-journal",
            "--json",
        ],
        &mtime,
    );
    assert_eq!(code, 0);
    assert!(
        stdout.contains("\"checked\":false") && stdout.contains("\"reason\""),
        "the JSON must carry checked:false with a reason: {stdout}"
    );
}

#[test]
fn status_with_check_journal_on_the_platform_default_marker_is_always_an_answer() {
    // Whatever marker this platform writes, and whether or not the probe can
    // reach a journal, the report must come back: exit 0, and a `journal`
    // object that either says it did not run *and why*, or says it ran *and
    // carries the numbers*.  Never `checked: true` with nothing behind it.
    //
    // On the Windows CI runner this is the one test that reaches the real
    // `FSCTL_QUERY_USN_JOURNAL` (the runner is an administrator and, since
    // #58, `index` writes a USN marker there).  On Linux it takes the
    // not-checked branch.  Either way the assertion is about observable
    // output, not about which platform is compiling it.
    let fx = Fixture::new("status-journal-native", "a.txt", "needle\n");
    assert_eq!(fx.index(), 0);

    let (code, stdout, _) = run(&[
        "status",
        "--db",
        fx.db.to_str().unwrap(),
        "--check-journal",
        "--json",
    ]);
    assert_eq!(code, 0, "a report is always an answer");

    if stdout.contains("\"checked\":true") {
        for key in ["next_usn", "consumed_bytes", "rolled_off", "journal_matches"] {
            assert!(
                stdout.contains(&format!("\"{key}\"")),
                "a checked probe must carry {key}: {stdout}"
            );
        }
    } else {
        assert!(
            stdout.contains("\"checked\":false") && stdout.contains("\"reason\""),
            "an unchecked probe must say why: {stdout}"
        );
    }
}

#[test]
fn status_without_check_journal_keeps_the_not_requested_json_shape() {
    // The off shape is the documented contract (docs/cli.md §4-5).
    let fx = Fixture::new("status-no-flag", "a.txt", "needle\n");
    assert_eq!(fx.index(), 0);
    let (code, stdout, _) = run(&["status", "--db", fx.db.to_str().unwrap(), "--json"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("\"checked\":false") && stdout.contains("not requested (--check-journal)"),
        "without the flag the JSON must say not requested: {stdout}"
    );
}
