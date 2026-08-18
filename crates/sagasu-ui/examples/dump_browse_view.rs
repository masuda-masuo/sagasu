//! M4 step 3 fixture dumper: serialize a real browse view as JSON.
//!
//! The step 3 frontend (`ui/`) is a static page with no Rust anywhere near it.
//! It is fed by committed fixture files that must be **the actual wire format**
//! — the JSON a webview would receive from step 2's Tauri command — and the
//! only way to be sure of that is to produce them from the real thing. This
//! example opens a real index database, calls [`sagasu_ui::browse_view`] — the
//! exact function step 2 wraps — and serializes the [`BrowseViewDto`] it
//! returned. Nothing here hand-assembles JSON and nothing shells out to the
//! CLI, so the fixtures cannot drift from the DTOs; when the DTOs change,
//! `ui/regenerate-fixtures.sh` rebuilds the whole set with one command.
//!
//! The selection and the query knobs come from the command line, so any state
//! the committed fixtures need (the empty root, one tag drilled, a preview
//! row deleted after indexing so `dropped` is non-empty) is reachable.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};

use sagasu_core::store::Store;
use sagasu_ui::{browse_view, BrowseQueryDto};

const USAGE: &str = "\
dump_browse_view: serialize the browse view of a real index as JSON.

Usage: dump_browse_view [OPTIONS] [TAG...]

  TAG               selection tags, namespace:value, ANDed. None = the whole
                    index, which is where an exploration starts.

Options:
  --db PATH         index database to read (default: index.db)
  --out PATH        write the JSON to PATH instead of stdout
  --max-axes N      axes to return (default 4)
  --max-values N    values per axis (default 8)
  --label-terms N   terms in the generated group label (default 5)
  --preview N       files to preview (default 5)
  -h, --help        print this help
";

struct Args {
    db: PathBuf,
    out: Option<PathBuf>,
    selected: Vec<String>,
    max_axes: usize,
    max_values: usize,
    label_terms: usize,
    preview: usize,
}

fn parse_numeric(flag: &str, value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .with_context(|| format!("{flag} wants a non-negative integer, got {value:?}"))
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        db: PathBuf::from("index.db"),
        out: None,
        selected: Vec::new(),
        max_axes: sagasu_core::browse::DEFAULT_MAX_AXES,
        max_values: sagasu_core::browse::DEFAULT_MAX_VALUES,
        label_terms: sagasu_core::browse::DEFAULT_LABEL_TERMS,
        preview: sagasu_core::browse::DEFAULT_PREVIEW,
    };

    let mut it = env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--db" => {
                args.db = PathBuf::from(it.next().context("--db wants a path")?);
            }
            "--out" => {
                args.out = Some(PathBuf::from(it.next().context("--out wants a path")?));
            }
            "--max-axes" => args.max_axes = parse_numeric("--max-axes", &it.next().context("--max-axes wants a number")?)?,
            "--max-values" => {
                args.max_values = parse_numeric("--max-values", &it.next().context("--max-values wants a number")?)?
            }
            "--label-terms" => {
                args.label_terms = parse_numeric("--label-terms", &it.next().context("--label-terms wants a number")?)?
            }
            "--preview" => args.preview = parse_numeric("--preview", &it.next().context("--preview wants a number")?)?,
            other if other.starts_with("--") => bail!("unknown option {other:?}"),
            tag => args.selected.push(tag.to_string()),
        }
    }
    Ok(args)
}

fn run(args: Args) -> Result<()> {
    let store = Store::open(&args.db)
        .with_context(|| format!("failed to open index database {:?}", args.db))?;

    // The same shape a webview sends in step 2, minus nothing: the DTO is
    // serialized exactly as `browse_view` returned it.
    let query = BrowseQueryDto {
        selected: args.selected,
        max_axes: args.max_axes,
        max_values: args.max_values,
        label_terms: args.label_terms,
        preview: args.preview,
    };
    let view = browse_view(&store, &query)?;

    let mut json = serde_json::to_string_pretty(&view).context("serializing the view")?;
    json.push('\n');
    match &args.out {
        Some(path) => {
            fs::write(path, &json).with_context(|| format!("writing {:?}", path))?;
            eprintln!("wrote {} bytes to {}", json.len(), path.display());
        }
        None => print!("{json}"),
    }
    Ok(())
}

fn main() -> ExitCode {
    match parse_args() {
        Err(err) => {
            eprintln!("error: {err:#}");
            eprintln!();
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
        Ok(args) => match run(args) {
            Err(err) => {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
            Ok(()) => ExitCode::SUCCESS,
        },
    }
}
