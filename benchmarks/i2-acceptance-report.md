# I2 acceptance re-measurement — `python-signature-impact`

Same scenario file, same fixture snapshot, same result schema as the I0
`basic-tools` baseline (#16 task 15). Nothing here is estimated: a value
that was not measured is `null` in the JSONL, and no performance claim is
made from elapsed time or RSS on a single run.

- I0 baseline: `benchmarks/baselines/python-signature-impact.basic-tools.jsonl`
- I2 run: `benchmarks/baselines/python-signature-impact.brainprint-i2.jsonl`
- Reproduce the I2 run:
  `cargo run -p brainprint-engine --example i2_benchmark -- --scenario benchmarks/scenarios/python-signature-impact.json --output benchmarks/reports/i2.jsonl`
- Reproduce the baseline:
  `python3 scripts/benchmark_baseline.py --scenario benchmarks/scenarios/python-signature-impact.json --output benchmarks/reports/ci.jsonl`

## Result

| variant | tool calls | source read bytes | source read lines | duplicate read bytes | CPU ms | peak RSS | success |
|---|---|---|---|---|---|---|---|
| `basic-tools` (I0 baseline) | 6 | 1,521 | 50 | 657 | 0 | 14,163,968 | yes |
| `brainprint-i2-structured` | 2 | 168 | 5 | 0 | null | null | yes |
| `brainprint-i2-text-fallback` | 1 | 864 | 29 | 0 | null | null | yes |
| `brainprint-i2` (both) | 3 | 1,032 | 34 | 168 | null | null | yes |

Indexing is setup, not part of the measured flow — the baseline's six
calls are the *lookup*, and so are I2's three.

## What each line means

**`brainprint-i2-structured` — what I2 owns.** Locating `build_profile`
and reading its definition takes two calls: one structured Symbol search
against `index.db`, and one `inspect` that returns the declaration's
metadata *and* its current source in the same packet. No `ls`, `find`,
`rg`, `cat` or `sed`; no broad text search; no second read of the file to
get the body. 168 bytes are read — the one file the definition lives in,
hash-verified before its span is sliced (#16 task 11).

**`brainprint-i2-text-fallback` — evidence, not relations.** The call
sites and the test are found by one bounded current-filesystem search.
They are *textual* matches. This deliberately does not claim a CALLS
edge or a "related test" relation: deciding what a call expression
resolves to is I3's canonical Relation graph, and the same fixture is
meant to be measured again there against that stronger claim.

**`brainprint-i2` — comparable with the baseline.** Three calls instead
of six, 1,032 bytes instead of 1,521, and 168 duplicate bytes instead of
657. The remaining duplicate is the definition file, which the structured
read verifies and the fallback then scans; the remaining bytes are the
fallback's single pass over the workspace, which exists precisely because
I2 does not model call sites.

All four expected paths are found with nothing missing:
`src/profile_app/profile.py` (definition, structured),
`src/profile_app/service.py`, `src/profile_app/admin.py`, and
`tests/test_profile.py` (textual evidence).

## How the numbers were obtained

- `tool_calls` — counted directly: the API calls the run makes.
- `source_read_bytes` — for the structured path, the size of the one file
  `inspect` read, taken from the filesystem; for the fallback,
  `ScopeReport::bytes_scanned`, which the search itself counts.
- `source_read_lines` — recounted by the harness over exactly the files
  the run read. That recount is measurement, not part of the metric.
- `duplicate_read_bytes` — the overlap between the two paths.
- `process_cpu_ms` / `peak_rss_bytes` — not measured by this harness, so
  recorded as `null` rather than guessed.
