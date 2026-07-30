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
