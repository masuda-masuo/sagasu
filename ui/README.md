# sagasu ui — the M4 step 3 browse frontend

A static browse screen for the M4 desktop UI, decided here *before* the Tauri
shell exists (step 3 runs before step 2 on purpose). It is plain HTML, CSS and
JavaScript — no build step, no bundler, no package manager, nothing fetched
from a CDN. Everything the page needs lives in this directory.

The webview boundary is `BrowseViewDto` from `crates/sagasu-ui` (step 1). The
page is fed by committed fixture files that are **the actual wire format**:
JSON produced by serializing the DTO that `sagasu_ui::browse_view` returned
from a real index — never `sagasu browse --json` (that is the CLI's human
output in machine form, a different contract; `docs/cli.md` §8) and never
hand-written.

## Opening the page

```
cd ui
python3 -m http.server 8000
```

then open <http://localhost:8000/>. (A plain static server; no Rust, no
Tauri, no webview involved.) Most browsers refuse `fetch()` on `file://`
URLs, so opening `ui/index.html` directly usually shows the "could not load"
notice instead of the fixtures — the page says so on screen. In step 2 the
same page loads inside the Tauri webview, where `loadBrowseView()`'s body is
swapped for an `invoke()` call and nothing else changes.

What you should be able to do once it is open:

- see the root browse state: selection, `matched` vs `corpus` (with the
  upper-bound note), the c-TF-IDF label, `axes` with clickable values,
  `axes_total`/`axes_refining`, the `recommended` card, the preview table,
  and the snapshot facts in the header;
- click a facet value to drill down — `kind:code` is the one state the mock
  ships after the root; the crumb trail (or the browser back button) gets you
  back out;
- click any *other* value (e.g. `ext:rs`, `path:crates`) to see the honest
  hole: a visible "no fixture for this selection" notice, distinct from an
  empty result set;
- see `dropped` on screen, in the drilled state: one of its previewed rows
  was deleted from the corpus after indexing, so it appears in the red
  "dropped" panel rather than the file table.

The recommended step is rendered as its own card, deliberately not as another
axis row — `docs/browse.md` §2-1: it is *not* the top row of the top axis,
and rendering it as one destroys the one thing it carries.

## Regenerating the fixtures

After the DTOs change, one command rebuilds the whole set:

```
./ui/regenerate-fixtures.sh
```

It builds the workspace, copies this repository to a temporary directory as
the corpus (`docs/browse.md` §7-2 documents the repo itself as a browse
corpus), indexes it and builds the tag layer with the real CLI, then dumps
the two states with `crates/sagasu-ui/examples/dump_browse_view`:

| fixture | selection | notes |
|---|---|---|
| `ui/fixtures/browse-root.json` | none (the exploration root) | `dropped` is empty — the check ran and found nothing |
| `ui/fixtures/browse-drilled.json` | `kind:code` (one click down from the root) | one previewed file is **deleted from the corpus between two dumps**, so `dropped` is non-empty and `matched` stays an upper bound |

The script verifies its own output against `ui/fixtures/index.json` (the
selection-key → file map; static plumbing, not regenerated) and fails loudly
if `kind:code` ever stops existing in the corpus.

The dumps are *snapshots*, not reproducible builds: the crawler walks in
parallel, so file ids, preview ordering and `ctime_ns`/`fs_id` come from the
machine that runs the script. What is stable is the *shape* — every field of
`BrowseViewDto`, real data, produced by the dumper.

### The dumper

`crates/sagasu-ui/examples/dump_browse_view.rs` opens a real index database,
calls `sagasu_ui::browse_view` — the exact function step 2 wraps — and writes
the returned `BrowseViewDto` as JSON. Selection and query knobs come from the
command line:

```
cargo run -p sagasu-ui --example dump_browse_view -- --db <DB> --out <FILE> [TAG...]
    [--max-axes N] [--max-values N] [--label-terms N] [--preview N]
```

It uses `serde_json` (already a dev-dependency of `sagasu-ui`) and nothing
else; examples build with dev-dependencies available.

## What the mock does not cover (said out loud)

- **No fixture for arbitrary selections.** Clicking any value other than
  `kind:code` shows a distinct "no fixture" notice — the page does not
  pretend a missing state is an empty result set.
- **No `find` / `search` screens.** `sagasu-ui` covers browse only; the other
  commands arrive later as more DTOs in the same shape. Browse does not need
  a search box to make sense on its own.
- **No DTO field is missing.** Everything the screen shows comes from
  `BrowseViewDto` as step 1 froze it. In particular the query knobs
  (`max_axes`, `max_values`, `label_terms`, `preview`) are not part of the
  DTO — they are request parameters — so the mock renders the default query
  only; nothing about the browse design depends on the knobs being tweakable
  from the UI.
- **No Tauri shell.** The shell is step 2 by design: it needs webkit2gtk
  system libraries and cannot be built or run in a container, so the
  frontend is settled here where it can be run and looked at. Nothing in the
  page needs a piece of the shell to be decided.
- **The dropped fixture is real, not synthetic.** The regeneration script
  produces it deterministically: index → tag → dump → delete one previewed
  file → dump again. It is committed as `browse-drilled.json` — the same
  state a user reaches by clicking.
- **Timestamp/hex columns.** `mtime_ns`/`ctime_ns` are rendered as the
  decimal strings they arrive as (converting to a JavaScript `Number` would
  silently round them past 2^53), and `magic`/`blake3`/`fs_id` as the
  lowercase hex they arrive as.