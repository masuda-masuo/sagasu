# bench — sagasu benchmark harness

Reproducible performance measurement for the sagasu project.  This harness
generates synthetic file trees and runs external commands (targets) defined in
TOML configs, recording wall-clock times and environment metadata so that every
result can be compared with any other.

## Quick start

```bash
# Build the harness
cargo build --release --manifest-path bench/Cargo.toml

# Generate a 200-file tree
./bench/target/release/bench gen --out /tmp/bt-a --files 200 --seed 42

# Verify determinism: two trees with the same seed are byte-identical
./bench/target/release/bench gen --out /tmp/bt-b --files 200 --seed 42
diff -r /tmp/bt-a /tmp/bt-b

# Run the smoke-test config (or omit --config to use the embedded default)
./bench/target/release/bench run \
    --config bench/configs/smoke.toml \
    --root /tmp/bt-a \
    --out /tmp/bench-result.json
```

## Subcommands

### `bench dump-default-config` — print embedded config

```
bench dump-default-config
```

Prints the platform-appropriate default config (`prototypes-windows.toml` on
Windows, `prototypes-linux.toml` elsewhere) to stdout.  Redirect to a file to
obtain a customisable copy:

```
bench dump-default-config > my-config.toml
bench run --config my-config.toml --root /tmp/tree --out results.json
```

### `bench gen` — deterministic tree generator

```
bench gen --out <dir> --files <N> [--seed <N>] [--japanese-ratio <0..1>] [--max-file-size <bytes>]
```

Generates a synthetic file tree with `N` files under `<dir>`.

| Flag               | Default | Description                                              |
|--------------------|---------|----------------------------------------------------------|
| `--out`            | —       | Output directory for the generated tree.                 |
| `--files`          | —       | Number of files to generate.                             |
| `--seed`           | `42`    | RNG seed.  Same seed + same files → byte-identical tree. |
| `--japanese-ratio` | `0.5`   | Fraction of files whose body is predominantly Japanese.  |
| `--max-file-size`  | `16777216` (16 MiB) | Cap applied to every size bucket, in bytes. |

A manifest file `<out>/.bench-manifest.json` is written alongside the tree.  The
manifest is **never** counted as one of the `N` generated files.  It records:

- seed, requested/actual file count, total bytes
- size histogram (10 logarithmically-spaced buckets)
- planted terms and how many files contain each

#### Size distribution

Files are assigned to five logarithmic buckets by probability:

| Percentile | Size range       | Notes                         |
|------------|------------------|-------------------------------|
| ≈50%       | 10 B – 1 KB      | Tiny configuration-like files |
| ≈30%       | 1 KB – 64 KB     | Small documents               |
| ≈15%       | 64 KB – 256 KB   | Medium documents              |
| ≈4.5%      | 256 KB – 4 MiB   | Large files                   |
| ≈0.5%      | 4 MiB – 16 MiB   | Very large files (tail)       |

This reproduces the real-machine profile observed on 2026-07-29, where 150,848 of
151,675 files (99.45%) were under 4 MiB. Within each bucket the size is
uniform-random. `--max-file-size` (default 16 MiB) caps every bucket, not only the
largest — see the disk-usage section for why lowering it to 4 MiB or below changes
what the benchmark is able to measure.

#### Content

Each file body is built from a pool of ~150 English words or ~150 Japanese
words, drawn with replacement to fill the target size.  `--japanese-ratio`
controls the fraction of files whose body uses ≥ 70 % Japanese words (the rest
English).

#### Planted query terms

A small, fixed set of known terms is inserted into a deterministic subset of
files so that search recall can be measured later without an external grep.
The manifest records **the actual count measured from the generated content**
— not the intended planting count.  This is important because several planted
terms also appear incidentally through the word-pool body generation, and the
manifest must reflect the real number of files that contain each term.

| Term          |
|---------------|
| `benchmark`   |
| `sagasu`      |
| `性能`        |
| `測定`        |
| `delta merge` |
| `全文検索`    |
| `スループット`|
| `latency`     |

Only `全文検索` is absent from the word pools, so its manifest count is purely
from deliberate planting.

#### Disk usage

At the default `--max-file-size 16 MiB`. The 10,000-file row is **measured**
(`--seed 42`); the larger rows are **extrapolated linearly** and have not been run.

| Files     | Total bytes            | On disk |
|-----------|------------------------|---------|
| 10,000    | 1,879,953,005 (measured, 1793 MiB) | 1.8 GB |
| 100,000   | ~18 GB (extrapolated)  | ~18 GB  |
| 1,000,000 | ~180 GB (extrapolated) | ~180 GB |

A 1,000,000-file tree is not practical at the default cap. Lower
`--max-file-size` to shrink it — but see the warning below before doing so for
anything that measures hashing.

**The size distribution deliberately keeps ~0.5% of files above 4 MiB** (measured:
49 of 10,000). That is not incidental. `proto-crawl` skips BLAKE3 for files larger
than its `--hash-max-size` (4 MiB by default), and on the real machine measured on
2026-07-29 exactly that 0.5% (827 of 151,675 files) took the skip path. A tree
generated with `--max-file-size 4194304` or lower has no such files at all, so it
silently measures a code path that never skips.

The synthetic tree is still far lighter than the real thing per file: ~188 KB
average here against ~2.2 MB on the machine measured (325 GiB across 151,675
files). Throughput figures in bytes/second are therefore not comparable to
real-machine numbers; the file-count proportions are what this tree reproduces.

### `bench run` — measurement harness

```
bench run [--config <toml>] --root <tree-dir> --out <results.json>
```

Runs every target defined in the TOML config file against the tree at `<root>`
and writes a JSON results file and a Markdown summary to stdout.

`--config` is optional.  When omitted, the harness uses an embedded default
config appropriate for the platform:
- **Windows**: `prototypes-windows.toml` (expects `proto-crawl` and
  `proto-fulltext` on `PATH` or in the current directory).
- **Linux / other**: `prototypes-linux.toml` (expects `./proto-crawl` and
  `./proto-fulltext` in the working directory).

Use `bench dump-default-config` to print the embedded config to stdout
(e.g. `bench dump-default-config > my-config.toml`) for customisation.

#### Config format

```toml
[[target]]
name    = "my-measurement"
command = "find"
args    = ["{root}", "-type", "f"]
repeat  = 3

[[target]]
name    = "with-setup"
command = "search"
args    = ["query", "--index", "{workdir}/index"]
repeat  = 10
setup   = { command = "build-index", args = ["{root}", "--out", "{workdir}/index"] }
```

`{root}` is replaced with the `--root` value.  `{workdir}` is replaced with a
scratch directory that the harness creates per target (and cleans between
trials, unless a `setup` is defined).  The harness never knows about any
specific prototype — targets are declared entirely in the TOML file.

#### `setup` key (optional)

A target may declare a `setup` command that runs once, **before the timed
trials, and is not timed itself**.  This is useful when a command needs an
index or database already in place before the measured command can run:

```toml
[[target]]
name = "fulltext-search"
command = "search"
args = ["query", "--index", "{workdir}/index"]
repeat = 10
setup = { command = "build-index", args = ["{root}", "--out", "{workdir}/index"] }
```

Behaviour:

- **When `setup` is present**, the work directory is **not** cleaned between
  trials — it holds whatever `setup` produced.  The same `{workdir}`
  substitution works in both `setup` and the command.
- **When `setup` is absent**, current behaviour is unchanged (the work
  directory is cleaned before each trial after the first).
- **A non-zero exit from `setup`** aborts the target: no trials run, the
  target is recorded as a setup failure in the results, and timing fields are
  `null`.  The harness does not fall through and produce meaningless timings.
- The results JSON per target includes `setup_ran` (bool) and
  `setup_succeeded` (bool).

#### What is measured

Wall-clock time from **process spawn to exit**.  Because the harness uses
`std::process::Command` and times the outer call, the measured duration
includes:

- Process startup (fork/exec)
- Dynamic linker resolution
- Kernel scheduling latency
- The command's own work

This is intentional: a real-user measurement begins when they press Enter and
ends when the prompt returns.  Treat the numbers accordingly when comparing
with profiler-internal timings.

#### Cold vs warm

- **Trial 0** is always labelled `cold`.  The first trial runs with whatever
  state the OS has (cold caches, no page cache).
- **Trials 1..N** are labelled `warm`.  No attempt is made to drop OS caches
  between trials.

#### Output

**JSON** (`--out`):  The JSON file contains per-target trial records, computed
statistics (min, max, P50, P95) split by cold/warm, and mandatory environment
metadata:

- OS, architecture, CPU count, total physical memory
- Harness version (from `Cargo.toml`)
- ISO-8601 UTC timestamp
- Resolved command line of each target
- Tree manifest info (file count, total bytes, manifest path)

A result file that cannot answer "what was measured, on what, with which
settings" is a defect — and this JSON always can.

**Stdout**:  A Markdown table in the style of the prototypes' summary blocks.

#### Failure handling

A non-zero exit code (or a failed spawn) is recorded as `failure: true` in the
trial record.  Failed trials are **excluded** from the warm/cold statistics.
If every trial of a target fails, the timing fields (`min_secs`, `max_secs`,
`p50_secs`, `p95_secs`) are `null` in the JSON and displayed as `—` in the
Markdown summary.  The failure count is always visible.

## Dependencies

The dependency set is intentionally minimal:

- `clap` – command-line parsing
- `serde` + `serde_json` – result serialisation
- `toml` – config parsing
- `rand` + `rand_chacha` – seedable deterministic RNG

No benchmarking framework (criterion) — the point is measuring external
processes.

## Portability

Everything builds and runs on both Linux and Windows.  Total physical memory
is detected on:
- **Linux**: via `/proc/meminfo`
- **Windows**: via `GlobalMemoryStatusEx`
- **Other platforms**: reported as `null` in JSON / `unknown` in the footer.

No shelling out to `sh -c`.

On Windows, `std::process::Command` searches the current directory by default;
on Linux it does not.  The prototype configs handle this difference (see below).

## Prototype measurement configs

Two pre-built configs are provided under `bench/configs/`:

| Config | Platform | Binary prefix |
|---|---|---|
| `prototypes-linux.toml` | Linux | `./proto-crawl`, `./proto-fulltext` |
| `prototypes-windows.toml` | Windows | `proto-crawl`, `proto-fulltext` |

### Targets covered

| Target | What it measures |
|---|---|
| `crawl-metadata` | `proto-crawl {root} --no-ignore --full-volume --db {workdir}/crawl.db` |
| `crawl-hash` | Same plus `--hash` |
| `fulltext-index` | `proto-fulltext index {root} --index-dir {workdir}/ft-index` |
| `fulltext-search-ja` | `proto-fulltext search 全文検索 --index-dir {workdir}/ft-index` (with setup) |
| `fulltext-search-en` | `proto-fulltext search benchmark --index-dir {workdir}/ft-index` (with setup) |

The search terms (`全文検索`, `benchmark`) are from the planted-terms set that
`bench gen` embeds into every generated tree, so queries are guaranteed to hit.

### Where the binaries must be

- **Linux (`prototypes-linux.toml`)**: expects `./proto-crawl` and
  `./proto-fulltext` in the current working directory.  Run `bench` from the
  directory containing the prototypes, or use a symlink.  (On Linux,
  `std::process::Command` does not search `.` by default.)
- **Windows (`prototypes-windows.toml`)**: expects `proto-crawl` and
  `proto-fulltext` (without `.exe` or `./`) on `PATH` or in the current
  directory.  Windows `CreateProcess` searches the current directory implicitly.

### Repeat counts

Crawl and index targets are I/O-bound and slow (seconds to minutes), so they
use `repeat = 3`.  Search targets are CPU-bound and fast (milliseconds), so
they use `repeat = 10` to give meaningful p50/p95.

### What is **not** covered

`proto-usn` is deliberately absent from both configs:

- It is Windows-only (NTFS USN journal).
- It requires administrator rights unconditionally (the manifest requests
  `requireAdministrator`).
- Its input is a USN journal number, not a file tree, so `bench gen` cannot
  produce the data it needs.

Running `proto-usn` against a synthetic tree is therefore not meaningful.

## CI integration

The `.github/workflows/prototypes.yml` workflow builds and checks `bench`
alongside the prototypes on every push and PR:

- **`check`** (Linux + Windows): builds `bench` with `cargo build --release`
  and runs `cargo clippy --all-targets -- -D warnings`.
- **`release-windows`**: builds `bench.exe` targeting
  `x86_64-pc-windows-msvc` and stages it alongside the three prototype
  binaries.  The Release attachment for every `proto-*` tag therefore
  includes four Windows executables: `proto-crawl.exe`, `proto-fulltext.exe`,
  `proto-usn.exe`, and `bench.exe`.

## Non-goals

This harness does *not*:

- Set or propose any speed target numbers.
- Include comparisons between tantivy and SQLite FTS5.
- Generate a 1,000,000-file tree as part of any smoke test.
- Measure resident memory (no persistent sagasu process exists yet).
- Run the prototype configs automatically in CI (they require actual
  prototype binaries and a file tree; building and measuring is a separate
  step on real hardware).
- Sign binaries or add code-signing steps.
