//! Measurement harness — runs external commands and records wall-clock times.
//!
//! ## Philosophy
//!
//! * **The harness is a stopwatch, not a profiler.**  It spawns a process and
//!   measures wall-clock time from spawn to exit.  Timing includes process
//!   startup, dynamic linking, and kernel scheduling — this is intentional.
//! * **Targets are declared entirely in TOML.**  Every measurable command is
//!   defined in the config file; the harness never knows about any specific
//!   prototype.
//! * **Cold = trial 0, warm = trials 1..N.**  No attempt is made to drop OS
//!   caches between trials.  The first trial runs with whatever state the OS
//!   has; subsequent trials may benefit from cached data.
//! * **A non-zero exit is a recorded failure, not a silent skip.**

use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Embedded default configs
// ---------------------------------------------------------------------------

/// Default config shipped with the release binary (Windows).
#[cfg(target_os = "windows")]
pub const EMBEDDED_CONFIG: &str = include_str!("../configs/prototypes-windows.toml");

/// Default config shipped with the release binary (non-Windows).
#[cfg(not(target_os = "windows"))]
pub const EMBEDDED_CONFIG: &str = include_str!("../configs/prototypes-linux.toml");

/// Name used when the embedded config is the source (not suitable for I/O — informational only).
pub const EMBEDDED_CONFIG_NAME: &str = "<embedded default config>";

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

/// Top-level configuration parsed from a TOML file.
#[derive(Debug, Deserialize)]
pub struct Config {
    pub target: Vec<TargetDef>,
}

/// A single measurable target defined in the config.
#[derive(Debug, Deserialize)]
pub struct TargetDef {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// Number of repeated measurements (including the first, which is "cold").
    pub repeat: u64,
    /// Optional setup command that runs once before the trials (not timed).
    /// When present, the work directory is NOT cleaned between trials.
    #[serde(default)]
    pub setup: Option<SetupDef>,
}

/// A setup command that runs once before the timed trials of a target.
/// Must exit zero for the target to proceed; a non-zero exit aborts the target.
#[derive(Debug, Deserialize)]
pub struct SetupDef {
    pub command: String,
    pub args: Vec<String>,
}

// ---------------------------------------------------------------------------
// Results types
// ---------------------------------------------------------------------------

/// Complete results for a run, serialised as JSON.
#[derive(Debug, Serialize)]
pub struct RunResults {
    pub harness_version: String,
    pub timestamp: String,
    pub environment: Environment,
    pub tree: TreeInfo,
    pub targets: Vec<TargetResult>,
}

#[derive(Debug, Serialize)]
pub struct Environment {
    pub os: String,
    pub arch: String,
    pub cpus: u64,
    /// Total physical memory in bytes.  `null` in JSON / "unknown" in footer
    /// when the value could not be determined.
    pub memory_total_bytes: Option<u64>,
    pub memory_total_human: String,
}

#[derive(Debug, Serialize)]
pub struct TreeInfo {
    pub path: String,
    pub files: u64,
    pub total_bytes: u64,
    pub manifest_path: String,
}

#[derive(Debug, Serialize)]
pub struct TargetResult {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub resolved: Vec<String>,
    pub cold: TrialStats,
    pub warm: TrialStats,
    pub trials: Vec<TrialRecord>,
    /// Whether a setup command was defined for this target.
    pub setup_ran: bool,
    /// Whether the setup command exited successfully (always true if setup_ran is false).
    pub setup_succeeded: bool,
}

#[derive(Debug, Serialize)]
pub struct TrialStats {
    pub trials: u64,
    /// Minimum wall-clock time (None if no non-failed trials).
    pub min_secs: Option<f64>,
    /// Maximum wall-clock time (None if no non-failed trials).
    pub max_secs: Option<f64>,
    /// 50th percentile wall-clock time (None if no non-failed trials).
    pub p50_secs: Option<f64>,
    /// 95th percentile wall-clock time (None if no non-failed trials).
    pub p95_secs: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct TrialRecord {
    pub index: u64,
    pub cold: bool,
    pub elapsed_secs: f64,
    pub exit_code: i32,
    pub failure: bool,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Load and validate a TOML config file.
pub fn load_config(path: &Path) -> Result<Config, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read config '{}': {}", path.display(), e))?;
    parse_config_text(&text).map_err(|e| {
        format!(
            "failed to parse config '{}': {}",
            path.display(),
            e
        )
        .into()
    })
}

/// Parse and validate TOML config text, stripping a leading UTF-8 BOM if present.
pub fn parse_config_text(text: &str) -> Result<Config, Box<dyn std::error::Error>> {
    // Strip UTF-8 BOM if present (bytes EF BB BF → U+FEFF)
    let text = text.trim_start_matches('\u{feff}');
    let cfg: Config = toml::from_str(text)?;
    if cfg.target.is_empty() {
        return Err("config must define at least one [[target]]".into());
    }
    for (i, t) in cfg.target.iter().enumerate() {
        if t.name.is_empty() {
            return Err(format!("target[{}] has an empty name", i).into());
        }
        if t.command.is_empty() {
            return Err(format!("target[{}] has an empty command", i).into());
        }
        if t.repeat == 0 {
            return Err(format!("target '{}' has repeat=0", t.name).into());
        }
        if let Some(ref setup) = t.setup {
            if setup.command.is_empty() {
                return Err(format!("target '{}' has an empty setup.command", t.name).into());
            }
        }
    }
    Ok(cfg)
}

/// Run all targets defined in `config` and return measurements.
///
/// * `config`    – parsed TOML configuration.
/// * `root`      – path to the generated tree root (substituted for `{root}`).
/// * `out_path`  – path for the JSON results file (not used for internal logic).
/// * `manifest`  – optional reference to the tree manifest (embedded in output).
pub fn run_benchmarks(
    config: &Config,
    root: &Path,
    manifest_info: Option<(u64, u64)>, // (files, total_bytes)
) -> Result<RunResults, Box<dyn std::error::Error>> {
    let root_str = root.to_string_lossy().into_owned();
    let version = env!("CARGO_PKG_VERSION").to_string();
    let timestamp = iso_timestamp();
    let env_info = collect_environment();

    let tree_info = TreeInfo {
        path: root_str.clone(),
        files: manifest_info.map(|(f, _)| f).unwrap_or(0),
        total_bytes: manifest_info.map(|(_, b)| b).unwrap_or(0),
        manifest_path: root
            .join(".bench-manifest.json")
            .to_string_lossy()
            .into_owned(),
    };

    let mut target_results = Vec::new();

    for def in &config.target {
        // Create a fresh workdir for this target
        let workdir = root.join(format!(".workdir-{}", sanitise_name(&def.name)));
        let _ = std::fs::remove_dir_all(&workdir);
        std::fs::create_dir_all(&workdir).map_err(|e| {
            format!(
                "failed to create work directory '{}': {} (os error {})",
                workdir.display(),
                e.kind(),
                e.raw_os_error().unwrap_or(0),
            )
        })?;

        // -----------------------------------------------------------------------
        // Setup phase (optional, once, not timed)
        // -----------------------------------------------------------------------
        let mut setup_ran = false;
        let mut setup_succeeded = true;

        if let Some(setup) = &def.setup {
            setup_ran = true;
            let setup_args = resolve_args(&setup.command, &setup.args, &root_str, &workdir);
            eprintln!(
                "[bench] target '{}': running setup: {} {}",
                def.name,
                setup_args[0],
                setup_args[1..].join(" ")
            );

            let output = std::process::Command::new(&setup_args[0])
                .args(&setup_args[1..])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .current_dir(&workdir)
                .output()
                .map_err(|e| {
                    format!(
                        "failed to spawn setup command '{}': {} (os error {})",
                        setup_args[0],
                        e.kind(),
                        e.raw_os_error().unwrap_or(0),
                    )
                });

            match output {
                Ok(out) => {
                    if !out.status.success() {
                        setup_succeeded = false;
                        let code = out.status.code().unwrap_or(-1);
                        eprintln!(
                            "[bench] target '{}': setup failed (exit code {})",
                            def.name, code
                        );
                    }
                }
                Err(e) => {
                    setup_succeeded = false;
                    eprintln!(
                        "[bench] target '{}': setup command {}: {}",
                        def.name, setup_args[0], e
                    );
                }
            }
        }

        // If setup failed, record the result with no trials and move on
        if !setup_succeeded {
            let resolved_all = resolve_args(&def.command, &def.args, &root_str, &workdir);
            let _ = std::fs::remove_dir_all(&workdir);
            target_results.push(TargetResult {
                name: def.name.clone(),
                command: def.command.clone(),
                args: def.args.clone(),
                resolved: resolved_all,
                cold: TrialStats {
                    trials: 0,
                    min_secs: None,
                    max_secs: None,
                    p50_secs: None,
                    p95_secs: None,
                },
                warm: TrialStats {
                    trials: 0,
                    min_secs: None,
                    max_secs: None,
                    p50_secs: None,
                    p95_secs: None,
                },
                trials: vec![],
                setup_ran,
                setup_succeeded,
            });
            continue;
        }

        // -----------------------------------------------------------------------
        // Trials
        // -----------------------------------------------------------------------
        let mut trials: Vec<TrialRecord> = Vec::new();

        for i in 0..def.repeat {
            // Resolve placeholders
            let resolved = resolve_args(&def.command, &def.args, &root_str, &workdir);

            // Clean workdir between repeats only when setup is absent.
            // When setup is present, the workdir holds whatever setup produced
            // and should persist across all trials.
            if i > 0 && def.setup.is_none() {
                let _ = std::fs::remove_dir_all(&workdir);
                std::fs::create_dir_all(&workdir).map_err(|e| {
                    format!(
                        "failed to recreate work directory '{}': {} (os error {})",
                        workdir.display(),
                        e.kind(),
                        e.raw_os_error().unwrap_or(0),
                    )
                })?;
            }

            let is_cold = i == 0;
            let t0 = Instant::now();
            let output = std::process::Command::new(&resolved[0])
                .args(&resolved[1..])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .current_dir(&workdir)
                .output()
                .map_err(|e| {
                    format!(
                        "failed to spawn command '{}': {} (os error {})",
                        resolved[0],
                        e.kind(),
                        e.raw_os_error().unwrap_or(0),
                    )
                });
            let elapsed = t0.elapsed();

            match output {
                Ok(out) => {
                    let exit_code = out.status.code().unwrap_or(-1);
                    trials.push(TrialRecord {
                        index: i,
                        cold: is_cold,
                        elapsed_secs: elapsed.as_secs_f64(),
                        exit_code,
                        failure: !out.status.success(),
                    });
                }
                Err(e) => {
                    // Process failed to spawn (command not found, etc.)
                    trials.push(TrialRecord {
                        index: i,
                        cold: is_cold,
                        elapsed_secs: elapsed.as_secs_f64(),
                        exit_code: -1,
                        failure: true,
                    });
                    eprintln!(
                        "[bench] target '{}' trial {}: command '{}': {}",
                        def.name, i, resolved[0], e
                    );
                }
            }
        }

        // Clean up workdir
        let _ = std::fs::remove_dir_all(&workdir);

        // Compute stats
        let resolved_all = resolve_args(&def.command, &def.args, &root_str, &workdir);
        let cold_stats = stats_for_trials(&trials, true);
        let warm_stats = stats_for_trials(&trials, false);

        target_results.push(TargetResult {
            name: def.name.clone(),
            command: def.command.clone(),
            args: def.args.clone(),
            resolved: resolved_all,
            cold: cold_stats,
            warm: warm_stats,
            trials,
            setup_ran,
            setup_succeeded,
        });
    }

    Ok(RunResults {
        harness_version: version,
        timestamp,
        environment: env_info,
        tree: tree_info,
        targets: target_results,
    })
}

/// Print a human-readable Markdown summary to stdout.
pub fn print_summary(results: &RunResults) {
    println!(
        "## bench run — {} files, {}",
        results.tree.files,
        human_bytes(results.tree.total_bytes)
    );
    println!();
    println!("| Target | Trial 0 (s) | Warm p50 (s) | Warm p95 (s) | Trials | Failures | Setup |");
    println!("|--------|-------------|--------------|--------------|--------|----------|-------|");
    for t in &results.targets {
        let cold_s = match t.cold.p50_secs {
            Some(v) => format!("{:.4}", v),
            None => "—".to_string(),
        };
        let warm_p50 = match t.warm.p50_secs {
            Some(v) => format!("{:.4}", v),
            None => "—".to_string(),
        };
        let warm_p95 = match t.warm.p95_secs {
            Some(v) => format!("{:.4}", v),
            None => "—".to_string(),
        };
        let n_fail = t.trials.iter().filter(|tr| tr.failure).count();
        let setup_status = if t.setup_ran {
            if t.setup_succeeded {
                "✅"
            } else {
                "❌"
            }
        } else {
            "—"
        };
        println!(
            "| {} | {} | {} | {} | {} | {} | {} |",
            t.name,
            cold_s,
            warm_p50,
            warm_p95,
            t.trials.len(),
            n_fail,
            setup_status
        );
    }
    println!();
    println!(
        "Tree: {} files · {} · Memory: {}",
        results.tree.files,
        human_bytes(results.tree.total_bytes),
        results.environment.memory_total_human,
    );
    println!(
        "Environment: {} {} · {} CPUs · {}",
        results.environment.os,
        results.environment.arch,
        results.environment.cpus,
        results.timestamp,
    );
    println!("Harness version: {}", results.harness_version);
    println!("* Times include process startup (wall-clock from spawn to exit).");
    println!(
        "* Trial 0 is the first trial; remaining trials are \"warm\".  No OS cache is ever dropped, so \"cold\" only means \"first\" — the page cache may already hold the tree."
    );
    println!("* Non-zero exit codes are recorded as failures in the JSON output.");
    println!("* Setup: ✅ = ran and succeeded, ❌ = ran and failed, — = no setup defined.");
    println!(
        "{}",
        cache_fit_note(
            results.tree.total_bytes,
            results.environment.memory_total_bytes
        )
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve `{root}` and `{workdir}` placeholders in a command definition.
fn resolve_args(cmd: &str, args: &[String], root: &str, workdir: &Path) -> Vec<String> {
    let workdir_str = workdir.to_string_lossy();
    std::iter::once(cmd.to_string())
        .chain(
            args.iter()
                .map(|a| a.replace("{root}", root).replace("{workdir}", &workdir_str)),
        )
        .collect()
}

/// Compute min/max/P50/P95 over a subset of trial records.
fn stats_for_trials(trials: &[TrialRecord], cold: bool) -> TrialStats {
    let filtered: Vec<f64> = trials
        .iter()
        .filter(|t| t.cold == cold && !t.failure)
        .map(|t| t.elapsed_secs)
        .collect();

    let n = filtered.len() as u64;
    if n == 0 {
        return TrialStats {
            trials: 0,
            min_secs: None,
            max_secs: None,
            p50_secs: None,
            p95_secs: None,
        };
    }

    let mut sorted = filtered.clone();
    sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());

    let min_secs = sorted[0];
    let max_secs = sorted[sorted.len() - 1];
    let p50_secs = percentile(&sorted, 50.0);
    let p95_secs = percentile(&sorted, 95.0);

    TrialStats {
        trials: n,
        min_secs: Some(min_secs),
        max_secs: Some(max_secs),
        p50_secs: Some(p50_secs),
        p95_secs: Some(p95_secs),
    }
}

/// Linear-interpolated percentile.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let k = (p / 100.0) * ((sorted.len() - 1) as f64);
    let lo = k.floor() as usize;
    let hi = k.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let frac = k - lo as f64;
    sorted[lo] * (1.0 - frac) + sorted[hi] * frac
}

/// Collect environment metadata.
fn collect_environment() -> Environment {
    let os = std::env::consts::OS.to_string();
    let arch = std::env::consts::ARCH.to_string();
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(1);

    let mem = total_memory_bytes();
    let human = human_bytes_or_unknown(mem);
    Environment {
        os,
        arch,
        cpus,
        memory_total_bytes: mem,
        memory_total_human: human,
    }
}

/// Attempt to read total physical memory in bytes.
/// Returns `None` when the value cannot be determined.
#[cfg(target_os = "linux")]
fn total_memory_bytes() -> Option<u64> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| {
                    // Format: "MemTotal:       16276012 kB"
                    l.split_whitespace()
                        .nth(1)
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(|kb| kb * 1024)
                })
        })
}

/// Attempt to read total physical memory in bytes via `GlobalMemoryStatusEx`.
#[cfg(target_os = "windows")]
fn total_memory_bytes() -> Option<u64> {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }

    extern "system" {
        fn GlobalMemoryStatusEx(lp_buffer: *mut MemoryStatusEx) -> i32;
    }

    let mut status = MemoryStatusEx {
        length: std::mem::size_of::<MemoryStatusEx>() as u32,
        memory_load: 0,
        total_phys: 0,
        avail_phys: 0,
        total_page_file: 0,
        avail_page_file: 0,
        total_virtual: 0,
        avail_virtual: 0,
        avail_extended_virtual: 0,
    };

    // SAFETY: GlobalMemoryStatusEx is a well-known Windows API.  The struct
    // is correctly sized and initialised with length before the call.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok != 0 && status.total_phys > 0 {
        Some(status.total_phys)
    } else {
        None
    }
}

/// Fallback for platforms without a known memory API.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn total_memory_bytes() -> Option<u64> {
    None
}

/// Format bytes in a human-friendly way.
fn human_bytes(b: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    if b == 0 {
        return "0 B".into();
    }
    let mut size = b as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if size >= 10.0 {
        format!("{:.0} {}", size, UNITS[unit_idx])
    } else {
        format!("{:.1} {}", size, UNITS[unit_idx])
    }
}

/// Format bytes or produce "unknown" when the value is absent.
/// This ensures `0 B` (an actual zero-byte measurement) is distinct from
/// `unknown` (the value could not be determined).
fn human_bytes_or_unknown(b: Option<u64>) -> String {
    match b {
        Some(v) => human_bytes(v),
        None => "unknown".into(),
    }
}

fn iso_timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple UTC formatting without external dep
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Gregorian calendar: days since 1970-01-01
    let (y, m, d) = days_to_date(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, hours, minutes, seconds
    )
}

fn days_to_date(mut days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant
    days += 719468; // shift epoch from 1970-01-01 to 0000-03-01
    let era = days / 146097;
    let doe = days % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Sanitise a name for use as a directory name.
fn sanitise_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Cache-fit judgment
// ---------------------------------------------------------------------------

/// Verdict on whether the generated tree can sit entirely in the OS page cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheFit {
    /// Tree size is known, memory is known, and the tree fits (`<=`): the whole
    /// tree can sit in the page cache, so trial 0 does not measure cold I/O.
    Fits,
    /// Tree size is known, memory is known, and the tree exceeds memory: the
    /// tree cannot be fully cached, so trial 0 includes a cold-I/O component.
    DoesNotFit,
    /// Tree size is zero (no manifest data) or total memory is unknown: no
    /// judgment is possible and none is claimed.
    Unknown,
}

/// Judge whether the tree fits in the OS page cache, given the tree's total
/// bytes and the machine's total physical memory.
///
/// Never claims fit or no-fit without data: when `tree_bytes` is 0 (no
/// manifest) or `memory_total_bytes` is `None` (could not be determined), the
/// verdict is [`CacheFit::Unknown`].
pub fn judge_cache_fit(tree_bytes: u64, memory_total_bytes: Option<u64>) -> CacheFit {
    match memory_total_bytes {
        Some(mem) if tree_bytes > 0 => {
            if tree_bytes <= mem {
                CacheFit::Fits
            } else {
                CacheFit::DoesNotFit
            }
        }
        _ => CacheFit::Unknown,
    }
}

/// Human-readable footer note for the cache-fit judgment.
fn cache_fit_note(tree_bytes: u64, memory_total_bytes: Option<u64>) -> String {
    match judge_cache_fit(tree_bytes, memory_total_bytes) {
        CacheFit::Fits => format!(
            "* WARNING: the tree ({}) fits in total memory ({}), so the whole tree can sit in the OS page cache; trial 0 does not measure cold I/O and I/O-bound indicators (especially hashing) will not reproduce real-machine ratios.",
            human_bytes(tree_bytes),
            human_bytes(memory_total_bytes.unwrap_or(0)),
        ),
        CacheFit::DoesNotFit => format!(
            "* The tree ({}) exceeds total memory ({}), so it cannot be fully cached; trial 0 includes a genuine cold-I/O component.",
            human_bytes(tree_bytes),
            human_bytes(memory_total_bytes.unwrap_or(0)),
        ),
        CacheFit::Unknown => format!(
            "* Cache fit cannot be judged ({}): neither \"fits\" nor \"does not fit\" is claimed.",
            cache_fit_unknown_reason(tree_bytes, memory_total_bytes)
        ),
    }
}

/// Why the cache fit could not be judged (for the "cannot be judged" note).
fn cache_fit_unknown_reason(tree_bytes: u64, memory_total_bytes: Option<u64>) -> &'static str {
    match (tree_bytes, memory_total_bytes) {
        (0, _) => "tree size is unknown (no manifest data)",
        (_, None) => "total memory is unknown",
        _ => "insufficient data",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percentile() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert!((percentile(&v, 50.0) - 3.0).abs() < 1e-9);
        assert!((percentile(&v, 95.0) - 4.8).abs() < 1e-9);
    }

    #[test]
    fn test_percentile_single() {
        assert!((percentile(&[42.0], 50.0) - 42.0).abs() < 1e-9);
    }

    #[test]
    fn test_percentile_empty() {
        assert!((percentile(&[], 50.0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_resolve_args_simple() {
        let root = "/tmp/tree";
        let workdir = Path::new("/tmp/work");
        let args = vec!["{root}/data".into(), "--out".into(), "{workdir}/out".into()];
        let resolved = resolve_args("find", &args, root, workdir);
        assert_eq!(resolved[0], "find");
        assert_eq!(resolved[1], "/tmp/tree/data");
        assert_eq!(resolved[2], "--out");
        assert_eq!(resolved[3], "/tmp/work/out");
    }

    #[test]
    fn test_human_bytes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(500), "500 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1048576), "1.0 MiB");
        assert_eq!(human_bytes(1073741824), "1.0 GiB");
    }

    #[test]
    fn test_sanitise_name() {
        assert_eq!(sanitise_name("find-files"), "find-files");
        assert_eq!(sanitise_name("my target!"), "my_target_");
    }

    #[test]
    fn test_load_config_valid() {
        let toml = r#"
[[target]]
name = "test"
command = "echo"
args = ["hello"]
repeat = 3
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.target.len(), 1);
        assert_eq!(cfg.target[0].name, "test");
    }

    #[test]
    fn test_load_config_empty_reject() {
        let toml = r#"
[[target]]
name = ""
command = ""
args = []
repeat = 0
"#;
        let _cfg = Config {
            target: vec![TargetDef {
                name: "".into(),
                command: "".into(),
                args: vec![],
                repeat: 0,
                setup: None,
            }],
        };
        // We test the validation via load_config which does extra checks
        let dir = std::env::temp_dir().join("bench_cfg_test");
        let _ = std::fs::create_dir_all(&dir);
        let cfg_path = dir.join("bad.toml");
        std::fs::write(&cfg_path, toml).unwrap();
        let result = load_config(&cfg_path);
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_iso_timestamp_format() {
        let ts = iso_timestamp();
        // Basic format check: YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(ts.len(), 20, "bad length: {}", ts);
        assert!(ts.ends_with('Z'), "not UTC: {}", ts);
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
    }

    #[test]
    fn test_days_to_date_known() {
        // 1970-01-01
        let (y, m, d) = days_to_date(0);
        assert_eq!((y, m, d), (1970, 1, 1));

        // 2026-07-29
        let days = days_since_epoch(2026, 7, 29);
        let (y, m, d) = days_to_date(days);
        assert_eq!((y, m, d), (2026, 7, 29));
    }

    fn days_since_epoch(y: u64, m: u64, d: u64) -> u64 {
        // Simple day count for a known date (not a general function)
        fn is_leap(y: u64) -> bool {
            (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
        }
        fn days_in(y: u64, m: u64) -> u64 {
            match m {
                1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
                4 | 6 | 9 | 11 => 30,
                2 => {
                    if is_leap(y) {
                        29
                    } else {
                        28
                    }
                }
                _ => 0,
            }
        }
        let mut total = 0u64;
        for yr in 1970..y {
            total += if is_leap(yr) { 366 } else { 365 };
        }
        for mo in 1..m {
            total += days_in(y, mo);
        }
        total + d - 1
    }

    #[test]
    fn test_target_def_with_setup() {
        let toml = r#"
[[target]]
name = "search"
command = "my-search"
args = ["{root}", "--index", "{workdir}/idx"]
repeat = 5
setup = { command = "build-index", args = ["{root}", "--out", "{workdir}/idx"] }
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.target.len(), 1);
        let t = &cfg.target[0];
        assert_eq!(t.name, "search");
        assert!(t.setup.is_some());
        let s = t.setup.as_ref().unwrap();
        assert_eq!(s.command, "build-index");
        assert_eq!(s.args, vec!["{root}", "--out", "{workdir}/idx"]);
    }

    #[test]
    fn test_target_def_without_setup_legacy() {
        let toml = r#"
[[target]]
name = "legacy"
command = "ls"
args = ["-la", "{root}"]
repeat = 2
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.target.len(), 1);
        let t = &cfg.target[0];
        assert!(t.setup.is_none());
    }

    #[test]
    fn test_load_config_valid_with_setup() {
        let toml = r#"
[[target]]
name = "with-setup"
command = "echo"
args = ["done"]
repeat = 3
setup = { command = "mkdir", args = ["-p", "{workdir}/data"] }
"#;
        let dir = std::env::temp_dir().join("bench_cfg_setup_test");
        let _ = std::fs::create_dir_all(&dir);
        let cfg_path = dir.join("good.toml");
        std::fs::write(&cfg_path, toml).unwrap();
        let result = load_config(&cfg_path);
        assert!(result.is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_config_rejects_empty_setup_command() {
        let toml = r#"
[[target]]
name = "bad-setup"
command = "echo"
args = ["hi"]
repeat = 1
setup = { command = "", args = [] }
"#;
        let dir = std::env::temp_dir().join("bench_cfg_bad_setup");
        let _ = std::fs::create_dir_all(&dir);
        let cfg_path = dir.join("bad.toml");
        std::fs::write(&cfg_path, toml).unwrap();
        let result = load_config(&cfg_path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("empty setup.command"), "error: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_setup_failure_produces_no_trials() {
        let result = TargetResult {
            name: "fail-setup".into(),
            command: "true".into(),
            args: vec![],
            resolved: vec!["true".into()],
            cold: TrialStats {
                trials: 0,
                min_secs: None,
                max_secs: None,
                p50_secs: None,
                p95_secs: None,
            },
            warm: TrialStats {
                trials: 0,
                min_secs: None,
                max_secs: None,
                p50_secs: None,
                p95_secs: None,
            },
            trials: vec![],
            setup_ran: true,
            setup_succeeded: false,
        };
        assert!(result.setup_ran);
        assert!(!result.setup_succeeded);
        assert!(result.trials.is_empty());
        assert!(result.cold.p50_secs.is_none());
        assert!(result.warm.p50_secs.is_none());
    }

    #[test]
    fn test_target_with_setup_serialization() {
        let tr = TargetResult {
            name: "with-setup".into(),
            command: "search".into(),
            args: vec!["query".into()],
            resolved: vec!["search".into(), "query".into()],
            cold: TrialStats {
                trials: 1,
                min_secs: Some(0.5),
                max_secs: Some(0.5),
                p50_secs: Some(0.5),
                p95_secs: Some(0.5),
            },
            warm: TrialStats {
                trials: 0,
                min_secs: None,
                max_secs: None,
                p50_secs: None,
                p95_secs: None,
            },
            trials: vec![TrialRecord {
                index: 0,
                cold: true,
                elapsed_secs: 0.5,
                exit_code: 0,
                failure: false,
            }],
            setup_ran: true,
            setup_succeeded: true,
        };
        let json = serde_json::to_string(&tr).unwrap();
        assert!(json.contains("\"setup_ran\":true"), "json: {json}");
        assert!(json.contains("\"setup_succeeded\":true"), "json: {json}");
    }

    #[test]
    fn test_parse_config_text_strips_bom() {
        let toml = r#"
[[target]]
name = "test"
command = "echo"
args = ["hello"]
repeat = 3
"#;
        // Parse without BOM
        let cfg1 = parse_config_text(toml).unwrap();
        assert_eq!(cfg1.target[0].name, "test");

        // Parse with BOM prefix
        let bom_toml = format!("\u{feff}{}", toml);
        let cfg2 = parse_config_text(&bom_toml).unwrap();
        assert_eq!(cfg2.target[0].name, "test");
        assert_eq!(cfg2.target.len(), cfg1.target.len());
    }

    #[test]
    fn test_parse_config_text_rejects_empty() {
        let result = parse_config_text("");
        assert!(result.is_err());
        // An empty string fails TOML parsing with "missing field `target`"
        let err = result.unwrap_err().to_string();
        assert!(err.contains("missing field"), "error: {err}");
    }

    #[test]
    fn test_human_bytes_or_unknown() {
        assert_eq!(human_bytes_or_unknown(Some(0)), "0 B");
        assert_eq!(human_bytes_or_unknown(Some(500)), "500 B");
        assert_eq!(human_bytes_or_unknown(Some(1536)), "1.5 KiB");
        assert_eq!(human_bytes_or_unknown(None), "unknown");
    }

    #[test]
    fn test_environment_memory_none_serializes_as_null() {
        let env = Environment {
            os: "test-os".into(),
            arch: "x86_64".into(),
            cpus: 4,
            memory_total_bytes: None,
            memory_total_human: "unknown".into(),
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(
            json.contains("\"memory_total_bytes\":null"),
            "expected null in json: {json}"
        );
        assert!(
            json.contains("\"memory_total_human\":\"unknown\""),
            "expected unknown in json: {json}"
        );
    }

    #[test]
    fn test_environment_memory_some_serializes_as_number() {
        let env = Environment {
            os: "test-os".into(),
            arch: "x86_64".into(),
            cpus: 4,
            memory_total_bytes: Some(8589934592),
            memory_total_human: "8.0 GiB".into(),
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(
            json.contains("\"memory_total_bytes\":8589934592"),
            "expected value in json: {json}"
        );
        assert!(
            json.contains("\"memory_total_human\":\"8.0 GiB\""),
            "expected 8.0 GiB in json: {json}"
        );
    }

    #[test]
    fn test_load_config_missing_path_reports_filename() {
        let missing = Path::new("/tmp/__this_path_does_not_exist__bench_cfg_test__.toml");
        let result = load_config(missing);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("__this_path_does_not_exist__bench_cfg_test__"),
            "error should contain the path, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Cache-fit judgment
    // -----------------------------------------------------------------------

    #[test]
    fn test_judge_cache_fit_fits() {
        // 10k-file tree (~1.8 GiB) on a 16 GiB machine: fits.
        assert_eq!(
            judge_cache_fit(1_879_953_005, Some(16 * 1024 * 1024 * 1024)),
            CacheFit::Fits
        );
        // Exact equality still fits (the whole tree could be cached).
        assert_eq!(judge_cache_fit(1024, Some(1024)), CacheFit::Fits);
    }

    #[test]
    fn test_judge_cache_fit_does_not_fit() {
        // 100k-file tree (~17 GiB) on a 16 GiB machine: does not fit.
        assert_eq!(
            judge_cache_fit(17 * 1024 * 1024 * 1024, Some(16 * 1024 * 1024 * 1024)),
            CacheFit::DoesNotFit
        );
    }

    #[test]
    fn test_judge_cache_fit_memory_unknown() {
        assert_eq!(judge_cache_fit(1_879_953_005, None), CacheFit::Unknown);
    }

    #[test]
    fn test_judge_cache_fit_tree_bytes_zero() {
        // Zero tree bytes (no manifest data) -> unknown, even with known memory.
        assert_eq!(
            judge_cache_fit(0, Some(16 * 1024 * 1024 * 1024)),
            CacheFit::Unknown
        );
        assert_eq!(judge_cache_fit(0, None), CacheFit::Unknown);
    }

    #[test]
    fn test_cache_fit_note_warns_when_fits() {
        let note = cache_fit_note(1_879_953_005, Some(16 * 1024 * 1024 * 1024));
        assert!(note.contains("WARNING"), "note: {note}");
        assert!(note.contains("fits in total memory"), "note: {note}");
        assert!(note.contains("does not measure cold I/O"), "note: {note}");
    }

    #[test]
    fn test_cache_fit_note_does_not_fit() {
        let note = cache_fit_note(17 * 1024 * 1024 * 1024, Some(16 * 1024 * 1024 * 1024));
        assert!(note.contains("exceeds total memory"), "note: {note}");
        assert!(note.contains("cold-I/O"), "note: {note}");
        assert!(!note.contains("WARNING"), "note: {note}");
    }

    #[test]
    fn test_cache_fit_note_unknown_is_not_a_warning() {
        // Memory unknown: "cannot judge", not a warning and not an all-clear.
        let note = cache_fit_note(1_879_953_005, None);
        assert!(note.contains("cannot be judged"), "note: {note}");
        assert!(note.contains("total memory is unknown"), "note: {note}");
        assert!(!note.contains("WARNING"), "note: {note}");
        assert!(!note.contains("exceeds"), "note: {note}");

        // Tree bytes zero: also "cannot judge", never fit/no-fit.
        let note0 = cache_fit_note(0, Some(16 * 1024 * 1024 * 1024));
        assert!(note0.contains("cannot be judged"), "note: {note0}");
        assert!(note0.contains("no manifest data"), "note: {note0}");
        assert!(!note0.contains("WARNING"), "note: {note0}");
        assert!(!note0.contains("exceeds"), "note: {note0}");
    }
}
