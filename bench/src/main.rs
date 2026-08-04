//! sagasu benchmark harness
//!
//! Two subcommands:
//!
//! * `bench gen`  – generate a deterministic synthetic file tree for measurement.
//! * `bench run`  – run external commands (targets) from a TOML config,
//!   measuring wall-clock times, and produce JSON + human output.
//!
//! See `bench/README.md` for full documentation.

mod gen;
mod run;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bench", about = "sagasu benchmark harness")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a deterministic synthetic file tree for measurement.
    Gen {
        /// Output directory for the generated tree.
        #[arg(long)]
        out: PathBuf,

        /// Number of files to generate.
        #[arg(long)]
        files: u64,

        /// RNG seed for deterministic generation (default: 42).
        #[arg(long, default_value_t = 42)]
        seed: u64,

        /// Proportion of files whose body is predominantly Japanese (0.0–1.0).
        #[arg(long, default_value_t = 0.5)]
        japanese_ratio: f64,

        /// Cap on the largest file in bytes (default: 16 MiB).  Lowering it to
        /// 4 MiB or below removes the tail above proto-crawl's --hash-max-size,
        /// which disables the hash-skip path the benchmark is meant to exercise.
        #[arg(long, default_value_t = 16 * 1024 * 1024)]
        max_file_size: u64,
    },
    /// Run benchmark targets defined in a TOML config.
    Run {
        /// Path to the TOML configuration file.
        /// When omitted, the embedded default config (prototypes-windows.toml on
        /// Windows, prototypes-linux.toml elsewhere) is used.
        #[arg(long)]
        config: Option<PathBuf>,

        /// Root directory of the generated tree (substituted for {root}).
        #[arg(long)]
        root: PathBuf,

        /// Path for the JSON results output file.
        #[arg(long)]
        out: PathBuf,
    },
    /// Write the embedded default TOML config file to stdout.
    ///
    /// This is useful when you want to customise the benchmark targets:
    /// redirect the output to a file, edit it, and pass it to `bench run --config`.
    DumpDefaultConfig,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.cmd {
        Command::Gen {
            out,
            files,
            seed,
            japanese_ratio,
            max_file_size,
        } => {
            let tree = gen::generate_tree(&out, files, seed, japanese_ratio, max_file_size)?;
            let m = &tree.manifest;
            println!("seed           : {}", m.seed);
            println!("requested files: {}", m.requested_files);
            println!("actual files   : {}", m.actual_files);
            println!(
                "total bytes    : {} ({})",
                m.total_bytes,
                human_bytes(m.total_bytes)
            );
            println!("japanese ratio : {}", m.japanese_ratio);
            println!("manifest       : {}/.bench-manifest.json", out.display());
            println!();
            println!("Size histogram:");
            for (bucket, count) in &m.size_histogram {
                if *count > 0 {
                    println!("  {:>12}: {}", bucket, count);
                }
            }
            println!();
            println!("Planted terms:");
            for (term, count) in &m.planted_terms {
                println!("  {:>15}: {} files", term, count);
            }
            Ok(())
        }

        Command::Run { config, root, out } => {
            // A relative --root would otherwise be re-resolved against each
            // target's workdir (<root>/.workdir-<name>/) when substituted for
            // {root}, making every spawn fail while bench still exits 0 (#29).
            // Resolve it up front; a missing root is a hard error here, not a
            // per-trial failure.
            let root = resolve_root(&root)?;

            let cfg = match config {
                Some(path) => run::load_config(&path)?,
                None => {
                    eprintln!(
                        "[bench] using embedded default config ({}): {}",
                        if cfg!(target_os = "windows") {
                            "Windows"
                        } else {
                            "Linux"
                        },
                        run::EMBEDDED_CONFIG_NAME,
                    );
                    run::parse_config_text(run::EMBEDDED_CONFIG)?
                }
            };

            // Try to read the tree manifest for metadata
            let manifest_path = root.join(".bench-manifest.json");
            let manifest_info = if manifest_path.exists() {
                std::fs::read_to_string(&manifest_path).ok().and_then(|s| {
                    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
                    let files = v.get("actual_files")?.as_u64()?;
                    let bytes = v.get("total_bytes")?.as_u64()?;
                    Some((files, bytes))
                })
            } else {
                None
            };

            let results = run::run_benchmarks(&cfg, &root, manifest_info)?;

            // Write JSON output
            let json = serde_json::to_string_pretty(&results)?;
            std::fs::write(&out, &json).map_err(|e| {
                format!(
                    "failed to write results '{}': {} (os error {})",
                    out.display(),
                    e.kind(),
                    e.raw_os_error().unwrap_or(0),
                )
            })?;
            println!("Wrote results to {}", out.display());
            println!();

            // Print human-readable summary
            run::print_summary(&results);

            Ok(())
        }
        Command::DumpDefaultConfig => {
            print!("{}", run::EMBEDDED_CONFIG);
            Ok(())
        }
    }
}

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

/// Resolve `--root` to an absolute path, or fail with the offending path.
///
/// `bench run` executes targets with their workdir *inside* the root, so a
/// relative root passed through to `{root}` substitution points at a
/// nonexistent path and every trial fails while the exit code stays 0 (#29).
fn resolve_root(root: &std::path::Path) -> Result<PathBuf, String> {
    let canon = std::fs::canonicalize(root).map_err(|e| {
        format!(
            "failed to resolve --root '{}': {} (os error {})",
            root.display(),
            e.kind(),
            e.raw_os_error().unwrap_or(0),
        )
    })?;
    Ok(strip_verbatim_prefix(canon))
}

/// Strip Windows verbatim prefixes: `\\?\C:\x` → `C:\x`,
/// `\\?\UNC\srv\share` → `\\srv\share`.  Anything else passes through.
///
/// On Windows `std::fs::canonicalize` returns verbatim (`\\?\`) paths; some
/// child processes mishandle them, and Windows is the primary measurement
/// platform, so the prefix must not leak into `{root}` substitution.  Kept
/// free of `#[cfg(windows)]` so it compiles and is tested on every platform.
fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{}", rest));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest.to_string());
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_verbatim_drive_path() {
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from(r"\\?\C:\bench\tree")),
            PathBuf::from(r"C:\bench\tree")
        );
    }

    #[test]
    fn strip_verbatim_unc_path() {
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from(r"\\?\UNC\srv\share\tree")),
            PathBuf::from(r"\\srv\share\tree")
        );
    }

    #[test]
    fn strip_verbatim_leaves_plain_paths_alone() {
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from("/tmp/tree")),
            PathBuf::from("/tmp/tree")
        );
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from(r"C:\tree")),
            PathBuf::from(r"C:\tree")
        );
    }

    #[test]
    fn resolve_root_missing_path_names_it() {
        let err = resolve_root(std::path::Path::new("no-such-dir-29")).unwrap_err();
        assert!(err.contains("no-such-dir-29"), "error was: {err}");
        assert!(err.contains("failed to resolve --root"), "error was: {err}");
    }

    #[test]
    fn resolve_root_existing_path_is_absolute() {
        let resolved = resolve_root(&std::env::temp_dir()).unwrap();
        assert!(resolved.is_absolute());
    }
}
