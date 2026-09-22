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

## Variants

- `basic-tools` — the I0 no-Brainprint filesystem/text baseline
  (`scripts/benchmark_baseline.py`).
- `brainprint-i2-structured` / `brainprint-i2-text-fallback` /
  `brainprint-i2` — the I2 re-measurement of the same scenario
  (`cargo run -p brainprint-engine --example i2_benchmark`). The
  structured and text-fallback lines are kept apart on purpose: a text
  match is not a Relation, and only I3 may claim it is. See
  `i2-acceptance-report.md`.
- `brainprint-i3-index` / `brainprint-i3` / `brainprint-i3-cold` — the
  I3 Relation Graph re-measurement of the same scenario
  (`cargo run -p brainprint-engine --example i3_benchmark`). The index
  line is the cost I3 adds, the plain line is the query flow comparable
  with the two above, and the cold line is both together. See
  `i3-acceptance-report.md`.
- `brainprint-i3-control` / `brainprint-i4-index` /
  `brainprint-i4-python-start` / `brainprint-i4-python-first-refresh` /
  `brainprint-i4-python` / `brainprint-i4-python-save` /
  `brainprint-i4-python-config` / `brainprint-i4-python-restart` — the
  I4 Python semantic re-measurement of the same scenario
  (`cargo run -p brainprint-engine --example i4_python_benchmark`). The
  control line is the I3 query flow re-run at the current commit; the
  `-python` line is the same Agent-facing flow with semantics current,
  and is the one comparable with everything above. Everything else is
  cost the semantic tier adds, kept in its own line rather than folded
  in: process start, the first refresh, and each lifecycle transition.
  Without the pinned Pyright install the semantic variants are skipped
  and the control still runs. See `i4-python-acceptance-report.md`.

Metrics the result schema has no column for — prepared source bytes,
relation query count, traversal size, confirmed/gap counts, broad
searches avoided — are recorded as `key=value` pairs in `notes`, which
is where the existing lines already carry scenario detail.

`process_cpu_ms` and `peak_rss_bytes` are `null` on the Rust harness
lines: the Workspace denies `unsafe_code`, so `getrusage` (what the
Python baseline uses for those two columns) is not reachable from a
harness example. Measure them around the process instead:
`/usr/bin/time -l ./target/debug/examples/i3_benchmark ...` on macOS, or
`/usr/bin/time -v` with GNU coreutils. A backend child process is inside
that figure on macOS but is not a steady-state reading; sample it
separately (`ps -Ao rss=`) if that is what is wanted.

## Promotion

Generated reports are not automatically baselines. A result is copied into `baselines/` only after its workspace snapshot and acceptance criteria are considered representative. No performance claim is made from a single run.
