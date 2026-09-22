# I3 acceptance re-measurement — `python-signature-impact`

Same scenario file, same fixture snapshot, same result schema as the I0
`basic-tools` baseline and the I2 re-measurement (#17 task 15). Nothing
here is estimated: a value that was not measured is `null` in the JSONL,
and no performance claim is made from elapsed time on a single run.

- I0 baseline: `benchmarks/baselines/python-signature-impact.basic-tools.jsonl`
- I2 run: `benchmarks/baselines/python-signature-impact.brainprint-i2.jsonl`
- I3 run: `benchmarks/baselines/python-signature-impact.brainprint-i3.jsonl`
- Reproduce the I3 run:
  `cargo run -p brainprint-engine --example i3_benchmark -- --scenario benchmarks/scenarios/python-signature-impact.json --output benchmarks/reports/i3.jsonl`

## Result

| variant | tool calls | source read bytes | source read lines | duplicate read bytes | elapsed ms | CPU ms | peak RSS | success |
|---|---|---|---|---|---|---|---|---|
| `basic-tools` (I0 baseline) | 6 | 1,521 | 50 | 657 | 0 | 0 | 14,163,968 | yes |
| `brainprint-i2` | 3 | 1,032 | 34 | 168 | 4 | null | null | yes |
| `brainprint-i3-index` | 1 | 864 | 29 | 0 | 21 | null | null | yes |
| **`brainprint-i3`** | **3** | **657** | **21** | **0** | **4** | null | null | yes |
| `brainprint-i3-cold` (index + query) | 4 | 1,521 | 50 | 657 | 25 | null | null | yes |

As in I0 and I2, the comparable line excludes its own setup: the
baseline's six calls are the *lookup*, and so are I3's three.
`brainprint-i3-index` and `brainprint-i3-cold` are published next to it
so the cost I3 adds is visible rather than hidden.

### Process-level figures

`process_cpu_ms` and `peak_rss_bytes` are `null` in the Rust harness
lines, exactly as they are for `brainprint-i2`: the Workspace denies
`unsafe_code`, so `getrusage` — what the I0 Python baseline calls for
these two columns — is not reachable from the harness, and a guess would
be worse than a recorded `null`. Measured around the process instead, on
one macOS run of the release-less debug binaries (includes process
start-up and the fixture copy, so it is an upper bound, not a hot cost):

| run | wall clock | user+sys CPU | peak RSS |
|---|---|---|---|
| `scripts/benchmark_baseline.py` (I0) | 0.52 s | 0.06 s | 21,479,424 B |
| `i2_benchmark` | 0.72 s | 0.04 s | 12,812,288 B |
| `i3_benchmark` (index + query + impact) | 0.63 s | 0.02 s | 12,025,856 B |

Reproduce with `/usr/bin/time -l ./target/debug/examples/i3_benchmark …`
on macOS, or `/usr/bin/time -v` with GNU coreutils.

## Agent cost avoided vs Brainprint cost added

**Avoided.** The baseline reads all 7 workspace files, then re-reads each
of the 4 hits — 1,521 bytes across 6 calls, of which 657 bytes are
duplicate. I3 reads 657 bytes across 3 calls with **0 duplicate bytes**
and **0 broad searches**, and hands back 533 bytes of prepared current
source in 7 ranges. The deterministic proxies for what the Agent no
longer has to do:

| proxy | `basic-tools` | `brainprint-i3` |
|---|---|---|
| files that must be read | 7 (+4 re-read) | 4 |
| raw bytes | 1,521 | 657 |
| duplicate bytes | 657 | 0 |
| broad text searches | 1 | 0 |
| bytes handed to the Agent | 1,521 (raw files) | 533 (prepared ranges) |
| relation rows before projection | n/a | 3 confirmed |
| final projected items | 4 undifferentiated paths | 1 definition + 3 callers + 1 related test |

**Added.** One baseline scan: 1 pass, 864 bytes, 7 files, 21 ms, and an
`index.db` that is then reused by every later query. Cold start is
therefore 4 calls / 1,521 bytes / 25 ms — the same byte count as the
baseline's single lookup, for a durable index plus a strictly stronger
answer. Every query after the first costs the 3-call line.

## What the answers actually are

This is the part the byte counts do not show. The baseline's four
"matches" are textual: nothing in that result says which file holds the
definition, which hold callers, or which is a test. I3 returns:

- `build_profile` located by stable `SymbolId`, with its current
  declaration source prepared (`def build_profile(...)`, 130 B);
- three **confirmed** `CALLS` relations, each with the exact evidence
  span and the declaration it sits in, prepared as current source;
- `tests/test_profile.py` as a related test, projected from a confirmed
  one-hop relation path and `ResourceRole::TEST` — not from its name;
- the coverage state behind all of it.

## Correctness gates

Performance numbers are only meaningful behind these, all of which the
I3 acceptance suite (`crates/engine/tests/i3_acceptance.rs`) asserts:

- Symbol recall: `build_profile` located from the structural index.
- Caller/importer recall: 3/3 confirmed callers, 3/3 importers of
  `profile.py`, all four expected paths accounted for (`missing=none`).
- Related-test recall: 1/1, through graph evidence.
- **No false confirmed relation**: the published graph for this
  Workspace is exactly 9 relations (3 CALLS, 5 IMPORTS, 1 USES_ENV),
  asserted line by line, over 9 bound Occurrences.
- Gaps stay gaps: 7 unresolved references (`str` annotations,
  shadowed parameter references, `os.getenv`'s receiver), 0 candidates
  invented, 0 promoted.
- Stale context errors: 0 in the tested lifecycle scenarios.
- The false-zero matrix passes through the integration surfaces.

## Measured limitations

- One run, one machine, one 7-file fixture. Nothing here says how the
  index scales; the scan is linear in files and this fixture is far too
  small to show a knee.
- `process_cpu_ms` / `peak_rss_bytes` stay `null` inside the harness for
  the reason above.
- The Python type annotations (`str`, `dict[str, str]`) are unresolved
  gaps: this tier models no builtins. That is honest coverage, and it
  means the definition's *outgoing* answer is `NoneWithIncompleteCoverage`
  rather than a clean zero.
- `os.getenv`'s callee stays unresolved (`RECEIVER_TYPE_REQUIRED`) even
  though the `USES_ENV` relation beside it is confirmed. Resolving it
  needs I4.
