# I4 Python P0 Level A acceptance — `python-signature-impact`

Same scenario file, same fixture snapshot and same result schema as the
I0 `basic-tools` baseline, the I2 re-measurement and the I3 re-measurement.
Nothing here is estimated: an unmeasured value is `null` in the JSONL and
is named as *not measured* below, and no performance claim is made from a
single run.

- I0 baseline: `benchmarks/baselines/python-signature-impact.basic-tools.jsonl`
- I2 run: `benchmarks/baselines/python-signature-impact.brainprint-i2.jsonl`
- I3 historical: `benchmarks/baselines/python-signature-impact.brainprint-i3.jsonl`
- **I3 control, re-run at this commit**: `benchmarks/baselines/python-signature-impact.brainprint-i3-control.jsonl`
- **I4 Python**: `benchmarks/baselines/python-signature-impact.brainprint-i4-python.jsonl`

Reproduce:

```sh
cargo run -p brainprint-engine --example i3_benchmark -- \
  --scenario benchmarks/scenarios/python-signature-impact.json \
  --output benchmarks/reports/i3-control.jsonl
cargo run -p brainprint-engine --example i4_python_benchmark -- \
  --scenario benchmarks/scenarios/python-signature-impact.json \
  --output benchmarks/reports/i4-python.jsonl
cd scripts/python_semantic_spike && npm install && cd -
cargo test -p brainprint-engine --test i4_python_acceptance -- --ignored --nocapture
```

## What was tested

| | |
|---|---|
| revision under test | `bd25087` plus this task's changes |
| platform | macOS 27.0, arm64 |
| Rust | 1.98.1 |
| Node | v24.13.0 |
| backend package | `pyright-typeserver` 1.1.414 (project-local pin, never `PATH`) |
| TSP protocol | negotiated 0.4.1; compatibility class `python-pyright-tsp:0.4` |
| benchmark fixture | `fixtures/workspaces/python-signature-impact` (7 files) |
| capability fixture | `fixtures/workspaces/python-semantic-spike` (14 Resources) |

Two fixtures on purpose. The benchmark has to stay on the *same* snapshot
the I0/I2/I3 lines used or the comparison is void; the capability surface
needs traps and shapes that fixture does not contain.

## Python capability matrix

Measured on the capability fixture, through the ordinary query APIs. The
"owner" column matters: this is the *Python semantic backend's* report,
and it does not claim facts I2/I3 produce.

### Resource / Structure — owned by I2/I3, not by Pyright

| capability | support | owner | evidence | required by P0 |
|---|---|---|---|---|
| `resource_discovery` | UNSUPPORTED *by this backend* | I2 baseline scan | `the_structural_workflow_is_unchanged_with_no_backend_installed` | yes, via I2 |
| `syntax_structure` | UNSUPPORTED *by this backend* | I2/I3 parsers | same | yes, via I2 |
| `symbol_definition` | **SUPPORTED** | both | `a_definition_is_matched_by_span_and_never_by_name` | yes |
| `symbol_span` | UNSUPPORTED *by this backend* | I2 Symbol table | same | yes, via I2 |
| `containing_scope` | UNSUPPORTED *by this backend* | I2 Symbol table | same | yes, via I2 |
| `import_declaration` | UNSUPPORTED *by this backend* | I3 extraction | `absolute_relative_and_external_imports_all_normalize` | yes, via I3 |
| `export_declaration` | UNSUPPORTED *by this backend* | I3 extraction | — | Python has no export clause |
| `embedded_region_mapping` | UNSUPPORTED | — | — | no (task 11) |
| `original_source_mapping` | UNSUPPORTED | — | — | no (task 11) |

A blank here is a statement about *who owns the fact*, not about the code.
The acceptance test asserts every one of these reads UNSUPPORTED in the
Python report, so nobody can later read a structural answer as a Pyright
answer.

### Binding / Relation

| capability | support | evidence | known limitation | required by P0 |
|---|---|---|---|---|
| `import_binding` | **SUPPORTED** | `pkg/imports.py` → `pkg/base.py` Resource; `import requests` → External | — | yes |
| `alias_resolution` | **SUPPORTED** | `from .base import Base as Exported`; `wire()` reaches `Base` | — | yes |
| `reexport_resolution` | **SUPPORTED** | `uses.py` → `reexport.py` → `base.py` | one hop measured; deeper chains untested | yes |
| `references` | **SUPPORTED** | `a_reference_site_structural_resolution_could_not_follow_becomes_references` | — | yes |
| `calls_intra_file` | **SUPPORTED** | `dispatch` → `getattr` (external), `wire` → `register` | — | yes |
| `calls_cross_file` | **SUPPORTED** | `x.run(1)` → `Base.run`, exactly one of six same-name `run`s | — | yes |
| `external_symbol_resolution` | **SUPPORTED** | `abc.ABC`, `json.dumps`, `os.getenv`, `requests` | stub/source collapse to one ExternalEntity; no dependency Resource | yes |

`alias_resolution` and `reexport_resolution` were **undeclared** before
this task — they worked but read UNSUPPORTED, understating coverage on two
capabilities the P0 contract names. Declared now from the measurement
above, not from the backend's theoretical surface.

### Type / Semantic

| capability | support | evidence | known limitation | required by P0 |
|---|---|---|---|---|
| `type_resolution` | **PARTIAL** | `plain(model: Model, seed: Base)` binds both; 4 annotation gaps closed on the benchmark fixture | `getExpectedType` is not reliably answered, so an absent expected type must not read as "there is none" | yes, applicable cases only |
| `inheritance` | **PARTIAL** | plain, qualified (`base.Base`), cross-file, nested (`Outer.Inner`), multiple (`Multi`), external (`abc.ABC`) — all six resolve | a base that is neither a name nor an attribute expression (subscripted generic, call, variable) has no declaration site to anchor to and stays a gap; not exercised by the fixture | yes |
| `implements` | **UNSUPPORTED** | `relation_implements=0` with a `Protocol` and a matching `DuckTyped` in the fixture | Python has no implements clause; matching members is duck typing, not evidence | n/a — must not be manufactured |
| `overrides` | **PARTIAL** | 10 confirmed; direct, transitive, qualified-base, nested, `@classmethod`/`@staticmethod`/`@property`, unique-across-two-bases | I3 records all decorator forms as METHOD so the *kind* distinction is lost; two same-depth ancestors declaring one name is refused, not ordered by a base list the graph does not store | yes |
| `static_dispatch_target` | **PARTIAL** | `CALLS` = 3 STATIC + 4 UNKNOWN | a direct call is STATIC; a call through a typed receiver binds to the declaration the *type* names, which is not necessarily what executes, so it is UNKNOWN rather than mislabelled | yes |
| `overload_resolution` | **UNSUPPORTED** | not implemented | not claimed | no |
| `implementation_target` | **UNSUPPORTED** | Pyright's public surface offers no explicit conformance fact | — | no |

`static_dispatch_target` was also undeclared before this task and is now
PARTIAL for the measured reason.

## Python P0 Level A vertical slice: **PASS**

Against the locked P0 contract in #19 — *package/module/import alias/
relative import, definition/cross-file binding, type annotation target,
references, callers/callees within what the backend reliably provides,
inheritance, override/implementation where applicable, same-name ambiguity,
external dependency identity, related tests/impact reflecting semantic
results, current source mapping, dynamic gap, source/config freshness,
backend unavailable/crash degradation* — every case answers, and every
case outside it is an explicit gap.

Why the three PARTIALs are compatible with that contract:

- **`type_resolution`** — the contract requires *the applicable type
  annotation target*, which resolves. What stays partial is the
  *expected* type at a site, which Pyright does not reliably answer;
  rounding it up would make "no expected type known" read as "there is
  no expected type".
- **`inheritance`** — every base shape the contract names resolves. What
  stays partial is a base expression with no declaration site, where the
  only honest answer is a gap.
- **`overrides`** — every override the contract names resolves. What
  stays partial is a distinction Python writes as a decorator and the
  Symbol model does not carry, and an ambiguity the graph genuinely
  cannot decide. Both are refusals, not errors.

`implements` and `implementation_target` are UNSUPPORTED and must stay so:
manufacturing IMPLEMENTS from a matching member shape is duck typing.

## Correctness results

All assertions are in `crates/engine/tests/i4_python_acceptance.rs`
(real backend), `crates/engine/tests/python_semantic_pyright.rs` (real
backend, tasks 5–8), and the always-on suites named per row.

| # | case | result | where |
|---|---|---|---|
| 1 | package/import alias resolution | pass | i4 acceptance |
| 2 | relative import | pass | i4 acceptance, `absolute_relative_and_external_imports_all_normalize` |
| 3 | re-export chain | pass | i4 acceptance |
| 4 | exact cross-file definition | pass | `an_internal_target_resolves_through_resource_identity_and_an_exact_span` |
| 5 | same-name trap chooses exact target | pass | i4 acceptance (6 `run`s, 1 chosen) |
| 6 | parameter/return type binding | pass | i4 acceptance, `declared_and_computed_types_resolve_and_a_bare_display_name_does_not` |
| 7 | external dependency normalization | pass | i4 acceptance |
| 8 | dependency source not in Resource inventory | pass | i4 acceptance (14 Resources, none under site-packages/typeshed) |
| 9 | intra-file reference | pass | i4 acceptance |
| 10 | cross-file reference | pass | i4 acceptance |
| 11 | intra-file call | pass | i4 acceptance |
| 12 | cross-file call | pass | i4 acceptance |
| 13 | receiver-typed call | pass | i4 acceptance, task 5 integration |
| 14 | ambiguous target stays non-confirmed | pass | `several_distinct_internal_targets_stay_candidates_rather_than_a_guess` |
| 15–18 | plain / qualified / nested / external inheritance | pass | i4 acceptance |
| 19–20 | direct and transitive override | pass | i4 acceptance |
| 21 | same-depth ambiguous override refused | pass | i4 acceptance (`Multi.run` = 0) |
| 22 | unrelated same-name method refused | pass | i4 acceptance (`Other.run`, `Unrelated.run` = 0) |
| 23 | duck-typed shape produces no IMPLEMENTS | pass | i4 acceptance (`DuckTyped` vs `Runner` Protocol) |
| 24 | related tests from the enriched graph | pass | i4 benchmark success gate (1/1, `Candidates`) |
| 25 | signature-change impact uses enriched relations | pass | i4 benchmark; 3/3 callers, `missing=none` |
| 26 | base-interface impact reaches the override | pass | i4 acceptance (`Base` → `Impl`, `Base.run` → `Impl.run`) |
| 27 | preparer returns current source, not locations | pass | i4 acceptance (hard gate, below) |
| 28 | no duplicate structural+semantic logical relation | pass | i4 acceptance (group-by count = 0) |
| 29 | no duplicate occurrence | pass | i4 acceptance (group-by count = 0) |
| 30 | source change blocks a stale-current result | pass | i4 acceptance; `a_source_change_while_the_backend_thinks_rejects_the_candidate` |
| 31 | dependent owner invalidates on dependency change | pass | i4 acceptance; `an_ancestor_change_makes_the_publication_not_current` |
| 32 | config change invalidates | pass | i4 benchmark (`affected_owners=6`), `a_config_change_dirties_every_owner_in_the_context` |
| 33 | unrelated config edit does not invalidate | pass | `an_unrelated_json_or_toml_change_is_not_a_config_change` |
| 34 | inventory ADD/DELETE/MOVE invalidates correctly | pass | `adding_or_removing_a_module_reaches_every_owner_in_the_context` |
| 35 | PROVEN environment may reopen current | pass | `a_reopen_under_a_proven_unchanged_environment_restores_current_without_a_backend` |
| 36 | UNKNOWN environment cannot reopen current | pass | `a_reopen_under_an_unknown_environment_cannot_restore_current`; task 8 integration |
| 37 | live refresh under UNKNOWN may publish current | pass | `a_live_refresh_under_an_unknown_environment_still_publishes_current` |
| 38 | backend absent gives Level B, not false zero | pass | `the_structural_workflow_is_unchanged_with_no_backend_installed` (always on) |
| 39 | incompatible protocol gives Level B | pass | `an_unknown_protocol_version_degrades_instead_of_being_parsed` |
| 40 | timeout does not become empty success | pass | `a_failed_refresh_keeps_the_last_valid_publication_and_marks_the_owner` |
| 41 | crash with unchanged basis keeps valid truth | pass | `a_crash_with_an_unchanged_basis_keeps_the_publication_current` |
| 42 | crash after invalidation keeps no stale truth | pass | `a_crash_after_an_input_moved_does_not_leave_the_old_fact_current` |
| 43 | restart uses a new snapshot identity | pass | `the_backend_snapshot_never_becomes_canonical_identity`, `a_stale_snapshot_restarts_the_whole_batch_rather_than_keeping_half` |
| 44 | another worktree stays isolated | pass | `two_worktrees_of_the_same_fixture_never_share_semantic_state` (real backend) |
| 45 | repeated refresh is idempotent | pass | `a_semantic_merge_is_idempotent`, task 8 integration |
| 46 | warm query needs no broad grep | pass | benchmark `broad_searches=0` |
| 47 | warm query does not reread to rediscover identity | pass | benchmark `repeated_reads=0`, `duplicate_read_bytes=0` |
| 48 | warm prepared inspection is enough to act on | pass | 533 prepared bytes over 7 ranges |
| 49 | I3 control correctness green | pass | `brainprint-i3-control`, `missing=none` |
| 50 | I2/I3 acceptance green | pass | `cargo test --workspace --locked` |
| 51 | tasks 1–8 semantic regressions green | pass | same |
| 52 | full suite passes without Pyright | pass | re-run with `node_modules` absent |
| 53 | real-backend acceptance passes with the pin | pass | 4 `#[ignore]` tests |

### One correctness defect was found and fixed

`lifecycle::plan_changes` computed the affected owner set from the
*semantic basis* alone. A structural publication re-resolves more than
that: every Resource whose relations point into a changed one is
re-resolved against the structure that then exists, and that
re-resolution removes canonical edges. A call site I3 already resolved
structurally never becomes a semantic dependency, so the callee's
Resource is not in the caller's basis — and the caller is still
re-resolved.

Because `semantic_evidence.relation_id` deliberately has no
`ON DELETE CASCADE`, an incomplete withdrawal is not a stale row: it is a
**failed structural publication**. Saving one file in the benchmark
fixture aborted the whole reconcile with a foreign-key error. `plan_changes`
now unions in the structural dependent set
(`graph_lifecycle::dependents_of`, made public for exactly this), and
`a_structural_dependent_is_withdrawn_before_the_replacement_that_re_resolves_it`
fails without the fix. Affected owners for the benchmark's single-file
save went 1 → 4; `unrelated_owners_refreshed` stayed 0.

No other implementation change was made for a benchmark number.

## False positives, false zeros, same-name ambiguity

Counted on the capability fixture, after refreshing all 12 Python owners.

| | |
|---|---|
| confirmed relations | 84 — `USES_TYPE` 39, `IMPORTS` 15, `EXTENDS` 12, `OVERRIDES` 10, `CALLS` 7, `REFERENCES` 1 |
| expected confirmed, asserted individually | 3 EXTENDS shapes, 1 external base, 1 multiple-inheritance pair, 9 OVERRIDES, 1 receiver-typed CALLS target, 2 USES_TYPE targets, 2 import shapes |
| **unexpected / false confirmed** | **0** |
| candidates | 0 |
| conflicts | 0 |
| unresolved | 7 — `POSSIBLY_SHADOWED` 5, `MISSING_RELATIVE_TARGET` 1 (`from ..outside import thing`, which genuinely does not exist), `NOT_A_NAME_EXPRESSION` 1 |
| unresolved still requiring semantics | **0** |
| `IMPLEMENTS` rows | 0, with a `Protocol` and a structurally matching class present |
| missing expected relations | 0 |

**Same-name ambiguity.** The fixture declares six methods named `run`
(`Base`, `Impl`, `Mixin`, `Unrelated`, `Other`, `DuckTyped`). `x.run(1)`
in `pkg/impl.py` produces exactly one confirmed `CALLS`, to `Base.run`,
and the evidence is anchored on the byte span of `x.run` in the current
file — asserted by slicing the file and comparing the text. Nothing is
chosen by name. `Other.run`, `Unrelated.run`, `Multi.run`, `Orphan.run`
and `DuckTyped.run` produce zero `OVERRIDES` between them.

## External dependency boundary

- Standard library and installed packages resolve to ExternalEntity:
  `abc.ABC` (an external base), `json.dumps` (an external call),
  `os.getenv` (the call I3 could not resolve), `requests` (an import).
- Resource rows before semantic refresh: **14**. After: **14**. No
  `site-packages`, `typeshed` or `node_modules` path is a Resource, and
  no dependency source body is stored.
- No backend URI, snapshot id or machine path becomes canonical identity
  (`normalized_evidence_carries_no_backend_identity`,
  `the_backend_snapshot_never_becomes_canonical_identity`).

## Dynamic Python

`pkg/dynamic.py` writes `getattr(target, name)()` against a `Base`-typed
receiver. Measured behaviour: **one** confirmed `CALLS`, to the external
builtin `getattr` — which is a real call. The attribute the call returns
produces no relation to `Base.run` or to any other Workspace symbol. The
`NOT_A_NAME_EXPRESSION` gap in the census is the honest record of it.

No heuristic was added to reduce the unresolved count. `__getattr__`,
dynamic `importlib` import and runtime attribute injection are not
modelled and are not in the fixture; they would land in the same
unresolved/unsupported-construct classes.

## Current-source / prepared context

Hard gate, asserted rather than described: for the incoming
`CALLS`+`OVERRIDES` inspection of `Base.run`, every prepared evidence
carries an `evidence_range`, no evidence carries an `unavailable`
reason, `source_complete()` holds, and at least one range is a
`ContainingDeclaration` containing `def `.

| | capability fixture | benchmark fixture |
|---|---|---|
| prepared ranges | 15 | 7 |
| prepared source bytes | 523 | 533 |
| Resources whose source was read | — | 4 |
| results that returned a location only | **0** | **0** |

The Agent does not have to `read` or `grep` again to discover what a
referenced declaration contains.

## Freshness and degradation

| scenario | observed |
|---|---|
| source change | affected owners withdrawn → structural current, semantic DIRTY (the honest intermediate state) → refresh → semantic CURRENT. `stale_current_incidents=0` |
| dependency source change | `invalidate_resource(base.py)` marks `impl.py`'s owner non-current with its last-valid publication kept, before any refresh |
| config change | adding `pyrightconfig.json` where none governed changes which file governs: 6/6 owners invalidated, 6/6 refreshed, `stale_current_incidents=0` |
| unrelated config edit | not a config change |
| inventory ADD/DELETE/MOVE | reaches every owner in the context, because Pyright resolves modules program-wide |
| environment PROVEN + changed fingerprint | DIRTY / `SEMANTIC_ENVIRONMENT_CHANGED` |
| environment UNKNOWN | DIRTY / `SEMANTIC_ENVIRONMENT_UNPROVEN` on reopen, even with an identical fingerprint; a *live* refresh under UNKNOWN still publishes CURRENT |
| backend absent | Resources, Symbols, Occurrences, structural relations all answer; semantic questions are explicit gaps; `semantic_publication` rows = 0 |
| incompatible protocol | refused before anything starts |
| timeout | owner marked, last-valid publication kept, never an empty success |
| crash, basis unchanged | publication stays current — runtime liveness and semantic currentness are separate axes |
| crash, basis moved | not current, structural truth intact |
| restart | new process, renegotiated, new snapshot; only the owners that need it are refreshed (1 of 6 in the benchmark) |

## Worktree isolation

Two copies of the same fixture, two `WorkspaceId`s, two context keys.
The left is fully refreshed; the right has never been analyzed.

- `left.context_key() != right.context_key()`
- left `pkg/impl.py` → `Current`; right `pkg/impl.py` → `None`
- right's context has zero published owners
- reading the right index with the *left* context key finds nothing
- live runtimes: left 1, right 0

No cross-worktree current-state leak.

## Shared runtime

Three logical callers acquire the same `AnalysisContext` concurrently;
`live_runtime_count()` = **1**. Task 14 owns the full 3–5 Agent
concurrency suite — this only proves the task 2 invariant holds under the
Python vertical slice.

## Benchmark

All lines are the same scenario and the same fixture snapshot. The
comparable line excludes its own setup, exactly as the I0, I2 and I3
lines do, and the setup is published beside it rather than hidden.

| variant | tool calls | read bytes | read lines | dup bytes | elapsed ms | CPU | RSS | success |
|---|---|---|---|---|---|---|---|---|
| `basic-tools` (I0) | 6 | 1,521 | 50 | 657 | 0 | 0 | 14,163,968 | yes |
| `brainprint-i2` | 3 | 1,032 | 34 | 168 | 4 | null | null | yes |
| `brainprint-i3` (historical) | 3 | 657 | 21 | 0 | 4 | null | null | yes |
| `brainprint-i3-control` (this commit) | 3 | 657 | 21 | 0 | 5 | null | null | yes |
| `brainprint-i4-index` | 1 | 864 | 29 | 0 | 30 | null | null | yes |
| `brainprint-i4-python-start` | 1 | 0 | 0 | 0 | 1,988 | null | null | yes |
| `brainprint-i4-python-first-refresh` | 1 | 0 | 0 | 0 | 105 | null | null | yes |
| **`brainprint-i4-python`** | **3** | **657** | **21** | **0** | **4** | null | null | yes |
| `brainprint-i4-python-save` | 1 | 0 | 0 | 0 | 30 | null | null | yes |
| `brainprint-i4-python-config` | 1 | 0 | 0 | 0 | 31 | null | null | yes |
| `brainprint-i4-python-restart` | 1 | 0 | 0 | 0 | 1,749 | null | null | yes |

**The warm Agent line did not move.** Same 3 calls, same 657 bytes over
the same 4 files, same 0 duplicate bytes, same 533 prepared bytes in 7
ranges, same p50. I4 did not regress the structural path by existing.

### Timing distribution

The warm lines are 9 repetitions; the recorded `elapsed_ms` is the
median. The lifecycle lines are `samples=1` each and are reported with the
spread from three whole-harness runs, because one sample is not a
performance claim.

| variant | samples | observations (ms) | p50 | p95 |
|---|---|---|---|---|
| `brainprint-i3-control` | 9 | 3 · 3 · 3 · 3 · 4 · 4 · 4 · 4 · 4 | 4 | 4 |
| `brainprint-i4-python` | 9 | 3 · 3 · 3 · 3 · 3 · 4 · 4 · 4 · 4 | 4 | 4 |
| `brainprint-i4-index` | 3 runs | 25 · 25 · 30 | — | — |
| `brainprint-i4-python-start` | 3 runs | 1,600 · 1,643 · 1,736 | — | — |
| `brainprint-i4-python-first-refresh` | 3 runs | 88 · 95 · 96 | — | — |
| `brainprint-i4-python-save` | 3 runs | 26 · 26 · 27 | — | — |
| `brainprint-i4-python-config` | 3 runs | 27 · 28 · 32 | — | — |
| `brainprint-i4-python-restart` | 3 runs | 1,728 · 1,775 · 1,779 | — | — |

Under load (the repository's own test suite running alongside) the same
lines were observed at up to 112 ms index, 12 ms warm and 5,177 ms
restart. Nothing here scales beyond "this machine, this day".

### Does a warm Agent query wake Pyright?

**No — measured, not assumed.** The supervisor's `requests_started`
counter is sampled either side of the nine warm queries:

```
backend_requests=0
```

The whole semantic refresh of the 6-file fixture cost 19 backend
requests; the Agent-facing question that follows costs zero. A non-zero
value would have been printed and would have failed the variant.

### Lifecycle cost

| | save | config change |
|---|---|---|
| affected owners | 4 | 6 |
| refreshed owners | 4 | 6 |
| **unrelated owners refreshed** | **0** | **0** |
| backend notifications | 1 | 0 |
| backend restarts | 0 | 0 |
| backend requests | 15 | 19 |
| **stale-current incidents** | **0** | **0** |

One `didChangeWatchedFiles` batch per logical save, and no document
overlay is ever sent (`the_lifecycle_never_emits_a_document_overlay`).

### Recovery

Backend unloaded, then re-acquired: ready again in 1,749 ms, one owner
refreshed in ~116 ms, `restart_attempts=0`, `crashes=0`. During the
outage the structural query answered with 3 confirmed relations —
structural truth does not disappear when the backend does.

## Semantic accuracy delta vs I3

Same fixture, same occurrences, counted from `index.db`.

| | I3 (control) | I4 Python |
|---|---|---|
| confirmed relations | 9 — `CALLS` 3, `IMPORTS` 5, `USES_ENV` 1 | **14** — `CALLS` 4, `IMPORTS` 5, `USES_ENV` 1, `USES_TYPE` 4 |
| candidates | 0 | 0 |
| unresolved | 7 | **2** |
| unresolved requiring semantics | 1 | **0** |
| unresolved reasons | `NO_STRUCTURAL_BINDING` 4, `POSSIBLY_SHADOWED` 2, `RECEIVER_TYPE_REQUIRED` 1 | `POSSIBLY_SHADOWED` 2 |

The two gaps the I3 acceptance report named by hand are exactly the two
that closed:

- **`os.getenv`** — `RECEIVER_TYPE_REQUIRED`, the one gap I3 said needed
  I4. Now a confirmed `CALLS` to an external identity.
- **the `str` / `dict[str, str]` annotations** — four
  `NO_STRUCTURAL_BINDING` gaps, because the structural tier models no
  builtins. Now four confirmed `USES_TYPE` edges to external identities.

`POSSIBLY_SHADOWED` × 2 remains unresolved. These are references to a
name a parameter may shadow; the semantic tier did not move them, and
nothing was invented to make the number smaller.

On the capability fixture the same measurement gives
`unresolved_requires_semantics = 0` out of 7 remaining gaps.

## Agent / context economy

Observed values for the representative workflow. Nothing is derived from
bytes.

| | `basic-tools` (I0) | `brainprint-i3-control` | `brainprint-i4-python` |
|---|---|---|---|
| Agent-facing tool calls | 6 | 3 | 3 |
| Resources whose source was read | 7 (+4 re-read) | 4 | 4 |
| raw source bytes read | 1,521 | 657 | 657 |
| duplicate source bytes | 657 | 0 | 0 |
| prepared source bytes | n/a | 533 | 533 |
| prepared ranges | n/a | 7 | 7 |
| broad text searches | 1 | 0 | 0 |
| repeated reads | 4 | 0 | 0 |
| temporary helper scripts | 0 | 0 | 0 |
| backend queries caused by the Agent query | n/a | n/a | **0** |
| final projected items | 4 undifferentiated paths | 1 definition + 3 callers + 1 test | same, over 14 confirmed relations instead of 9 |

**Tokens: not measured.** No tokenizer was run. Bytes and calls are the
deterministic proxies; estimating tokens from bytes and presenting the
result as telemetry would be a fabricated number.

## Backend resource cost

| | measured |
|---|---|
| backend processes per `AnalysisContext` | **1** (three concurrent callers, one process) |
| cold ready | 1,600–1,988 ms (4 observations) |
| first owner refresh | 88–105 ms |
| warm backend requests per Agent query | 0 |
| refresh requests for the whole 6-file fixture | 19 |
| restart ready | 1,728–1,779 ms |
| restart attempts / crashes during acceptance | 0 / 0 |

Process-level figures, measured **around** the process because the
Workspace denies `unsafe_code` and `getrusage` is unreachable from a
harness example (identical to the I2/I3 lines, where these two schema
columns are `null`):

```sh
/usr/bin/time -l ./target/debug/examples/i3_benchmark        …   # macOS
/usr/bin/time -l ./target/debug/examples/i4_python_benchmark …
```

| run | wall clock | user+sys CPU | peak RSS |
|---|---|---|---|
| `i3_benchmark` (structural only) | 0.39 s | 0.04 s | 11,894,784 B |
| `i4_python_benchmark` (two backend starts) | 4.25 s | 1.41 s | 156,303,360 B |

macOS `time -l` reports the peak across the process *and its waited-for
children*, so the second row is the whole tree, not steady state. The
Pyright child alone, sampled with `ps -Ao rss=` while it was serving:
**76–86 MB** across 3 samples. Platform: macOS 27.0, arm64.

## Regressions

- **None in the structural path.** `brainprint-i3-control` matches the
  historical `brainprint-i3` line on every metric except elapsed time
  (5 ms vs 4 ms, inside the observed 3–4 ms spread of both).
- **Cold start is the cost I4 adds**: ~1.6–2.0 s before any semantic
  answer exists, plus ~0.1 s per owner refresh, plus ~76–86 MB of Node.
  It is published as its own variant rather than folded into the warm
  line. No optimization was attempted — Task 9 is measurement, and no
  contract failure was demonstrated here.
- **`plan_changes` now does more work per change batch** (one extra
  graph query, and a larger affected set). That is the correctness fix
  above, not a regression to tune away.

## Not measured

- Tokens. No tokenizer was run.
- `process_cpu_ms` / `peak_rss_bytes` inside the Rust harness — `null` in
  the JSONL, measured around the process instead.
- Steady-state Pyright RSS over a long session; the 76–86 MB figure is
  three `ps` samples during one run.
- Scaling. One 7-file benchmark fixture and one 14-Resource capability
  fixture on one machine. Nothing here says how any of it scales.
- Multi-Agent concurrency beyond the three-caller shape check — task 14.
- `extraPaths` and upper-directory config traversal; config discovery
  still looks only at the two files in the project root.

## Remaining Python limitations

- `getExpectedType` is not reliably answered, so `type_resolution` stays
  PARTIAL and an absent expected type must never read as "none".
- A base expression that is neither a name nor an attribute expression
  has no declaration site to anchor to and stays a gap.
- Decorator-carried member kinds (`@classmethod`, `@staticmethod`,
  `@property`) all record as METHOD, so an override edge exists but the
  kind distinction does not.
- Two same-depth ancestors declaring one name is refused rather than
  resolved by C3/MRO; the graph does not store base-list order, and
  inferring it would be a guess.
- Overload resolution is not implemented.
- `IMPLEMENTS` and `implementation_target` are UNSUPPORTED and should
  stay so unless Pyright exposes an explicit conformance fact.
- Dynamic Python (`getattr`, `__getattr__`, dynamic import, monkey
  patching) is not modelled; it produces gaps, never edges.
- `POSSIBLY_SHADOWED` references are not resolved by the semantic tier.
- The environment observer is layout-based and conservative: a real venv
  with an `import`-line `.pth` reads UNKNOWN, which means its persisted
  publications need a refresh on every daemon reopen.

## Design conflicts discovered

One, fixed above: the semantic affected-owner set and the structural
re-resolution set were computed independently, and the smaller one was
being used to satisfy an ordering contract the larger one defines. They
are now the same question asked once.

No conflict with the task 3 currentness model, the task 8 owner
granularity, the withdraw ordering contract, or the dependency
deep-index boundary.
