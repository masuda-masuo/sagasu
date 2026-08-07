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
//! which single value to take next, what the group is called — is
//! [`sagasu_core::browse`]. Nothing here computes anything the M4 Tauri UI would
//! then have to recompute differently; it formats a [`BrowseView`] and adds the
//! two things a *terminal* needs that an API does not: the literal next command
//! to type, and the existence check on the previewed rows.
//!
//! ## Why there is no `--json`
//!
//! The machine-readable interface is the core API: the UI of M4 links against
//! `sagasu-core` (design.md §3 — "コアが Rust ならそのまま接続") and calls
//! [`sagasu_core::browse::browse`] directly, so a JSON encoding here would be a
//! second contract describing the same structs, kept in sync by hand. CLI-wide
//! machine output is issue #6's question, and when it is answered it should be
//! answered once for every subcommand rather than starting here.
//!
//! ## What the printers are, structurally
//!
//! The pieces that *build strings* — [`shell_quote`], [`next_command`],
//! [`share_pct`] — are pure functions returning `String`, not `println!` sites,
//! so they have unit tests underneath them. Every finding in the first
//! adversarial review of this command was in the presentation layer, which at
//! the time had no tests at all.

use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use clap::Parser;

use sagasu_core::browse::{self, BrowseQuery, BrowseView, FacetAxis};
use sagasu_core::delta;
use sagasu_core::store::Store;
use sagasu_core::tagindex;
use sagasu_core::tags::Tag;

use crate::json;
use crate::output::{print_tag_freshness, tag_freshness, Output, Report, TagFreshness};

/// Default database path, so the reprinted command can leave `--db` out when it
/// would be redundant.
const DEFAULT_DB: &str = "index.db";

/// Arguments of `sagasu browse`.
#[derive(Parser)]
pub struct BrowseArgs {
    /// Tags chosen so far, `namespace:value`, ANDed. None = the whole index,
    /// which is where an exploration starts.
    selection: Vec<String>,

    /// Path to the SQLite database file.
    #[arg(long, default_value = DEFAULT_DB)]
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
pub fn cmd_browse(args: BrowseArgs, mode: Output) -> Result<()> {
    let mut report = Report::new(mode);
    let store = Store::open(&args.db)
        .with_context(|| format!("failed to open metadata index {:?}", args.db))?;

    // The same block `sagasu tags` prints, from the same function: a drill-down
    // reads the same index-time snapshot and owes the same admission about it.
    let stats = store.get_stats()?;
    let freshness = tag_freshness(
        &store,
        &stats,
        &TagFreshness {
            db: &args.db,
            no_fresh: args.no_fresh,
            delta_limit: args.delta_limit,
        },
        &mut report,
    );
    if !report.is_json() {
        print_tag_freshness(&freshness);
    }

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

    // Deletion is invisible to every delta source (design.md §5), so the
    // previewed rows are checked one by one — the same thing `sagasu tags`
    // does, bounded by the page rather than by the corpus. Done here rather
    // than inside the printer so both renderings see the same partition.
    let (rows, gone) = if args.files == 0 {
        (Vec::new(), Vec::new())
    } else {
        tagindex::partition_existing(view.preview.clone())
    };

    if report.is_json() {
        let (command, reason) = next_step(&view, &args);
        json::browse(
            &args.db.display().to_string(),
            &view,
            &freshness,
            command.as_deref(),
            reason.as_deref(),
            &rows,
            &gone,
            args.files > 0,
        );
    } else {
        print_selection(&view);
        print_label(&view);
        print_universal(&view);
        print_axes(&view, &args);
        print_preview(&view, &args, &rows, &gone);
        println!();
        println!("{}", next_line(&view, &args));
    }

    if !gone.is_empty() {
        report.warn(format!(
            "index is stale: {} of the previewed files have been deleted \
             since it was built — re-run `sagasu index <root>` and `sagasu tag`.",
            gone.len()
        ));
    }

    if report.is_json() {
        json::warnings(&report);
    }

    // An index with no tag layer has no facet tree at all — the same situation
    // `sagasu tags` (with no query) exits 1 for. Exiting 0 here would let a
    // scripted `browse` treat "there is nothing to browse" as a successful
    // empty answer, which is the silent-omission failure with a warning stapled
    // to it. The stderr warning itself already came from `tag_freshness`.
    if !view.snapshot.built() {
        process::exit(1);
    }
    Ok(())
}

// ── String builders (unit-tested below) ─────────────────────────────────────

/// Characters that need no quoting in any of the shells this hint targets.
fn is_shell_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || "._:/=+@,-".contains(c)
}

/// Quote one argument so the printed command survives a copy-paste.
///
/// Necessary rather than cosmetic: `path:` tag values are whole directory
/// components, and `TOKEN_SEPARATORS` in [`sagasu_core::tags`] includes the
/// space, so a folder called `2024 reports` produces the tag
/// `path:2024 reports`. Unquoted, pasting the hint back gives
/// `error: tag "reports" is not in namespace:value form` — a command the tool
/// printed and the tool then rejects.
///
/// POSIX single-quoting (`'…'`, with `'` written `'\''`). That is bash, zsh and
/// — for any value without an embedded apostrophe — PowerShell. `cmd.exe` wants
/// double quotes and is not covered; the hint is advisory, and the alternative
/// is to guess the shell from the OS, which is wrong as often as it is right.
pub(crate) fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(is_shell_safe) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// The flags of this invocation that differ from the defaults.
///
/// Carried into the reprinted command because they change the answer, not just
/// the layout: `--values` is the display budget the axis ranking is computed
/// against, and `--no-fresh` decides whether the next step is checked at all. A
/// hint that silently drops them hands the user a command that browses a
/// different tree from the one they are looking at.
fn non_default_flags(args: &BrowseArgs) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |flag: &str, value: String| {
        out.push(flag.to_string());
        out.push(value);
    };
    if args.db != Path::new(DEFAULT_DB) {
        push("--db", args.db.display().to_string());
    }
    if args.axes != browse::DEFAULT_MAX_AXES {
        push("--axes", args.axes.to_string());
    }
    if args.values != browse::DEFAULT_MAX_VALUES {
        push("--values", args.values.to_string());
    }
    if args.label_terms != browse::DEFAULT_LABEL_TERMS {
        push("--label-terms", args.label_terms.to_string());
    }
    if args.files != browse::DEFAULT_PREVIEW {
        push("--files", args.files.to_string());
    }
    if args.delta_limit != delta::DEFAULT_DELTA_LIMIT {
        push("--delta-limit", args.delta_limit.to_string());
    }
    if args.no_fresh {
        out.push("--no-fresh".to_string());
    }
    out
}

/// The literal command for the next step, quoted and ready to paste.
pub(crate) fn next_command(
    subcommand: &str,
    flags: &[String],
    selected: &[Tag],
    extra: Option<&Tag>,
) -> String {
    let mut words = vec!["sagasu".to_string(), subcommand.to_string()];
    words.extend(flags.iter().cloned());
    words.extend(selected.iter().map(|t| t.to_string()));
    words.extend(extra.map(|t| t.to_string()));
    words
        .iter()
        .map(|w| shell_quote(w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A share as a percentage, without rounding a real bucket down to `0%`.
///
/// `0% of the group` next to a non-zero file count reads as a bug or as an
/// empty bucket; it is neither, it is one file in sixty thousand.
pub(crate) fn share_pct(share: f64) -> String {
    let pct = 100.0 * share;
    if share > 0.0 && pct < 0.5 {
        "<1%".to_string()
    } else {
        format!("{pct:.0}%")
    }
}

/// The next step as data: the command to run next, and/or the reason there is
/// no step to recommend.
///
/// Both renderings go through this. *Why* there is no next step decides what
/// the user should do, and collapsing the four cases into one sentence was a way
/// of telling three groups of users something untrue — a machine consumer is
/// owed the same distinction, so the branch lives here rather than in the
/// `println!`.
pub(crate) fn next_step(view: &BrowseView, args: &BrowseArgs) -> (Option<String>, Option<String>) {
    if let Some(step) = &view.recommended {
        let flags = non_default_flags(args);
        return (
            Some(next_command(
                "browse",
                &flags,
                &view.selected,
                Some(&step.tag),
            )),
            None,
        );
    }
    if !view.snapshot.built() {
        return (
            None,
            Some("no tag layer in this index — run `sagasu tag` first".to_string()),
        );
    }
    if view.matched == 0 {
        return (
            None,
            Some("nothing to add — no live file carries all of these tags".to_string()),
        );
    }
    if view.axes_refining == 0 {
        return (
            Some(next_command("tags", &db_flag(args), &view.selected, None)),
            Some("nothing to add — this group is a leaf".to_string()),
        );
    }
    (
        None,
        Some(format!(
            "suppressed — {} axis/axes could narrow this group, but no value \
             is on screen at --axes {} --values {}",
            view.axes_refining, args.axes, args.values,
        )),
    )
}

/// The `next :` line, including the four different reasons there may be no step.
fn next_line(view: &BrowseView, args: &BrowseArgs) -> String {
    match (next_step(view, args), &view.recommended) {
        ((Some(command), _), Some(step)) => format!(
            "next    : {command}\n          ({:.2} bits — the step whose outcome is least \
             predictable, leaving {} of {} file(s))",
            step.bits, step.files, view.matched,
        ),
        ((Some(command), Some(reason)), None) => {
            format!("next    : ({reason}; list it with `{command}`)")
        }
        ((_, Some(reason)), None) => format!("next    : ({reason})"),
        _ => "next    : (nothing to add)".to_string(),
    }
}

/// Just `--db`, for hints at commands that do not take the browse flags.
fn db_flag(args: &BrowseArgs) -> Vec<String> {
    if args.db != Path::new(DEFAULT_DB) {
        vec!["--db".to_string(), args.db.display().to_string()]
    } else {
        Vec::new()
    }
}

// ── Printers ────────────────────────────────────────────────────────────────

/// The group the user is standing in.
fn print_selection(view: &BrowseView) {
    if view.is_root() {
        println!("select  : (whole index — no tag chosen yet)");
    } else {
        let labels: Vec<String> = view.selected.iter().map(|t| t.to_string()).collect();
        println!("select  : {}", labels.join(" AND "));
    }
    let share = if view.corpus > 0 {
        view.matched as f64 / view.corpus as f64
    } else {
        0.0
    };
    println!(
        "matched : {} of {} live files ({})",
        view.matched,
        view.corpus,
        share_pct(share)
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
        // Three reasons for an empty label, and only one of them is "there was
        // nothing to say". Reporting `--label-terms 0` as an absence of tags is
        // the tool lying about its own configuration.
        if view.matched == 0 {
            println!("label   : (none — this group is empty)");
        } else if view.label_vocabulary == 0 {
            println!("label   : (none — this group carries no tag beyond the selection)");
        } else {
            println!(
                "label   : (suppressed — --label-terms 0; {} candidate tag(s) were \
                 computed)",
                view.label_vocabulary
            );
        }
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
    // Four different reasons for an empty axis list, and they call for four
    // different next moves. Printing "this group is a leaf" for all of them
    // tells a user the tree ended when in fact they capped it, or matched
    // nothing, or never built a tag layer.
    if view.axes.is_empty() {
        if !view.snapshot.built() {
            println!("          (no tag layer in this index — run `sagasu tag` first)");
        } else if view.matched == 0 {
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
        "  {:<8}: {:.2} bits, {} value(s), covers {} of the group",
        axis.namespace,
        axis.score,
        axis.distinct,
        share_pct(axis.coverage),
    );
    for value in &axis.values {
        println!(
            "      {:>7}  {:<32} ({} of the group)",
            value.files,
            value.tag.to_string(),
            share_pct(value.share),
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
    // `covers 100%` above a column of shares adding to 232% is not a
    // contradiction, it is a multi-valued namespace — but nothing on screen says
    // so, and the reader's only two guesses are "bug" and "I misread it".
    if is_multi_valued(axis) {
        println!(
            "              (a file can carry several {}: values, so these shares \
             count some files more than once and sum past 100%)",
            axis.namespace,
        );
    }
}

/// Whether an axis assigns more tags than it covers files — i.e. some file in
/// the group carries two or more values of this namespace.
pub(crate) fn is_multi_valued(axis: &FacetAxis) -> bool {
    let shown: i64 = axis.values.iter().map(|v| v.files).sum();
    shown + axis.tail_assignments > axis.files
}

/// A few files of the group, existence-checked.
fn print_preview(
    view: &BrowseView,
    args: &BrowseArgs,
    rows: &[sagasu_core::store::FileRow],
    gone: &[sagasu_core::store::FileRow],
) {
    println!();
    if args.files == 0 {
        println!("files   : (not listed — pass -n/--files N)");
        return;
    }
    let fetched = view.preview.len();
    println!("files   : {} of {} shown", rows.len(), view.matched);
    for row in rows {
        println!("{:>8}  {}", row.file_id, row.path);
    }
    if !gone.is_empty() {
        println!(
            "dropped : {} of the {fetched} previewed row(s) no longer exist on disk",
            gone.len()
        );
        for row in gone {
            println!(
                "{:>8}  {}  (deleted since the index was built)",
                row.file_id, row.path
            );
        }
    }
    if view.matched > fetched as i64 {
        println!(
            "          ({} more — raise -n/--files, or list them all with \
             `sagasu tags`)",
            view.matched - fetched as i64
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sagasu_core::browse::{NextStep, TagLayerSnapshot};

    fn args() -> BrowseArgs {
        BrowseArgs {
            selection: Vec::new(),
            db: PathBuf::from(DEFAULT_DB),
            axes: browse::DEFAULT_MAX_AXES,
            values: browse::DEFAULT_MAX_VALUES,
            label_terms: browse::DEFAULT_LABEL_TERMS,
            files: browse::DEFAULT_PREVIEW,
            no_fresh: false,
            delta_limit: delta::DEFAULT_DELTA_LIMIT,
        }
    }

    fn view() -> BrowseView {
        BrowseView {
            selected: Vec::new(),
            matched: 10,
            corpus: 100,
            label: Vec::new(),
            label_vocabulary: 0,
            universal: Vec::new(),
            axes: Vec::new(),
            axes_total: 0,
            axes_refining: 0,
            recommended: None,
            preview: Vec::new(),
            snapshot: TagLayerSnapshot {
                tag_scan_generation: Some(1),
                scan_generation: 1,
            },
        }
    }

    fn tag(s: &str) -> Tag {
        Tag::parse(s).unwrap()
    }

    #[test]
    fn a_tag_value_with_a_space_survives_the_round_trip() {
        // The real shape: `TOKEN_SEPARATORS` includes the space, so a directory
        // called `2024 reports` becomes the tag `path:2024 reports`.
        assert_eq!(shell_quote("path:2024 reports"), "'path:2024 reports'");
        // …and the whole line is pasteable rather than three broken words.
        let line = next_command(
            "browse",
            &[],
            &[tag("kind:image")],
            Some(&tag("path:2024 reports")),
        );
        assert_eq!(line, "sagasu browse kind:image 'path:2024 reports'");
    }

    #[test]
    fn ordinary_arguments_are_not_dressed_up_in_quotes() {
        for plain in ["kind:image", "path:acme-corp", "--db", "sagasu", "a/b_c.d"] {
            assert_eq!(shell_quote(plain), plain);
        }
        assert_eq!(
            next_command("browse", &[], &[tag("kind:image")], Some(&tag("ext:png"))),
            "sagasu browse kind:image ext:png"
        );
    }

    #[test]
    fn the_characters_a_shell_would_act_on_are_quoted() {
        assert_eq!(shell_quote("a$b"), "'a$b'");
        assert_eq!(shell_quote("a`b`"), "'a`b`'");
        assert_eq!(shell_quote("a*b"), "'a*b'");
        assert_eq!(shell_quote("a\"b"), "'a\"b'");
        assert_eq!(shell_quote("a;b"), "'a;b'");
        assert_eq!(shell_quote(""), "''");
        // An embedded apostrophe closes and reopens the quoting.
        assert_eq!(shell_quote("john's"), r"'john'\''s'");
    }

    #[test]
    fn a_database_path_with_a_space_is_quoted_too() {
        let mut a = args();
        a.db = PathBuf::from("/tmp/my index/i.db");
        let flags = non_default_flags(&a);
        assert_eq!(flags, vec!["--db", "/tmp/my index/i.db"]);
        assert_eq!(
            next_command("browse", &flags, &[], Some(&tag("ext:png"))),
            "sagasu browse --db '/tmp/my index/i.db' ext:png"
        );
    }

    #[test]
    fn the_reprinted_command_carries_the_flags_that_change_the_answer() {
        let mut a = args();
        a.values = 2;
        a.axes = 1;
        a.no_fresh = true;
        assert_eq!(
            non_default_flags(&a),
            vec!["--axes", "1", "--values", "2", "--no-fresh"]
        );
        // …and leaves out the ones that are still at their default.
        assert!(non_default_flags(&args()).is_empty());
    }

    #[test]
    fn the_next_line_names_the_recommended_step_not_the_first_row() {
        let mut v = view();
        v.selected = vec![tag("kind:image")];
        v.recommended = Some(NextStep {
            namespace: "ext".to_string(),
            tag: tag("ext:png"),
            files: 5,
            share: 0.5,
            bits: 1.0,
        });
        let line = next_line(&v, &args());
        assert!(line.contains("sagasu browse kind:image ext:png"), "{line}");
        assert!(line.contains("1.00 bits"), "{line}");
        assert!(line.contains("leaving 5 of 10"), "{line}");
    }

    #[test]
    fn each_reason_for_having_no_next_step_says_which_one_it_is() {
        // No tag layer at all.
        let mut v = view();
        v.snapshot.tag_scan_generation = None;
        assert!(next_line(&v, &args()).contains("no tag layer"));

        // Nothing matched.
        let mut v = view();
        v.matched = 0;
        assert!(next_line(&v, &args()).contains("no live file carries"));

        // A genuine leaf: the axes exist but none of them refines.
        let mut v = view();
        v.selected = vec![tag("kind:code"), tag("path:cli")];
        v.axes_total = 4;
        v.axes_refining = 0;
        let leaf = next_line(&v, &args());
        assert!(leaf.contains("this group is a leaf"), "{leaf}");
        assert!(leaf.contains("sagasu tags kind:code path:cli"), "{leaf}");

        // Suppressed by the display budget — the case that used to advise
        // running `sagasu tag`, on an index that already had a tag layer and six
        // usable axes.
        let mut v = view();
        v.axes_refining = 6;
        let mut a = args();
        a.axes = 0;
        let capped = next_line(&v, &a);
        assert!(capped.contains("suppressed"), "{capped}");
        assert!(capped.contains("6 axis/axes"), "{capped}");
        assert!(!capped.contains("sagasu tag`"), "{capped}");
    }

    #[test]
    fn a_bucket_of_one_in_sixty_thousand_is_not_reported_as_zero_percent() {
        assert_eq!(share_pct(1.0 / 63901.0), "<1%");
        assert_eq!(share_pct(0.004), "<1%");
        // Zero really is zero, and the ordinary cases round as before.
        assert_eq!(share_pct(0.0), "0%");
        assert_eq!(share_pct(0.006), "1%");
        assert_eq!(share_pct(0.5), "50%");
        assert_eq!(share_pct(1.0), "100%");
    }

    #[test]
    fn a_multi_valued_axis_is_recognised_from_the_view_alone() {
        use sagasu_core::browse::FacetValue;
        let value = |v: &str, n: i64| FacetValue {
            tag: tag(&format!("path:{v}")),
            files: n,
            share: n as f64 / 10.0,
        };
        let mut axis = FacetAxis {
            namespace: "path".to_string(),
            score: 1.0,
            coverage: 1.0,
            files: 10,
            distinct: 2,
            values: vec![value("a", 10), value("b", 6)],
            tail_assignments: 0,
        };
        assert!(is_multi_valued(&axis), "16 assignments over 10 files");

        // A single-valued axis: every file lands in exactly one bucket.
        axis.values = vec![value("a", 6), value("b", 4)];
        assert!(!is_multi_valued(&axis));

        // The values that did not fit on screen still count towards it.
        axis.values = vec![value("a", 6)];
        axis.tail_assignments = 5;
        assert!(is_multi_valued(&axis));
    }
}
