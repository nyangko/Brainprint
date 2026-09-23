# Task 15 acceptance ledger — I4 final acceptance and closure

#19 task 15. Base `159c7ea`.

All 80 cases the task listed, each with a verdict and the evidence that
carries it. Where a verdict is not PASS the reason is written out. No
case was dropped for being inconvenient.

The measurements themselves are in
`benchmarks/i4-final-acceptance-report.md`; this ledger says whether
each required thing was accounted for, and points at where.

Harness and artifacts added by this task:

```text
crates/engine/examples/i4_final_benchmark.rs   the five-family measurement
scripts/i4_final_acceptance/measure_rss.py     external process-tree RSS
benchmarks/i4-final-acceptance-report.md       the report
benchmarks/reports/i4-final.jsonl              machine-readable observations
benchmarks/reports/i4-final-rss.json           machine-readable resource sample
benchmarks/reports/i4-python-task15.jsonl      the historical scenario, re-run
```

Nothing under `benchmarks/baselines/` was touched. The historical I0/I2/I3
lines are preserved as historical observations.

---

## Cross-language

| # | case | verdict | evidence |
|---|---|---|---|
| 1 | Python final real acceptance passes | PASS | `i4_python_acceptance` 2, `python_semantic_pyright` 2 |
| 2 | TypeScript final real acceptance passes | PASS | `i4_typescript_acceptance` 3, `typescript_semantic_lsp` 11 |
| 3 | JavaScript accepted capability cases pass | PASS | inside `i4_typescript_acceptance`; the `js/` fixtures are measured separately and their UNSUPPORTED verdicts (`type_resolution`, `inheritance`) are unchanged |
| 4 | React TSX acceptance passes with zero additional backend | PASS | `i4_typescript_acceptance`; benchmark line `i4-final-react` records `additional_backend_count=0`, and the TSX anchor `UserCard` was answered by the TypeScript process |
| 5 | Svelte final real acceptance passes | PASS | `i4_svelte_acceptance` 3 |
| 6 | C# final real acceptance passes | PASS | `i4_csharp_acceptance` 13 |
| 7 | Rust final real acceptance passes | PASS | `i4_rust_acceptance` 13 |
| 8 | capability matrices unchanged unless a real bug required it | PASS | no matrix was edited in task 15; no correctness bug was found that required one |
| 9 | every remaining PARTIAL/UNSUPPORTED limitation stays explicit | PASS | report §2.2 and §13 restate them per family and point back to the task 9–13 ledgers |

## Canonical correctness

| # | case | verdict | evidence |
|---|---|---|---|
| 10 | semantic results normalize into Brainprint canonical identities | PASS | every benchmark answer was read back through `QueryIndex` / `RelationIndex` / `InspectPreparer` |
| 11 | backend internal IDs do not leak | PASS | report §2.4 — `GraphEndpoint` has no variant that can carry one, so it is a compile-level property; each family also asserts the negative |
| 12 | structural truth survives backend absence | PASS | `cargo test --workspace --locked` 1168 passed with nothing installed; `brainprint-i4-python-restart` answered 3 confirmed structural relations *during* the outage |
| 13 | structural + semantic logical relations are not duplicated | PASS | `relation_rows` equals `confirmed` on all five families; per-family merge acceptance covers the corroboration path |
| 14 | dependency trees are not deep-indexed | PASS | report §2.5 — the Resource inventory after a full refresh is the fixture's own files only (12/24/6/15/9) |
| 15 | external dependencies use ExternalEntity / canonical external identity | PASS | Python packages, `@types/node`, Svelte runes, .NET assemblies, Rust crate+version — never a machine path |
| 16 | generated/virtual source is not exposed as editable Workspace source | PASS | report §2.6 |
| 17 | Svelte exact original mapping remains correct | PASS | `i4_svelte_acceptance`: 29/29 original spans, 0 generated |
| 18 | C# LogicalSymbol partial declarations remain correct | PASS | `i4_csharp_acceptance` |
| 19 | Rust trait IMPLEMENTS remains correct without fake inheritance/override | PASS | `i4_rust_acceptance`; `inheritance`/`overrides`/`overload_resolution` stay UNSUPPORTED |
| 20 | React remains TS/JS adapter semantics only | PASS | no React backend kind exists; hook/route/component-ownership remain deliberately UNSUPPORTED |

## Freshness

| # | case | verdict | evidence |
|---|---|---|---|
| 21 | Python source change produces no stale-current | PASS | 10 transitions, `stale_current_incidents=0` |
| 22 | TS/JS source and config change produce no stale-current | PASS | 10 save + 10 config transitions, 0 |
| 23 | Svelte source change produces no stale-current | PASS | 10 save transitions, 0. Config → current is `not measured`: the container's config basis comes from its toolchain rather than one movable Workspace file (report §4.5); `i4_svelte_acceptance` covers config handling |
| 24 | C# document and project change produce no stale-current | PASS | 10 save + 3 project-reload transitions, 0 |
| 25 | Rust source and Cargo change produce no stale-current | PASS | 10 save + 3 manifest-reload transitions, 0 |
| 26 | obsolete publication is rejected | PASS | `an_answer_about_an_older_revision_cannot_publish_over_a_newer_one` (task 14, always-on) |
| 27 | cold persisted CURRENT query does not force a backend wake | PASS | every `-warm` line: `backend_processes=0`, 20 samples each, 2–3 ms |
| 28 | backend unavailable does not convert stale truth to CURRENT | PASS | `a_crash_cannot_make_invalidated_truth_current_again` |

**Total measured freshness transitions: 43 source + config across five
families, 0 stale-current incidents.**

## Runtime

| # | case | verdict | evidence |
|---|---|---|---|
| 29 | same-context 3–5 clients start one backend | PASS | real fleet: 10 clients, 5 contexts, 5 starts, `duplicate_starts=0` |
| 30 | five backend families coexist | PASS | `live_runtimes=5`, all READY |
| 31 | React starts no sixth backend | PASS | `react_additional_backends=0` |
| 32 | worktree state stays isolated | PASS | `i4_fleet_acceptance` — context key, runtime entry, dedupe key, publication, crash and `LogicalIdentity` all diverge on `WorkspaceId` alone |
| 33 | request dedupe remains correct | PASS | task 14 runtime suite, 55 tests |
| 34 | capacity/runtime state does not alter capability truth | PASS | no capability verdict moved; report §2.2 states the axes separately |
| 35 | backend failure remains context-local | PASS | `a_crash_in_one_worktree_is_invisible_in_the_other`, `a_degraded_context_stops_trying_and_leaves_the_rest_of_the_fleet_alone` |

## Benchmark completeness

| # | case | verdict | evidence |
|---|---|---|---|
| 36 | exact environment recorded | PASS | report §1: commit, OS, kernel, CPU, RAM, filesystem, every backend version, `rust-src` presence, fixture sizes, commands |
| 37 | cold startup measured per real backend | PASS | n = 5 per family, report §4.1 |
| 38 | first semantic query measured | PASS | report §4.2 |
| 39 | warm semantic query measured | PASS | n = 20 per family, report §4.3 |
| 40 | source save → current measured | PASS | n = 10 per family, report §4.4 |
| 41 | config/project change → current measured | PARTIAL | measured for TS/JS and Python (n = 10) and C# and Rust (n = 3, reason recorded: a solution/Cargo reload measures MSBuild and cargo more than Brainprint). Svelte is `not measured` with its reason stated — case 23 |
| 42 | sample count recorded | PASS | every line carries `..._samples=` |
| 43 | p50 recorded where the sample count allows | PASS | every distribution |
| 44 | p95 recorded where the sample count allows | PASS | reported for the n = 20 warm lines; every n < 20 line prints `not_measured(samples<20)` rather than dressing the maximum up as a percentile |
| 45 | RSS recorded or explicitly unknown | PASS | measured externally: 560.8 MB max simultaneous over 5 processes, per-process breakdown in report §5.1. `ResourceUsage` inside the engine stays `None` and is never read as zero |
| 46 | CPU recorded or explicitly unknown | N/A → recorded as **unknown** | report §5.2: unreachable without `unsafe_code` in-process, and no reproducible cross-platform external reader was built, because that is not I4 semantic correctness |
| 47 | process-tree scope recorded for resource metrics | PASS | report §5.1 and the script's own `scope` field: whole fixture-owned tree by descent from the harness pid, never by process name |
| 48 | DB/index size recorded for representative fixtures | PASS | report §5.3, with the zero deltas explained and no scaling claim made |
| 49 | backend startup/restart count recorded | PASS | `backend_starts` / `backend_restarts` on every `-cold` line; `restarts=0`, `crashes=0` on the fleet line |
| 50 | unresolved → resolved delta recorded | PASS | report §3, per family, as counts |
| 51 | conflict count recorded with its denominator | PASS | 0 conflicts over 66 owners refreshed by live backends, all five families installed — stated as such rather than as a bare zero |
| 52 | false-positive relation count recorded | PASS | 0; the per-language suites assert exact target sets and the fixtures carry same-name traps on purpose |
| 53 | false-zero count recorded | PASS | 0; each `-warm` line records its anchor and reference count, and the harness will not measure an anchor with no incoming edges when one with edges exists |
| 54 | stale-current count recorded | PASS | 0 over 43 transitions |
| 55 | raw source bytes recorded | PASS | report §7 (`whole_file_bytes_avoided` is derived from it) and §9 (657 bytes) |
| 56 | prepared source bytes recorded | PASS | report §7 and §9 |
| 57 | duplicate source bytes recorded | PASS | 0 on every family and on the historical scenario |
| 58 | query/tool operation count recorded | PASS | 3 calls per family; 3 on the historical scenario |
| 59 | unavailable values remain unknown / not measured | PASS | report §5.4 lists all six, and the JSONL carries `null` for `process_cpu_ms` and `peak_rss_bytes` on every line |
| 60 | no counterfactual savings presented as facts | PASS | the only comparison made is §9, which is paired observations in one schema. No "X% faster", no projected token saving |

## Historical comparison

| # | case | verdict | evidence |
|---|---|---|---|
| 61 | I3 historical baseline preserved | PASS | `benchmarks/baselines/` untouched; new runs wrote to `benchmarks/reports/` |
| 62 | I4 Python representative workflow compared where the schema is compatible | PASS | report §9: same scenario file, same fixture, same JSONL schema |
| 63 | incompatible schema differences stated rather than normalized away | PASS | report §9.4 states that §9.1's timings came from earlier machines and are not comparable, and that §4's tables use a different harness and different fixtures and are not comparable with §9 at all |
| 64 | semantic accuracy gain and added runtime cost both shown | PASS | §9.3 is the gain, §9.4 is the cost, in separate tables |
| 65 | no single "Brainprint score" hides tradeoffs | PASS | there is no composite score anywhere in the report |

## Regression

| # | case | verdict | evidence |
|---|---|---|---|
| 66 | `cargo fmt --check` passes | PASS | |
| 67 | `cargo clippy --workspace --all-targets -- -D warnings` passes | PASS | |
| 68 | `cargo test --workspace --locked` passes | PASS | 1168 passed, 0 failed, 50 ignored |
| 69 | `cargo build --workspace --locked` passes | PASS | |
| 70 | I3 structural acceptance remains green | PASS | `i3_acceptance` 20 passed; `i2_acceptance` 7 passed |
| 71 | Task 14 fleet acceptance remains green | PASS | 15 always-on + 3 real |
| 72 | all available real backend suites pass | PASS | table in report §2.1 |
| 73 | the ordinary suite stays backend/toolchain independent | PASS | the 1168 passing tests need no external toolchain; the 50 real-backend tests are `#[ignore]` and skip with a printed reason. Normal CI installs no language server |

## Roadmap boundary

| # | case | verdict | evidence |
|---|---|---|---|
| 74 | I5 functionality was not pulled in | PASS | no `delivered_tokens`, no delivery budget, no continuation, no projection planner, no client shaping. Report §7 measures only raw / prepared / duplicate bytes through the existing `InspectPreparer` |
| 75 | I6 functionality was not pulled in | PASS | no git/test/build command wrappers, no bounded exec, no diagnostics surface. Report §13 claims a *semantic/code-intelligence layer*, not a standalone coding workflow |
| 76 | I7 real Agent A/B is explicitly deferred | PASS | report §12, with the reason: comparing mature Agent tooling against an engine-only API would be an unfair comparison rather than a measurement |
| 77 | no new public backend-specific Agent API introduced for benchmarking | PASS | the harness is an `examples/` binary using the existing public surfaces; `brainprint-engine` gained nothing in task 15 |
| 78 | no benchmark-only behaviour changes canonical production truth | PASS | the harness copies each fixture to `$TMPDIR` and touches nothing in the repository. `dotnet restore` inside the C# copy is developer tooling the C# acceptance already does, and production never runs it |
| 79 | no arbitrary memory or latency threshold invented | PASS | report §10: `max_live_runtimes` stays `None` and `idle_timeout` is unchanged, both with the reason written down |
| 80 | the report states what I4 proves and what it does not | PASS | report §13, final two paragraphs |

---

## Verdict

**79 PASS, 1 PARTIAL (case 41), 0 FAIL.**

Case 41 is partial because one of five families has no single Workspace
file whose change constitutes a config change, and because two families'
project reloads were measured three times rather than ten. Both reasons
are recorded at the point of measurement, and neither is a correctness
gap: every family's freshness contract is proved by case 21–25 and by
its own acceptance suite.

Every I4 hard correctness gate passed. No stale-current incident, no
false zero in a supported case, no worktree contamination, no dependency
deep index, no generated-source identity leak, and same-context
multi-client backend reuse still holds against real processes.

**I4 is complete.** What it costs is recorded; what it does not yet
prove is named and assigned.
