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

# Run the smoke-test config
./bench/target/release/bench run \
    --config bench/configs/smoke.toml \
    --root /tmp/bt-a \
    --out /tmp/bench-result.json
```

## Subcommands

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
bench run --config <toml> --root <tree-dir> --out <results.json>
```

Runs every target defined in the TOML config file against the tree at `<root>`
and writes a JSON results file and a Markdown summary to stdout.

#### Config format

```toml
[[target]]
name    = "my-measurement"
command = "find"
args    = ["{root}", "-type", "f"]
repeat  = 3
```

`{root}` is replaced with the `--root` value.  `{workdir}` is replaced with a
scratch directory that the harness creates per target (and cleans between
trials).  The harness never knows about any specific prototype — targets are
declared entirely in the TOML file.

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

Everything builds and runs on both Linux and Windows.  Memory detection is
currently Linux-only (`/proc/meminfo`); other platforms report 0 for memory
total.  No shelling out to `sh -c`.

## Non-goals

This harness does *not*:

- Set or propose any speed target numbers.
- Include comparisons between tantivy and SQLite FTS5.
- Generate a 1,000,000-file tree as part of any smoke test.
- Measure resident memory (no persistent sagasu process exists yet).
- Wire itself to the prototypes (the config format makes it possible; doing so
  is a separate step).
- Integrate with CI.
