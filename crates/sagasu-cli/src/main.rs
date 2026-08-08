//! sagasu CLI — local file search engine.
//!
//! Subcommands:
//! - `index`:    parallel metadata crawl + SQLite index.
//! - `hash`:     backfill BLAKE3 content hashes for unhashed files.
//! - `fulltext`: extract bodies from the indexed files and build the tantivy
//!   (Lindera) full-text index.
//! - `search`:   keyword search over the full-text index, score-ordered.
//! - `find`:     path search over the metadata index.
//! - `tag`:      generate the rule-based semantic tag layer (design.md §6).
//! - `tags`:     browse tags, filter files by them, explain one file's tags.
//! - `browse`:   facet drill-down — from a tag selection to the axes worth
//!   looking at next, with a machine-generated label for the group (design.md
//!   §6, issue #5).
//! - `status`:   print index statistics.
//!
//! The pipeline order is `index` → (`hash`) → `fulltext` → `search`, with `tag`
//! hanging off `index` as an independent stage (`index` → `tag` → `tags`):
//! `fulltext` and `tag` both read the live rows of the metadata index rather
//! than walking the filesystem again, so every stage shares one exclusion set
//! and one set of stable file IDs.
//!
//! ## Freshness
//!
//! `search` and `find` are **fresh by default**: before answering they ask a
//! delta source what changed since the index was built, live-scan that set and
//! merge it over the index result, so a file added, edited or deleted since the
//! last crawl is answered correctly without re-indexing (design.md §5).
//! `--no-fresh` turns the merge off — and says so in the output, because an
//! unmerged answer that looks fresh is the exact failure this design exists to
//! prevent.
//!
//! ## File layout
//!
//! This file holds the clap definitions and the dispatch, nothing else. Each
//! subcommand's arguments and implementation live next to the others it shares a
//! stage with: [`index`] for the write side (`index` / `hash` / `fulltext`),
//! [`search`] for the read side (`search` / `find`), [`tag`] for the tag layer
//! (`tag` / `tags`), [`browse`] for the drill-down over it, [`status`] for the
//! report, and [`output`] for the formatting more than one of them needs —
//! which now includes the tag-layer snapshot block `tags` and `browse` share.

use std::process;

use clap::{Parser, Subcommand};

use crate::output::Output;

mod browse;
mod index;
mod json;
mod output;
mod search;
mod status;
mod tag;

#[derive(Parser)]
#[command(name = "sagasu", about = "Local file search engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Machine-readable output: JSON Lines for result streams (`search`,
    /// `find`, `tags`, `browse`), one JSON object for summaries (`index`,
    /// `hash`, `fulltext`, `tag`, `status`). See docs/cli.md §4.
    ///
    /// Warnings stay on stderr as before *and* appear in the JSON; errors are
    /// never JSON, so decide on the exit code rather than on the stream.
    ///
    /// Global on purpose: one spelling for nine subcommands, and it may be
    /// written before or after the subcommand.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Crawl a directory tree and build (or update) a metadata index.
    Index(index::IndexArgs),
    /// Backfill BLAKE3 content hashes for files that don't have one yet.
    Hash(index::HashArgs),
    /// Extract bodies from the indexed files and build the full-text index.
    Fulltext(index::FulltextArgs),
    /// Search the full-text index (score order, path + snippet).
    Search(search::SearchArgs),
    /// Find files by path substring over the metadata index.
    Find(search::FindArgs),
    /// Generate the rule-based semantic tag layer over the metadata index.
    Tag(tag::TagArgs),
    /// Browse tags, filter files by them, or explain one file's tags.
    Tags(tag::TagsArgs),
    /// Drill down through the facet hierarchy: what to filter on next.
    Browse(browse::BrowseArgs),
    /// Print index statistics.
    Status(status::StatusArgs),
}

/// Default location of the tantivy index, used by both `fulltext` and `search`.
const DEFAULT_INDEX_DIR: &str = "fulltext-index";

/// What a subcommand did, decoupled from the process exit code so the 0/1/2
/// contract (docs/cli.md §6) is owned in exactly one place.
///
/// A subcommand never calls `process::exit` itself: it returns one of these,
/// or an `Err`, and [`run`] is the only function that turns that into a code.
/// The scattered exits that produced the old 2-value contract were exactly
/// this enum's reason to exist.
pub(crate) enum Outcome {
    /// The command ran correctly and the answer is non-empty — exit 0.
    Success,
    /// Read command: ran correctly, but the answer is empty — exit 1.
    Empty,
    /// The setup was unusable or no work was done; the command already warned
    /// about it on stderr — exit 2. Kept apart from `Err` because its message
    /// is a `WARNING:`, not an `error:` line, and apart from `Empty` because
    /// "nothing was indexed" / "the index is empty" is a broken setup, not a
    /// legitimate "no match".
    Unusable,
}

// ── main ────────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();

    let mode = Output::from_flag(cli.json);

    process::exit(run(cli.command, mode));
}

/// Run one subcommand and map its outcome to the process exit code.
///
/// 0 = ran fine with an answer; 1 = read command whose answer is empty; 2 =
/// everything that used to mean "problem" — `anyhow` errors (`error:` on
/// stderr) and the unusable-setup / no-work-done cases the subcommands already
/// reported as warnings. clap usage errors exit 2 on their own, which is the
/// same code by design (docs/cli.md §6).
fn run(command: Command, mode: Output) -> i32 {
    let result = match command {
        Command::Index(args) => index::cmd_index(args, mode),
        Command::Hash(args) => index::cmd_hash(args, mode),
        Command::Fulltext(args) => index::cmd_fulltext(args, mode),
        Command::Search(args) => search::cmd_search(args, mode),
        Command::Find(args) => search::cmd_find(args, mode),
        Command::Tag(args) => tag::cmd_tag(args, mode),
        Command::Tags(args) => tag::cmd_tags(args, mode),
        Command::Browse(args) => browse::cmd_browse(args, mode),
        Command::Status(args) => status::cmd_status(args, mode),
    };

    match result {
        Ok(Outcome::Success) => 0,
        Ok(Outcome::Empty) => 1,
        Ok(Outcome::Unusable) => 2,
        Err(e) => {
            eprintln!("error: {e:#}");
            2
        }
    }
}
