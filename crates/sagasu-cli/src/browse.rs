//! `sagasu browse` — the facet drill-down (design.md §6, issue #5).
//!
//! One step of an exploration: given the tags chosen so far, print the group
//! they define, a machine-generated label for it, and the axes worth looking at
//! next with their top values. Adding one of those values to the command line is
//! the next step; three or four steps is the design target for reaching a file
//! whose directory you do not know.
//!
//! ## This file is a printer
//!
//! Every decision — which axes rank highest, which values are worth offering,
//! what the group is called — is [`sagasu_core::browse`]. Nothing here computes
//! anything the M4 Tauri UI would then have to recompute differently; it
//! formats a [`BrowseView`] and adds the two things a *terminal* needs that an
//! API does not: the literal next command to type, and the existence check on
//! the previewed rows.
//!
//! ## Why there is no `--json`
//!
//! The machine-readable interface is the core API: the UI of M4 links against
//! `sagasu-core` (design.md §3 — "コアが Rust ならそのまま接続") and calls
//! [`sagasu_core::browse::browse`] directly, so a JSON encoding here would be a
//! second contract describing the same structs, kept in sync by hand. CLI-wide
//! machine output is issue #6's question, and when it is answered it should be
//! answered once for every subcommand rather than starting here.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

use sagasu_core::browse::{self, BrowseQuery, BrowseView, FacetAxis};
use sagasu_core::delta;
use sagasu_core::store::Store;
use sagasu_core::tagindex;
use sagasu_core::tags::Tag;

use crate::output::{print_tag_freshness, TagFreshness};

/// Arguments of `sagasu browse`.
#[derive(Parser)]
pub struct BrowseArgs {
    /// Tags chosen so far, `namespace:value`, ANDed. None = the whole index,
    /// which is where an exploration starts.
    selection: Vec<String>,

    /// Path to the SQLite database file.
    #[arg(long, default_value = "index.db")]
    db: PathBuf,

    /// Axes to propose.
    #[arg(long, default_value_t = browse::DEFAULT_MAX_AXES)]
    axes: usize,

    /// Values to show per axis. Also the display budget the ranking is computed
    /// against, so raising it can change *which* axes come first, not only how
    /// many rows appear under them.
    #[arg(long, default_value_t = browse::DEFAULT_MAX_VALUES)]
    values: usize,

    /// Terms in the generated group label.
    #[arg(long, default_value_t = browse::DEFAULT_LABEL_TERMS)]
    label_terms: usize,

    /// Files to preview under the view. 0 = none.
    #[arg(long, short = 'n', default_value_t = browse::DEFAULT_PREVIEW)]
    files: usize,

    /// Skip the delta probe that reports how much the filesystem has moved
    /// since the tag layer was built.
    #[arg(long)]
    no_fresh: bool,

    /// Give up the delta probe above this many changed files.
    #[arg(long, default_value_t = delta::DEFAULT_DELTA_LIMIT)]
    delta_limit: usize,
}

/// Run `sagasu browse`.
pub fn cmd_browse(args: BrowseArgs) -> Result<()> {
    let store = Store::open(&args.db)
        .with_context(|| format!("failed to open metadata index {:?}", args.db))?;

    // The same block `sagasu tags` prints, from the same function: a drill-down
    // reads the same index-time snapshot and owes the same admission about it.
    let stats = store.get_stats()?;
    print_tag_freshness(
        &store,
        &stats,
        &TagFreshness {
            db: &args.db,
            no_fresh: args.no_fresh,
            delta_limit: args.delta_limit,
        },
    );

    let selected: Vec<Tag> = args
        .selection
        .iter()
        .map(|t| Tag::parse(t))
        .collect::<Result<Vec<_>>>()?;

    let query = BrowseQuery {
        selected,
        max_axes: args.axes,
        max_values: args.values,
        label_terms: args.label_terms,
        preview: args.files,
    };
    let view = browse::browse(&store, &query)?;

    println!();
    print_selection(&view);
    print_label(&view);
    print_universal(&view);
    print_axes(&view, &args);
    print_preview(&view, &args)?;
    Ok(())
}

/// The group the user is standing in.
fn print_selection(view: &BrowseView) {
    if view.is_root() {
        println!("select  : (whole index — no tag chosen yet)");
    } else {
        let labels: Vec<String> = view.selected.iter().map(|t| t.to_string()).collect();
        println!("select  : {}", labels.join(" AND "));
    }
    let share = if view.corpus > 0 {
        100.0 * view.matched as f64 / view.corpus as f64
    } else {
        0.0
    };
    println!(
        "matched : {} of {} live files ({:.1}%)",
        view.matched, view.corpus, share
    );
    // Same footing as `sagasu tags`: these counts come from the index and no row
    // behind them has been stat'd, so presenting them as facts would be the
    // silent-omission failure at the other end.
    println!(
        "          (an indexed count — an upper bound; only the previewed rows \
         below are checked against the filesystem)"
    );
}

/// The c-TF-IDF label of the group.
fn print_label(view: &BrowseView) {
    if view.label.is_empty() {
        println!("label   : (none — this group carries no tag beyond the selection)");
        return;
    }
    let terms: Vec<String> = view
        .label
        .iter()
        .map(|t| format!("{} ({}/{})", t.tag, t.files, view.matched))
        .collect();
    println!("label   : {}", terms.join("  "));
    // "5 of 5" and "5 of 900" are different amounts of trust in a label, and the
    // difference is invisible unless it is printed.
    println!(
        "          (c-TF-IDF over {} candidate tag(s): share of this group × \
         ln(1 + live files / files carrying the tag))",
        view.label_vocabulary,
    );
}

/// The ranked axes and their values — the actual next steps.
fn print_axes(view: &BrowseView, args: &BrowseArgs) {
    let dead = view.axes_total.saturating_sub(view.axes_refining);
    println!(
        "axes    : {} of {} shown, ranked by expected bits over the top {} value(s)",
        view.axes.len(),
        view.axes_refining,
        args.values,
    );
    if view.axes_refining > view.axes.len() {
        println!(
            "          ({} more axis/axes could narrow this group — raise --axes)",
            view.axes_refining - view.axes.len()
        );
    }
    // An axis present in the group but unable to split it is not a missing
    // result, but a user who counted namespaces in `sagasu tags` and got a
    // different number here deserves to know which number moved and why.
    if dead > 0 {
        println!(
            "          ({dead} further namespace(s) are present in this group but \
             cannot narrow it — every file shares the same value, or the only \
             values left are the ones already selected)"
        );
    }
    // Three different reasons for an empty axis list, and they call for three
    // different next moves. Printing "this group is a leaf" for all of them —
    // which this did until `--axes 0` was tried — tells a user the tree ended
    // when in fact they capped it, or when the selection matched nothing at all.
    if view.axes.is_empty() {
        if view.matched == 0 {
            println!("          (no live file carries all of these tags)");
        } else if view.axes_refining == 0 {
            println!("          (nothing left to drill into — this group is a leaf)");
        } else {
            println!("          (--axes 0: the axes above were computed, not shown)");
        }
        return;
    }

    for axis in &view.axes {
        println!();
        print_axis(axis);
    }
}

/// One axis: its score, why it scored that, and its values.
fn print_axis(axis: &FacetAxis) {
    println!(
        "  {:<8}: {:.2} bits, {} value(s), covers {:.0}% of the group",
        axis.namespace,
        axis.score,
        axis.distinct,
        100.0 * axis.coverage,
    );
    for value in &axis.values {
        println!(
            "      {:>7}  {:<32} ({:.0}% of the group)",
            value.files,
            value.tag.to_string(),
            100.0 * value.share,
        );
    }
    // A facet list cut at `--values` with nothing said about it reads as the
    // whole axis, and the count behind the cut is the part that matters: a tail
    // of 4 is a rounding error, a tail of 4000 means the axis was barely shown.
    if axis.distinct > axis.values.len() as i64 {
        println!(
            "              ({} of {} values shown — {} file-tag(s) behind the rest; \
             raise --values)",
            axis.values.len(),
            axis.distinct,
            axis.tail_assignments,
        );
    }
}

/// Tags the whole group shares. Not steps, but the truest one-line description
/// of what the group is — and the only place they appear, since an axis made
/// entirely of them is dropped from the ranking.
fn print_universal(view: &BrowseView) {
    if view.universal.is_empty() {
        return;
    }
    let all: Vec<String> = view.universal.iter().map(|t| t.to_string()).collect();
    println!(
        "shared  : all {} file(s) in this group carry {} — none of these narrows it",
        view.matched,
        all.join(", ")
    );
}

/// A few files of the group, existence-checked, and the command for the next step.
fn print_preview(view: &BrowseView, args: &BrowseArgs) -> Result<()> {
    println!();
    if args.files == 0 {
        println!("files   : (not listed — pass -n/--files N)");
    } else {
        let fetched = view.preview.len();
        // Deletion is invisible to every delta source (design.md §5), so the
        // rows about to be printed are checked one by one — the same thing
        // `sagasu tags` does, bounded by the page rather than by the corpus.
        let (rows, gone) = tagindex::partition_existing(view.preview.clone());
        println!("files   : {} of {} shown", rows.len(), view.matched);
        for row in &rows {
            println!("{:>8}  {}", row.file_id, row.path);
        }
        if !gone.is_empty() {
            println!(
                "dropped : {} of the {fetched} previewed row(s) no longer exist on disk",
                gone.len()
            );
            for row in &gone {
                println!(
                    "{:>8}  {}  (deleted since the index was built)",
                    row.file_id, row.path
                );
            }
            eprintln!(
                "WARNING: index is stale: {} of the previewed files have been deleted \
                 since it was built — re-run `sagasu index <root>` and `sagasu tag`.",
                gone.len()
            );
        }
        if view.matched > fetched as i64 {
            println!(
                "          ({} more — raise -n/--files, or list them all with \
                 `sagasu tags`)",
                view.matched - fetched as i64
            );
        }
    }

    // The literal next command. A drill-down that makes the user reconstruct the
    // argument list by hand is a drill-down with an extra step in it.
    let mut prefix = vec!["sagasu".to_string(), "browse".to_string()];
    if args.db != Path::new("index.db") {
        prefix.push("--db".to_string());
        prefix.push(args.db.display().to_string());
    }
    prefix.extend(view.selected.iter().map(|t| t.to_string()));
    let next = view
        .axes
        .first()
        .and_then(|a| a.values.first())
        .map(|v| v.tag.to_string());
    match next {
        Some(tag) => println!("\nnext    : {} {}", prefix.join(" "), tag),
        None if view.selected.is_empty() => println!(
            "\nnext    : (nothing to add — `sagasu tag` first, then `sagasu tags` \
             to see the namespaces)"
        ),
        None => println!(
            "\nnext    : (nothing to add — list this group with `sagasu tags {}`)",
            view.selected
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        ),
    }
    Ok(())
}
