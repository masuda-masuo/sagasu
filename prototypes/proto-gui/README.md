# proto-gui — M2 incremental search measurement instrument

## What it measures

This is NOT a UI prototype — it is a measurement instrument that answers
four open hypotheses from the M2 incremental search design (issue #17):

1. **Per-keystroke latency**: What is the end-to-end latency (search + IPC +
   render) when a search fires on every input event without debounce?
2. **Delta-walk feasibility on every keystroke**: Can the freshness delta walk
   run on every query, or does it need caching?
3. **IPC overhead**: How much latency does the webview IPC round-trip add
   versus the pure Rust search time?
4. **Plain DOM at 10k rows**: Does a plain DOM list (no virtual scrolling)
   survive 10k result rows?

## How to run

**Prerequisite**: a tantivy index built by `proto-fulltext`.  The index
directory (`ft-index`) must contain `sagasu-meta.txt` (written by
`proto-fulltext index`).

On Windows, with the WebView2 runtime installed (included in Windows 10
1809+ and Windows 11):

```
proto-gui.exe --index-dir <root>\.workdir-fulltext-index\ft-index
```

## Timing columns

| Column | Meaning |
|--------|---------|
| search | Tantivy index search (ms) |
| δ-walk | Delta walk: stat-only scan for changed files (ms) |
| live-grep | Live-grep on changed files (ms, 0 on cache hit) |
| merge | Merge index + live results (ms) |
| total server | search + δ-walk + live-grep + merge (ms) |
| IPC | WebView round-trip: performance.now() around invoke minus server total (ms) |
| render | DOM update: double requestAnimationFrame after list update (ms) |
| total client | total server + IPC + render (ms) |

## Modes

- **Δ every query**: Delta walk + live-grep on every keystroke.
- **Δ cached**: Delta walk result is cached for `ttl_ms`.  Live-grep still
  runs on every query (the cache holds the changed-path *set*, not the grep
  results).  Use the Δ-walk column to see whether the cache hit avoids the
  walk cost.
- **index only**: Index search with no freshness at all (baseline).

## Exporting the log

Click "📤 export log" or call the Tauri command `export_log(path)`.  The
output is JSONL — one line per query, containing:

- `timestamp_unix_s`, `query`, `mode`, `limit`
- All server timings: `search_ms`, `delta_walk_ms`, `live_grep_ms`,
  `merge_ms`, `total_ms`
- Client timings: `ipc_ms`, `render_ms`
- Delta info: `delta_changed_count`, `delta_cache_hit`,
  `delta_cache_age_ms`

## WebView2 runtime

The Tauri v2 shell requires the WebView2 runtime.  Windows 10 1809+ and
Windows 11 ship it; for older or Server SKUs, install it from
<https://developer.microsoft.com/en-us/microsoft-edge/webview2/>.

## Building

The Tauri shell can only be compiled on Windows (needs webkit2gtk system
libs on Linux).  The CI Windows job builds it with:

```
cargo build --release -p proto-gui
```

Core logic (`proto-gui-core`) is a plain rlib, fully buildable and testable
on Linux:

```
cargo test -p proto-gui-core
```
