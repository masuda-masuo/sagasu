//! Parallel metadata crawl + rescan diff.
//!
//! The walker uses `ignore::WalkBuilder` with `build_parallel()` to traverse the
//! filesystem. By design it only collects `stat`-derived metadata — no file is
//! opened, so `blake3` and `magic` stay NULL after `index`.
//!
//! ## Built-in exclusion
//!
//! The following directory *names* (basename only, depth ≥ 1) are excluded by
//! default: `node_modules`, `target`, `__pycache__`, `.git`, `.hg`, `.svn`,
//! `.venv`, `venv`, `.cache`, `.npm`, `.cargo/registry`.  `--no-default-excludes`
//! drops this list; `--exclude <NAME>` adds to it.
//!
//! The list is **defined here, independently of any version-control rule**, and
//! that is the whole point of it (issue #14). The walker turns every filter the
//! `ignore` crate applies by default off — `hidden`, `ignore`, `git_ignore`,
//! `git_global`, `git_exclude` — because a gitignore rule answers "should this
//! be committed?" and a search engine is asking "should this be findable?".
//! Those are different questions, and a measurement that treated them as one
//! saw 494 files where the tree held 151,674. What is left after that is a list
//! of build outputs and caches: large, machine-generated, and reproducible from
//! something else that *is* indexed.
//!
//! Two further filters exist and are **opt-in**, both reported separately in
//! [`CrawlSummary`]:
//!
//! - [`HiddenPolicy::SkipOsHidden`] skips entries carrying the operating
//!   system's hidden flag. On Windows that is `FILE_ATTRIBUTE_HIDDEN`; on Unix
//!   there is no such attribute, and the leading dot is **not** treated as one
//!   (see [`is_os_hidden`]). `.github/`, `.config/`, `.vscode/` are content.
//! - A `.gitignore` read from the crawl root, applied to **directories only**
//!   (see [`ExcludeSet::with_gitignore`]).
//!
//! ## Rescan diff
//!
//! On rescan the store is not blindly upserted. Instead the walker compares each
//! encountered file against the existing index and classifies it as unchanged,
//! changed, renamed, or new.  Paths present in the index but absent from the
//! filesystem become tombstones.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Instant, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{WalkBuilder, WalkState};

use crate::store::{self, FileEntry, Store};

// ── Default excludes ────────────────────────────────────────────────────────

/// Directory basenames excluded by default (case-insensitive on the walk).
/// Note: `.cargo` is special-cased — we only skip `.cargo/registry`, not
/// `.cargo` itself or other children.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "node_modules",
    "target",
    "__pycache__",
    ".git",
    ".hg",
    ".svn",
    ".venv",
    "venv",
    ".cache",
    ".npm",
    ".cargo", // see special-case logic below
];

/// Name of the `meta` row an [`ExcludeSet`] is persisted under, so the
/// search-time delta path can rebuild the *same* set instead of guessing.
pub const EXCLUDE_POLICY_KEY: &str = "exclude_policy";

/// Filename read from the crawl root when [`ExcludeSet::with_gitignore`] is on.
pub const GITIGNORE_FILE: &str = ".gitignore";

// ── Hidden attribute ────────────────────────────────────────────────────────

/// `FILE_ATTRIBUTE_HIDDEN` (`winnt.h`).
pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;

/// Whether a Windows file-attribute word marks the entry hidden.
///
/// Split out from [`is_os_hidden`] so the rule itself is testable everywhere:
/// a Linux CI job cannot create a file with the hidden attribute, but it can
/// check that this is the bit we look at.
pub fn hidden_from_attributes(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_HIDDEN != 0
}

/// Whether the **operating system** marks this entry hidden.
///
/// This deliberately has nothing to do with a leading dot. `.github`,
/// `.config`, `.vscode` are ordinary directories that a naming convention asks
/// `ls` to keep out of the way; the files inside them are exactly the kind of
/// thing someone opens a search engine to find. Windows has a real per-entry
/// attribute and this reads it; Unix has none, so this is always `false` there
/// — including for dot-names (issue #14).
#[cfg(windows)]
pub fn is_os_hidden(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    hidden_from_attributes(meta.file_attributes())
}

/// Whether the operating system marks this entry hidden — always `false` on
/// Unix, which has no hidden attribute. See the Windows sibling for why the
/// leading dot is not consulted.
#[cfg(not(windows))]
pub fn is_os_hidden(_meta: &std::fs::Metadata) -> bool {
    false
}

/// [`is_os_hidden`] for a path. An entry we cannot stat is not called hidden:
/// dropping a file because its metadata was unavailable would be the silent
/// loss this module exists to prevent.
pub fn path_is_os_hidden(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| is_os_hidden(&m))
        .unwrap_or(false)
}

/// What the crawl does with entries the OS marks hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HiddenPolicy {
    /// Index them. The default, and the reason `.github/**` is findable.
    #[default]
    Include,
    /// Skip them (`--skip-hidden`). Only has an effect where the OS has a
    /// hidden attribute, i.e. Windows — see [`is_os_hidden`].
    SkipOsHidden,
}

impl HiddenPolicy {
    /// Stable label used in the persisted policy and in CLI output.
    pub fn as_str(self) -> &'static str {
        match self {
            HiddenPolicy::Include => "include",
            HiddenPolicy::SkipOsHidden => "skip-os-hidden",
        }
    }
}

/// Why a path is outside the indexed set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExcludeReason {
    /// Under a directory whose basename is on the exclusion list. Carries the
    /// lowercased basename, which is the key the per-name skip counts use.
    Name(String),
    /// The entry, or a directory above it, carries the OS hidden flag.
    OsHidden,
    /// A directory above it matched the crawl root's `.gitignore`.
    Gitignore,
}

/// The effective exclusion set of a crawl: the built-in name list (unless
/// dropped) plus whatever the user added, plus the two opt-in filters.
///
/// This is a type rather than a `Vec<String>` because the *same* set has to be
/// applied in two places that do not share a walker: the crawl below, and the
/// search-time delta set ([`crate::delta`]). A USN Journal read returns raw
/// volume-wide change records, so unless the very same rule is applied there,
/// the delta set fills up with build artefacts and telemetry the index never
/// contained — measured at a 94% noise ratio on a real machine (issue #16).
///
/// The opt-in filters make that sharing load-bearing in the other direction
/// too: a directory the crawl pruned but the delta scan still walks turns into
/// a live search hit for a file the index deliberately does not contain. So the
/// whole policy is [`ExcludeSet::encode`]d into `meta` by the crawl and
/// [`ExcludeSet::decode`]d back by everything that queries.
#[derive(Debug, Clone)]
pub struct ExcludeSet {
    names: Vec<String>,
    /// The user's additions, kept apart from the built-ins so the policy can be
    /// re-encoded exactly as it was requested.
    extra: Vec<String>,
    no_default: bool,
    hidden: HiddenPolicy,
    /// Compiled root `.gitignore`, when the crawl opted into reading one.
    /// `Arc` because [`ExcludeSet`] is cloned once per walker thread.
    gitignore: Option<Arc<Gitignore>>,
}

impl ExcludeSet {
    /// Build the set from the built-in list plus `extra`. `no_default` drops the
    /// built-in list, leaving only `extra`.
    pub fn new(extra: &[String], no_default: bool) -> Self {
        let mut names: Vec<String> = if no_default {
            Vec::new()
        } else {
            DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect()
        };
        for e in extra {
            if !names.iter().any(|n| n.eq_ignore_ascii_case(e)) {
                names.push(e.clone());
            }
        }
        Self {
            names,
            extra: extra.to_vec(),
            no_default,
            hidden: HiddenPolicy::default(),
            gitignore: None,
        }
    }

    /// Set the hidden-entry policy (builder style).
    pub fn with_hidden(mut self, hidden: HiddenPolicy) -> Self {
        self.hidden = hidden;
        self
    }

    /// Read `root/.gitignore` and apply it — **to directories only**.
    ///
    /// The narrowing is the design decision, not an implementation shortcut
    /// (issue #14). A gitignore entry for a *directory* nearly always means
    /// "everything below here is generated", which is the same statement the
    /// built-in list makes and is worth picking up automatically for the
    /// project-specific cases it cannot know (`dist/`, `out/`, `.next/`).
    /// A gitignore entry for a *file* means one of three things — generated,
    /// secret, or local-only — and the last two (`.env`, `notes.local.md`,
    /// `TODO.md`) are precisely what someone searches their own disk for. So
    /// file rules are read and then not applied.
    ///
    /// Only the root file is read; nested `.gitignore`s in subprojects are not.
    /// A missing file is not an error — it means "no rules".
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or compiled. It
    /// is not swallowed: a user who asked for gitignore support and silently
    /// did not get it would be counting a different corpus than they think.
    pub fn with_gitignore(mut self, root: &Path, enabled: bool) -> Result<Self> {
        if !enabled {
            self.gitignore = None;
            return Ok(self);
        }
        let file = root.join(GITIGNORE_FILE);
        let mut builder = GitignoreBuilder::new(root);
        if file.exists() {
            if let Some(err) = builder.add(&file) {
                bail!("failed to read {}: {err}", file.display());
            }
        }
        let compiled = builder
            .build()
            .with_context(|| format!("failed to compile {}", file.display()))?;
        self.gitignore = Some(Arc::new(compiled));
        Ok(self)
    }

    /// The excluded directory basenames, in effective order.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The hidden-entry policy in force.
    pub fn hidden_policy(&self) -> HiddenPolicy {
        self.hidden
    }

    /// Whether a root `.gitignore` is being applied.
    pub fn uses_gitignore(&self) -> bool {
        self.gitignore.is_some()
    }

    /// Number of gitignore rules in force (0 when the file was absent or empty).
    pub fn gitignore_rules(&self) -> usize {
        self.gitignore.as_ref().map(|g| g.len()).unwrap_or(0)
    }

    /// Whether a directory basename is excluded (case-insensitive).
    pub fn contains(&self, name: &str) -> bool {
        self.names.iter().any(|e| e.eq_ignore_ascii_case(name))
    }

    /// If `path` lives under an excluded directory at or below `root`, return
    /// that directory's lowercased basename (the key used in skip counts).
    ///
    /// `.cargo` is special-cased: only `.cargo/registry` is skipped, not
    /// `.cargo` itself or its other children.
    pub fn matched_dir(&self, path: &Path, root: &Path) -> Option<String> {
        for ancestor in path.ancestors().skip(1) {
            if ancestor == root {
                break;
            }
            let Some(name) = ancestor.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let hit = if name.eq_ignore_ascii_case(".cargo") {
                self.contains(name) && path.starts_with(ancestor.join("registry"))
            } else {
                self.contains(name)
            };
            if hit {
                return Some(name.to_lowercase());
            }
        }
        None
    }

    /// The reason `path` (a file at or below `root`) is out of the indexed set,
    /// or `None` when it belongs in it.
    ///
    /// `is_hidden` answers the hidden question for **one** path; walking the
    /// ancestors is done here. It is injected because the crawl can memoize the
    /// answer per directory across the thousands of files inside it, while a
    /// delta query — bounded by its limit — has nothing worth memoizing.
    ///
    /// The order of the checks is the order of their cost: names are string
    /// comparisons, gitignore is glob matching, hidden is a `stat` per
    /// directory. It is also the order the reasons are worth reporting in.
    pub fn reason_for(
        &self,
        path: &Path,
        root: &Path,
        is_hidden: &mut dyn FnMut(&Path) -> bool,
    ) -> Option<ExcludeReason> {
        if let Some(name) = self.matched_dir(path, root) {
            return Some(ExcludeReason::Name(name));
        }
        if let Some(gi) = &self.gitignore {
            for ancestor in path.ancestors().skip(1) {
                if ancestor == root {
                    break;
                }
                if gi.matched(ancestor, true).is_ignore() {
                    return Some(ExcludeReason::Gitignore);
                }
            }
        }
        if self.hidden == HiddenPolicy::SkipOsHidden {
            if is_hidden(path) {
                return Some(ExcludeReason::OsHidden);
            }
            for ancestor in path.ancestors().skip(1) {
                if ancestor == root {
                    break;
                }
                if is_hidden(ancestor) {
                    return Some(ExcludeReason::OsHidden);
                }
            }
        }
        None
    }

    /// The reason `path` is out of scope, stat'ing directly for the hidden
    /// check. The convenience form of [`ExcludeSet::reason_for`] for callers
    /// with nothing to memoize.
    pub fn reason_for_path(&self, path: &Path, root: &Path) -> Option<ExcludeReason> {
        self.reason_for(path, root, &mut path_is_os_hidden)
    }

    /// Serialize the policy for the `meta` table.
    ///
    /// Line-oriented rather than delimiter-packed because a directory basename
    /// may contain almost any byte; a newline is the one thing that is illegal
    /// in a path component on Windows and vanishingly rare on Unix. The root
    /// `.gitignore` is referenced, not copied — [`ExcludeSet::decode`] re-reads
    /// it, so an edited file takes effect at the next query rather than being
    /// frozen at crawl time.
    pub fn encode(&self) -> String {
        let mut out = String::from("v1\n");
        out.push_str(&format!("defaults={}\n", u8::from(!self.no_default)));
        out.push_str(&format!("hidden={}\n", self.hidden.as_str()));
        out.push_str(&format!("gitignore={}\n", u8::from(self.uses_gitignore())));
        for name in &self.extra {
            out.push_str("extra=");
            out.push_str(name);
            out.push('\n');
        }
        out
    }

    /// Rebuild a policy written by [`ExcludeSet::encode`].
    ///
    /// An unknown version or a malformed line is an error rather than a
    /// best-effort parse: falling back to the defaults would quietly give the
    /// delta path a *different* exclusion set than the crawl used, which is the
    /// exact inconsistency the encoding exists to remove.
    pub fn decode(text: &str, root: &Path) -> Result<Self> {
        let mut lines = text.lines();
        match lines.next() {
            Some("v1") => {}
            other => bail!("unknown exclusion policy format: {other:?}"),
        }

        let mut no_default = false;
        let mut hidden = HiddenPolicy::Include;
        let mut gitignore = false;
        let mut extra: Vec<String> = Vec::new();

        for line in lines {
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                bail!("malformed exclusion policy line: {line:?}");
            };
            match key {
                "defaults" => no_default = value == "0",
                "gitignore" => gitignore = value == "1",
                "hidden" => {
                    hidden = match value {
                        "include" => HiddenPolicy::Include,
                        "skip-os-hidden" => HiddenPolicy::SkipOsHidden,
                        other => bail!("unknown hidden policy: {other:?}"),
                    }
                }
                "extra" => extra.push(value.to_string()),
                other => bail!("unknown exclusion policy key: {other:?}"),
            }
        }

        Self::new(&extra, no_default)
            .with_hidden(hidden)
            .with_gitignore(root, gitignore)
    }
}

/// Resolve a database path to its canonical (absolute) form, tolerating a file
/// that does not exist yet: the parent directory is canonicalized and the file
/// name appended.
pub fn canonical_db_path(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    match abs.parent() {
        Some(parent) => match parent.canonicalize() {
            Ok(cp) => cp.join(abs.file_name().unwrap_or_default()),
            Err(_) => abs,
        },
        None => abs,
    }
}

/// The database file and its WAL/SHM siblings, canonicalized.
///
/// These must never be indexed, and must never enter a search-time delta set
/// either: SQLite writes to them on every scan, so they would look like a
/// changed file on every single query.
pub fn db_sibling_paths(db_path: &Path) -> Vec<PathBuf> {
    let db_canon = canonical_db_path(db_path);
    vec![
        db_canon.clone(),
        PathBuf::from(format!("{}-wal", db_canon.display())),
        PathBuf::from(format!("{}-shm", db_canon.display())),
    ]
}

/// Path equality that tolerates case differences (Windows filesystems).
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
}

// ── Config / Summary ────────────────────────────────────────────────────────

/// Configuration for a metadata crawl.
#[derive(Debug, Clone)]
pub struct CrawlConfig {
    /// Root directory to crawl.
    pub root: PathBuf,
    /// Path to the SQLite database.
    pub db_path: PathBuf,
    /// Additional directory basenames to exclude (appended to built-in list).
    pub exclude: Vec<String>,
    /// If true, drop the built-in exclusion list.
    pub no_default_excludes: bool,
    /// What to do with entries the OS marks hidden. The default indexes them —
    /// see [`HiddenPolicy`].
    pub hidden: HiddenPolicy,
    /// Read `root/.gitignore` and apply its **directory** rules on top of the
    /// exclusion list. Off by default — see [`ExcludeSet::with_gitignore`].
    pub use_gitignore: bool,
    /// Number of walker threads (0 = auto).
    pub threads: usize,
}

impl CrawlConfig {
    /// Config for `root` and `db_path` with every documented default.
    pub fn new(root: impl Into<PathBuf>, db_path: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            db_path: db_path.into(),
            exclude: Vec::new(),
            no_default_excludes: false,
            hidden: HiddenPolicy::default(),
            use_gitignore: false,
            threads: 0,
        }
    }
}

/// Summary of a crawl operation.
#[derive(Debug, Clone, Default)]
pub struct CrawlSummary {
    /// Total files examined by the walker (before filtering).
    pub scanned: u64,
    /// Files that were successfully indexed (new + unchanged + changed + renamed).
    pub indexed: u64,
    /// Per-exclusion-name skip counts (lowercased basename → count).
    pub skipped: HashMap<String, u64>,
    /// Files skipped because they, or a directory above them, carry the OS
    /// hidden flag. Always 0 unless [`HiddenPolicy::SkipOsHidden`] is in force.
    /// Counted apart from [`CrawlSummary::skipped`] because "a rule you named"
    /// and "an attribute the filesystem carries" are different things to have
    /// to undo.
    pub skipped_hidden: u64,
    /// Files skipped because a directory above them matched the root
    /// `.gitignore`. Always 0 unless the crawl opted into reading one.
    pub skipped_gitignore: u64,
    /// Files newly inserted.
    pub added: u64,
    /// Files whose metadata changed in place.
    pub changed: u64,
    /// Files detected as renames/moves (same fs_id or size+mtime).
    pub renamed: u64,
    /// Files that were in the index but absent on disk (tombstoned).
    pub deleted: u64,
    /// Wall-clock duration of the crawl.
    pub elapsed_secs: f64,
}

impl CrawlSummary {
    /// Every file the walker saw and then dropped, across all three reasons.
    /// `scanned - skipped_total()` is what reached the store.
    pub fn skipped_total(&self) -> u64 {
        self.skipped.values().sum::<u64>() + self.skipped_hidden + self.skipped_gitignore
    }
}

// ── Hash summary ────────────────────────────────────────────────────────────

/// Summary of a hash backfill pass.
#[derive(Debug, Clone, Default)]
pub struct HashSummary {
    /// Files successfully hashed.
    pub hashed: u64,
    /// Files skipped because they exceeded `--max-size`.
    pub skipped_too_large: u64,
    /// Files skipped because they were unreadable at hash time.
    pub skipped_unreadable: u64,
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Run a metadata crawl of `config.root` and persist results into the SQLite
/// store at `config.db_path`.
///
/// On the *first* scan every file is "added". On subsequent scans the diff
/// classifies each entry as unchanged / changed / renamed / new, and marks
/// missing files as deleted (tombstoned).
///
/// # Errors
///
/// Returns an error if the database cannot be opened or the root does not exist.
/// If zero files are indexed the function still returns `Ok(summary)` — the
/// caller is responsible for checking `summary.indexed == 0` and issuing a
/// warning / non-zero exit.
pub fn crawl(config: CrawlConfig) -> Result<CrawlSummary> {
    let root = config
        .root
        .canonicalize()
        .with_context(|| format!("root not found: {}", config.root.display()))?;

    let store = Store::open(&config.db_path)?;
    store.ensure_schema_version()?;

    // Scan marker (unix ns when the scan started).
    let scan_marker_ns = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);

    // Build the effective exclude list. A `.gitignore` that cannot be read is a
    // hard error here: it was asked for explicitly, and a crawl that quietly
    // ignored the request would report a file count for a different corpus.
    let excludes = ExcludeSet::new(&config.exclude, config.no_default_excludes)
        .with_hidden(config.hidden)
        .with_gitignore(&root, config.use_gitignore)?;

    // The database file (and its WAL/SHM siblings) must never be indexed. If the
    // user pointed `--db` inside the crawl root, skip those paths explicitly.
    let db_skip = db_sibling_paths(&config.db_path);

    // Point-in-time marker for the search-time delta merge (design.md §5).
    // Taken *before* the walk: anything that changes while the crawl is running
    // falls on the delta side of the next search, which is the safe direction.
    // On Windows this is a USN Journal position when one is available, and a
    // wall-clock instant otherwise; the source that produced it also knows how
    // to read a range back from it.
    let delta_marker = crate::delta::source_for(&crate::delta::DeltaConfig {
        root: root.clone(),
        excludes: excludes.clone(),
        skip_paths: db_skip.clone(),
        threads: config.threads,
    })
    .current_marker()
    .unwrap_or(crate::delta::ScanMarker::Mtime {
        started_ns: scan_marker_ns,
    });

    // ── Walk in parallel, collect into a channel ────────────────────────────
    let (tx, rx) = mpsc::channel::<FileEntry>();
    let skip_map = Arc::new(Mutex::new(HashMap::<String, u64>::new()));
    let skipped_hidden = Arc::new(AtomicU64::new(0));
    let skipped_gitignore = Arc::new(AtomicU64::new(0));
    let scanned = Arc::new(AtomicU64::new(0));

    let threads = if config.threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    } else {
        config.threads
    };

    // Clone Arcs before spawning so the main thread retains its references.
    let skip_map_ref = skip_map.clone();
    let hidden_ref = skipped_hidden.clone();
    let gitignore_ref = skipped_gitignore.clone();
    let scanned_ref = scanned.clone();
    let root_ref = root.clone();
    let db_skip_ref = db_skip.clone();

    let mut builder = WalkBuilder::new(&root);
    builder
        .threads(threads)
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false);

    let walker = builder.build_parallel();
    let t0 = Instant::now();

    // ── Transaction: meta writes AND file diff commit atomically ────────────
    // Begin the transaction FIRST so that root_path / scan_marker /
    // scan_generation are only persisted when the crawl completes successfully.
    // A failed crawl rolls the whole thing back: no phantom generation bump,
    // no misleading marker age in `sagasu status`.
    let tx_db = store.begin_tx()?;
    Store::meta_set_tx(&tx_db, "root_path", &root.to_string_lossy())?;
    Store::meta_set_tx(&tx_db, "scan_marker", &scan_marker_ns.to_string())?;
    Store::meta_set_tx(&tx_db, "delta_marker", &delta_marker.encode())?;
    // Persist the exclusion policy alongside the marker. Everything that queries
    // this index replays it (design.md §5-1) — without it the delta scan walks
    // directories the crawl pruned and answers with files the index has no row
    // for, which is the same class of failure as losing files, pointed the other
    // way.
    Store::meta_set_tx(&tx_db, EXCLUDE_POLICY_KEY, &excludes.encode())?;
    let scan_gen = Store::next_scan_generation_tx(&tx_db)?;

    // Capture the summary from within the scope via Arc<Mutex>.
    let result = Arc::new(Mutex::new(None::<Result<CrawlSummary>>));
    let result_ref = result.clone();

    let tx_walk = tx.clone();
    let excludes_walk = excludes.clone();

    std::thread::scope(|s| {
        s.spawn(move || {
            walker.run(|| {
                let tx = tx_walk.clone();
                let excludes = excludes_walk.clone();
                let skip_map = skip_map_ref.clone();
                let hidden_count = hidden_ref.clone();
                let gitignore_count = gitignore_ref.clone();
                let scanned = scanned_ref.clone();
                let root = root_ref.clone();
                let db_skip = db_skip_ref.clone();

                // Per-thread memo for the hidden attribute: a directory's answer
                // is asked once per file inside it, and each answer is a `stat`.
                // Only populated when the policy actually looks.
                let mut hidden_memo: HashMap<PathBuf, bool> = HashMap::new();

                Box::new(move |entry| {
                    let Ok(entry) = entry else {
                        return WalkState::Continue;
                    };

                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        return WalkState::Continue;
                    }

                    // Never index our own database / WAL / SHM files.
                    if db_skip.iter().any(|p| same_path(p, entry.path())) {
                        return WalkState::Continue;
                    }

                    scanned.fetch_add(1, Ordering::Relaxed);

                    // Check the entry and its parent directories against the
                    // exclusion policy. Every drop lands in a counter: an
                    // exclusion nobody can see is indistinguishable from a bug.
                    let mut is_hidden = |p: &Path| -> bool {
                        if let Some(&known) = hidden_memo.get(p) {
                            return known;
                        }
                        let answer = path_is_os_hidden(p);
                        hidden_memo.insert(p.to_path_buf(), answer);
                        answer
                    };
                    match excludes.reason_for(entry.path(), &root, &mut is_hidden) {
                        Some(ExcludeReason::Name(name)) => {
                            let mut sm = skip_map.lock().unwrap();
                            *sm.entry(name).or_insert(0) += 1;
                            return WalkState::Skip;
                        }
                        Some(ExcludeReason::OsHidden) => {
                            hidden_count.fetch_add(1, Ordering::Relaxed);
                            return WalkState::Skip;
                        }
                        Some(ExcludeReason::Gitignore) => {
                            gitignore_count.fetch_add(1, Ordering::Relaxed);
                            return WalkState::Skip;
                        }
                        None => {}
                    }

                    let Ok(meta) = entry.metadata() else {
                        return WalkState::Continue;
                    };

                    let size = meta.len() as i64;
                    let mtime_ns = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);
                    let ctime_ns = meta
                        .created()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);

                    let path = entry.path();
                    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase());
                    let fs_id = store::fs_id_from_metadata(&meta);

                    let _ = tx.send(FileEntry {
                        path: path.to_string_lossy().into_owned(),
                        size,
                        mtime_ns,
                        ctime_ns,
                        ext,
                        fs_id,
                        scan_gen,
                    });

                    WalkState::Continue
                })
            });
            // tx dropped → channel closes.
        });

        // Drop our copy so the channel closes when the walker thread exits.
        drop(tx);

        // ── Main thread: apply to store with rescan diff ────────────────────
        let outcome = (|| -> Result<CrawlSummary> {
            // Snapshot existing live-file mappings: path → file_id.
            let mut existing_paths: HashMap<String, i64> = HashMap::new();
            for (id, p) in store.get_live_paths(&tx_db)? {
                existing_paths.insert(p, id);
            }

            // Track which existing file_ids have been claimed this scan.
            let mut seen_ids: HashMap<i64, bool> = HashMap::new();
            for &id in existing_paths.values() {
                seen_ids.insert(id, false);
            }

            let mut added: u64 = 0;
            let mut changed: u64 = 0;
            let mut renamed: u64 = 0;
            let mut indexed: u64 = 0;

            for entry in rx {
                let existing = store.find_by_path(&tx_db, &entry.path)?;

                if let Some(row) = existing {
                    let unchanged = row.size == entry.size && row.mtime_ns == entry.mtime_ns;
                    seen_ids.insert(row.file_id, true);

                    if unchanged {
                        store.touch_file(&tx_db, row.file_id, entry.scan_gen)?;
                    } else {
                        store.update_file_changed(&tx_db, row.file_id, &entry)?;
                        changed += 1;
                    }
                } else {
                    // Try rename/move detection.
                    let matched = if let Some(ref fs_id) = entry.fs_id {
                        store.find_by_fs_id(&tx_db, fs_id)?
                    } else {
                        None
                    };

                    let matched = match matched {
                        Some(ref row) if !seen_ids.get(&row.file_id).copied().unwrap_or(true) => {
                            Some(row.clone())
                        }
                        _ => {
                            let candidate =
                                store.find_by_size_mtime(&tx_db, entry.size, entry.mtime_ns)?;
                            match candidate {
                                Some(ref row)
                                    if !seen_ids.get(&row.file_id).copied().unwrap_or(true) =>
                                {
                                    Some(row.clone())
                                }
                                _ => None,
                            }
                        }
                    };

                    if let Some(row) = matched {
                        if seen_ids.get(&row.file_id).copied().unwrap_or(false) {
                            store.insert_file(&tx_db, &entry)?;
                            added += 1;
                        } else {
                            seen_ids.insert(row.file_id, true);
                            if row.size == entry.size && row.mtime_ns == entry.mtime_ns {
                                // Pure rename/move: content unchanged.
                                store.update_file_renamed(
                                    &tx_db,
                                    row.file_id,
                                    &entry.path,
                                    entry.scan_gen,
                                )?;
                                renamed += 1;
                            } else {
                                // Renamed AND modified: update all metadata and
                                // null blake3/magic (stale content hash) so
                                // `sagasu hash` recomputes it.
                                store.update_file_changed(&tx_db, row.file_id, &entry)?;
                                changed += 1;
                            }
                        }
                    } else {
                        store.insert_file(&tx_db, &entry)?;
                        added += 1;
                    }
                }

                indexed += 1;
            }

            let deleted = store.tombstone_unseen(&tx_db, scan_gen)?;

            tx_db.commit()?;

            let elapsed_secs = t0.elapsed().as_secs_f64();
            store.wal_checkpoint()?;

            let skipped = skip_map.lock().unwrap().clone();
            let scanned_total = scanned.load(Ordering::Relaxed);

            Ok(CrawlSummary {
                scanned: scanned_total,
                indexed,
                skipped,
                skipped_hidden: skipped_hidden.load(Ordering::Relaxed),
                skipped_gitignore: skipped_gitignore.load(Ordering::Relaxed),
                added,
                changed,
                renamed,
                deleted,
                elapsed_secs,
            })
        })();

        *result_ref.lock().unwrap() = Some(outcome);
    });

    // Extract the result from the Arc before local variables are dropped.
    let mut guard = result.lock().unwrap();
    guard
        .take()
        .context("walk: internal error — summary not captured")?
}

// ── Hash backfill ───────────────────────────────────────────────────────────

/// Backfill `blake3` and `magic` for live files that have NULL hashes.
///
/// Files larger than `max_size` are skipped (they keep NULL).  The function
/// returns a summary of how many files were hashed and how many were skipped.
pub fn hash_backfill(db_path: &Path, max_size: u64) -> Result<HashSummary> {
    const BATCH: i64 = 1000;

    let store = Store::open(db_path)?;
    let mut summary = HashSummary::default();
    let mut cursor: i64 = 0;

    loop {
        let files = store.get_unhashed_files_batch(cursor, BATCH)?;
        let Some(last) = files.last() else {
            break;
        };
        cursor = last.file_id;

        for row in &files {
            if row.size as u64 > max_size {
                summary.skipped_too_large += 1;
                continue;
            }

            let contents = match std::fs::read(&row.path) {
                Ok(c) => c,
                Err(_) => {
                    summary.skipped_unreadable += 1;
                    continue;
                }
            };

            let blake3_hash = blake3::hash(&contents);
            let magic_len = store::MAGIC_LEN.min(contents.len());
            let magic = &contents[..magic_len];

            store.update_hash(row.file_id, blake3_hash.as_bytes(), magic)?;
            summary.hashed += 1;
        }
    }

    store.wal_checkpoint()?;

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hidden_bit_is_the_one_we_look_at() {
        // FILE_ATTRIBUTE_HIDDEN, alone and among neighbours.
        assert!(hidden_from_attributes(FILE_ATTRIBUTE_HIDDEN));
        assert!(hidden_from_attributes(0x0000_0022)); // HIDDEN | DIRECTORY
                                                      // READONLY (0x1), SYSTEM (0x4), ARCHIVE (0x20), DIRECTORY (0x10):
                                                      // none of them means hidden, and SYSTEM in particular sits next door.
        assert!(!hidden_from_attributes(0x0000_0035));
        assert!(!hidden_from_attributes(0));
    }

    #[test]
    fn a_leading_dot_is_never_a_hidden_attribute() {
        // The whole of issue #14 in one assertion: on Unix nothing is hidden,
        // and on Windows the answer comes from the attribute word, so a
        // dot-named file is only hidden if someone actually set the bit. This
        // runs on both platforms and must hold on both.
        let dir = std::env::temp_dir().join(format!("sagasu_dot_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".github")).unwrap();
        let file = dir.join(".github").join("ci.yml");
        std::fs::write(&file, "on: push\n").unwrap();

        assert!(!path_is_os_hidden(&file));
        assert!(!path_is_os_hidden(&dir.join(".github")));

        let set = ExcludeSet::new(&[], false).with_hidden(HiddenPolicy::SkipOsHidden);
        assert_eq!(set.reason_for_path(&file, &dir), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_path_is_not_reported_as_hidden() {
        // "Could not stat" must not become "excluded": that is the silent loss.
        assert!(!path_is_os_hidden(Path::new("/nonexistent/sagasu/hidden")));
    }

    #[test]
    fn the_default_set_covers_build_output_but_not_dot_directories() {
        let set = ExcludeSet::new(&[], false);
        let root = Path::new("/root");
        assert!(matches!(
            set.reason_for_path(&root.join("node_modules/pkg/a.js"), root),
            Some(ExcludeReason::Name(_))
        ));
        assert!(matches!(
            set.reason_for_path(&root.join("target/debug/x.rs"), root),
            Some(ExcludeReason::Name(_))
        ));
        for dot in [
            ".github/workflows/ci.yml",
            ".config/app.toml",
            ".vscode/s.json",
        ] {
            assert_eq!(
                set.reason_for_path(&root.join(dot), root),
                None,
                "{dot} must stay in scope"
            );
        }
    }

    #[test]
    fn the_policy_round_trips_through_its_encoding() {
        let dir = std::env::temp_dir().join(format!("sagasu_policy_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(GITIGNORE_FILE), "build/\n*.log\n").unwrap();

        let original = ExcludeSet::new(&["notes".to_string(), "tmp".to_string()], false)
            .with_hidden(HiddenPolicy::SkipOsHidden)
            .with_gitignore(&dir, true)
            .unwrap();
        let decoded = ExcludeSet::decode(&original.encode(), &dir).unwrap();

        assert_eq!(decoded.names(), original.names());
        assert_eq!(decoded.hidden_policy(), HiddenPolicy::SkipOsHidden);
        assert!(decoded.uses_gitignore());
        assert_eq!(decoded.gitignore_rules(), original.gitignore_rules());
        assert_eq!(decoded.encode(), original.encode());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_policy_we_cannot_read_is_an_error_not_a_shrug() {
        // Falling back to the defaults would give the delta path a different
        // exclusion set than the crawl used, without saying so.
        let root = Path::new("/root");
        assert!(ExcludeSet::decode("v2\n", root).is_err());
        assert!(ExcludeSet::decode("v1\nhidden=maybe\n", root).is_err());
        assert!(ExcludeSet::decode("v1\nnonsense\n", root).is_err());
        assert!(ExcludeSet::decode("v1\nfuture_key=1\n", root).is_err());
    }

    #[test]
    fn gitignore_applies_to_directories_and_not_to_files() {
        let dir = std::env::temp_dir().join(format!("sagasu_gi_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(GITIGNORE_FILE), "dist/\n*.log\n.env\n").unwrap();

        let set = ExcludeSet::new(&[], false)
            .with_gitignore(&dir, true)
            .unwrap();

        // A generated directory: excluded.
        assert_eq!(
            set.reason_for_path(&dir.join("dist/app.js"), &dir),
            Some(ExcludeReason::Gitignore)
        );
        // Files a gitignore names are still indexed — "do not commit" is not
        // "do not find", and `.env` / `*.log` are things people search for.
        assert_eq!(set.reason_for_path(&dir.join("app.log"), &dir), None);
        assert_eq!(set.reason_for_path(&dir.join(".env"), &dir), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_gitignore_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("sagasu_nogi_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let set = ExcludeSet::new(&[], false)
            .with_gitignore(&dir, true)
            .unwrap();
        assert!(set.uses_gitignore());
        assert_eq!(set.gitignore_rules(), 0);
        assert_eq!(set.reason_for_path(&dir.join("a.txt"), &dir), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
