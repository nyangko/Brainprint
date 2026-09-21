# Brainprint Benchmarks

I0 separates benchmark inputs, accepted baselines, and generated run output.

## Layout

- `scenarios/` — versioned, human-readable scenario definitions.
- `baselines/` — promoted baseline results that are intentionally kept for comparison.
- `reports/` — generated local/CI JSONL runs. JSONL files are ignored by Git.

A scenario is reusable across variants. The same workspace snapshot, query, and acceptance set must be used when comparing a basic-tools baseline with a future Brainprint-backed run.

## Result schema

Each JSONL line is one run:

- `schema_version`
- `run_id`
- `scenario_id`
- `variant`
- `build` — Brainprint build identity, or `null` for a non-Brainprint baseline
- `started_at_unix_ms`, `ended_at_unix_ms`, `elapsed_ms`
- `metrics.tool_calls`
- `metrics.source_read_bytes`
- `metrics.source_read_lines`
- `metrics.duplicate_read_bytes`
- `metrics.process_cpu_ms`
- `metrics.peak_rss_bytes`
- `success`, `failure_kind`, `notes`

An unavailable measurement is `null`. A measured zero is `0`.

## Promotion

Generated reports are not automatically baselines. A result is copied into `baselines/` only after its workspace snapshot and acceptance criteria are considered representative. No performance claim is made from a single run.
