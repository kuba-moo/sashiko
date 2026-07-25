# Benchmarking

Sashiko includes a benchmark tool to evaluate AI review performance
against known bugs. It ingests patches, waits for reviews to complete,
then uses an AI judge to compare findings against ground-truth
descriptions.

## Prerequisites

- A running sashiko daemon with a configured LLM provider
- A clean database (move or remove any existing `sashiko.db`)
- A benchmark JSON file (several are provided in `benchmarks/`) or a corpus
  directory

## Quick start

```bash
# Start with a clean database
mv sashiko.db sashiko.db.bak

# Run the benchmark
cargo run --bin benchmark -- --file benchmarks/benchmark_small.json
```

For an annotated patch corpus:

```bash
cargo run --bin benchmark -- --corpus /path/to/corpus
```

## Benchmark files

| File | Description |
|------|-------------|
| `benchmarks/benchmark_tiny.json` | Minimal set for quick smoke tests. |
| `benchmarks/benchmark_small.json` | Small set for development iteration. |
| `benchmarks/benchmark.json` | Full benchmark suite. |
| `benchmarks/benchmark_preexisting.json` | Tests detection of pre-existing bugs. |
| `benchmarks/benchmark_smoke.json` | CI smoke test set. |

Each file contains entries with a commit hash, a `Fixed-by` reference,
and a `problem_description` that the AI judge uses to evaluate whether
sashiko detected the issue.

## Corpus directory mode

Corpus mode evaluates patch-based cases with one or more ground-truth
annotations. The corpus directory contains one subdirectory per case. Each case
provides an mbox, JSON metadata identifying the patch file and optional base
commit, and annotation files describing the expected issues. The metadata's
`test_patch` selects the message to evaluate when the mbox contains a series.

The benchmark submits each mbox through the Inject API with deterministic
Message-ID headers. Its structured LLM judge evaluates in both directions:
each annotation is classified as `DETECTED`, `PARTIALLY_DETECTED`, or `MISSED`,
and findings that match no annotation are reported as false positives. Results
are written to `corpus_results.json`.

## Command-line options

```
cargo run --bin benchmark -- [OPTIONS]
```

| Flag | Description |
|------|-------------|
| `-f, --file <PATH>` | Path to a legacy benchmark JSON file. |
| `-c, --corpus <PATH>` | Path to an annotated corpus directory. |
| `-p, --port <PORT>` | Override the daemon port (defaults to Settings.toml value). |
| `-r, --repo <URL>` | Override the kernel repository URL (`--file` only). |
| `--analyze-only` | Skip ingestion; only evaluate existing results in the database. |

Exactly one of `--file` and `--corpus` is required.

## Output

The tool prints a summary to the console:

- **Detection rates**: Detected, Missed, Partially Detected
- **Performance metrics**: Average tokens in/out, average turns,
  average time per review
- **Counts**: Total concerns and findings

Detailed results are written to `benchmark_results.json` for legacy mode or
`corpus_results.json` for corpus mode, including the AI judge's explanations.

## Re-evaluating existing results

If you have already run ingestion and reviews but want to re-score with
updated evaluation logic:

```bash
cargo run --bin benchmark -- --file benchmarks/benchmark_small.json --analyze-only
```

This skips patch submission and review, reading results directly from the
database.
