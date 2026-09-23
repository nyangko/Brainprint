# I4 final acceptance and benchmark

#19 task 15. The last I4 task: re-run every language's acceptance
unchanged, measure what the semantic tier costs on this machine, compare
what it proves against what I3 proved, and close I4.

Everything below is an observation or something derived from one by
arithmetic that is written out. Where a value was not measured it says
`not measured` and why. No number in this document is an estimate, a
projection, or a counterfactual.

**What this report closes:** I4. Not the #10 P0 benchmark, which needs
surfaces I5 and I6 have not built yet, and not the real-project Agent
A/B, which #12 assigns to I7. Section *Deferred* says what is missing
and who owns it.

---

## 1. Environment

Every figure in this report came from one machine on one day. Nothing
here is comparable with a measurement from a different machine, and the
historical I0/I2/I3 lines were taken on earlier ones — see §9.

| | |
|---|---|
| Brainprint commit | `159c7ea` (the tree these measurements ran against) |
| OS | macOS 27.0, build 26A428 |
| Kernel | Darwin 27.0.0, `arm64` (`xnu-13432.1.9~1`) |
| CPU | Apple M5, 10 logical cores |
| Physical RAM | 25,769,803,776 bytes (24 GiB) |
| Filesystem | APFS; every fixture is copied to `$TMPDIR` before measuring |
| Build profile | `--release` for the harness, so the harness is not the cost |

| backend | version | provenance |
|---|---|---|
| Rust toolchain | rustc 1.98.1 (48a229cea 2026-09-01), cargo 1.98.1 | rustup, host `aarch64-apple-darwin` |
| Python | pyright / pyright-typeserver 1.1.414 | pinned under `scripts/python_semantic_spike` |
| TypeScript / JavaScript | typescript 7.0.2 (native preview server) | pinned under `scripts/typescript_semantic_spike` |
| Svelte | svelte-language-server 0.18.4, svelte 5.57.1, svelte2tsx 0.7.61, **typescript 6.0.3** | pinned under `scripts/svelte_semantic_spike` |
| C# | microsoft.codeanalysis.languageserver.osx-arm64 5.4.0-2.26179.14, .NET SDK 10.0.102 | restored under `scripts/csharp_semantic_spike` |
| Rust semantic | rust-analyzer 1.98.1 (48a229ce 2026-09-01) | `rustup which rust-analyzer` |
| Node | v24.13.0 | supplied, never searched for by the engine |
| `rust-src` | **present** on this machine | this differs from task 13, where it was absent |

Svelte's TypeScript 6.0.3 and the standalone TypeScript 7.0.2 are two
different toolchains in two different processes, and this report never
adds their costs together as though they were one runtime.

**Preconditions.** Cold samples start a process that was not running.
Warm samples run with **every backend shut down**, which is the point:
the claim under test is that a warm question reads persisted canonical
truth, and the cheapest way to prove it is to ask when there is no
server to wake. Cold and warm observations are never mixed into one
distribution.

**Fixtures.** Files per fixture, excluding `node_modules` and `target`:
python-semantic-spike 13, typescript-semantic-spike 28,
svelte-semantic-spike 12, csharp-semantic-spike 23, rust-semantic-spike
15, python-signature-impact 7. These are small on purpose — they are
correctness fixtures — and no storage or latency figure here should be
extrapolated to a real repository.

**Commands.**

```sh
cargo build --release -p brainprint-engine --example i4_final_benchmark
./target/release/examples/i4_final_benchmark \
  --output benchmarks/reports/i4-final.jsonl \
  --cold-samples 5 --warm-repeat 20 --lifecycle-repeat 10

cargo run --release -p brainprint-engine --example i4_python_benchmark -- \
  --scenario benchmarks/scenarios/python-signature-impact.json \
  --output benchmarks/reports/i4-python-task15.jsonl --repeat 20

python3 scripts/i4_final_acceptance/measure_rss.py \
  --hold-ms 20000 --output benchmarks/reports/i4-final-rss.json
```

---

## 2. Correctness

### 2.1 Every language's own acceptance, re-run unchanged

Not rewritten for this task, not reshaped so the counts would look
symmetric. These suites are the evidence; task 15 only ran them again.

| suite | result | what it proves |
|---|---|---|
| `i4_python_acceptance` | 2 passed | Python Level A slice + Level B |
| `python_semantic_pyright` | 2 passed | the pyright transport boundary |
| `i4_typescript_acceptance` | 3 passed | TS/JS + React/TSX Level A slice |
| `typescript_semantic_lsp` | 11 passed | the TS7 transport boundary |
| `i4_svelte_acceptance` | 3 passed | Svelte container + embedded TS/JS |
| `i4_csharp_acceptance` | 13 passed | C# Roslyn slice, trust, partial types |
| `i4_rust_acceptance` | 13 passed | Rust slice, trait/impl, trust |
| `i4_fleet_acceptance` (real) | 3 passed | five real families in one supervisor |
| `i4_fleet_acceptance` (always-on) | 15 passed | publication, worktree, capacity |
| `i2_acceptance` | 7 passed | Resource tier |
| `i3_acceptance` | 20 passed | structural Relation graph |
| `cargo test --workspace --locked` | 1168 passed, 0 failed, 50 ignored | the whole suite with no backend installed |

The 50 ignored are exactly the real-backend tests, and every one of them
was then run explicitly, above.

### 2.2 Cross-language summary

The per-language matrices are not restated here; they live in the task
9–13 completion records in #19 and in
`scripts/{csharp,rust}_semantic_spike/ACCEPTANCE.md`. This is the
one-page view, and **nothing in it changed in task 15** — no capability
verdict was moved to make a number look better.

| | Python | TS / JS | React (TSX) | Svelte | C# | Rust |
|---|---|---|---|---|---|---|
| structural tier | I2/I3, complete | I2/I3, complete | I2/I3, complete | I2/I3 + container extraction | I2/I3, complete | I2/I3, complete |
| semantic backend | pyright-typeserver | TS7 | **none — TS/JS serves it** | svelte-language-server | Roslyn LSP | rust-analyzer |
| central capability | import/reference/inheritance | overload + implements | component binding | original-source mapping | partial types + overload | trait IMPLEMENTS |
| known PARTIAL | `type_resolution`, `inheritance` (non-name bases) | `static_dispatch_target`, `overrides`, `type_resolution`, `implementation_target`; JS `static_dispatch_target` | — (inherits TS/JS) | `calls_*`, `static_dispatch_target`, `$props()` destructuring, `<script module>` cross-scope | multi-target | `import_binding` (compound/glob `use`), `type_resolution` (bounds, generic args) |
| UNSUPPORTED / N/A | `export_declaration` (no such clause) | JS `type_resolution`, `inheritance`, `overrides`; `implements`/`overload` N/A for JS | hook/route/component-ownership (deliberate) | `inheritance`/`implements`/`overrides`/`overload` — no such syntax | — | `inheritance`, `overrides`, `overload_resolution` — no such language feature |
| real-backend acceptance | pass | pass | pass | pass | pass | pass |
| Level B without the backend | proved | proved | proved | proved | proved | proved |
| prepared current source | `source_complete=true` | `source_complete=true` | via TS/JS | `source_complete=true` | `source_complete=true` | `source_complete=true` |
| dependency source indexed | no | no | no | no | no | no |
| freshness transition measured | save + config | save + config | via TS/JS | save (config: see §5.4) | save + project reload | save + Cargo.toml reload |

React deserves the emphasis: **additional backend count = 0**. A `.tsx`
file is TypeScript, a component identifier is an ordinary binding, and
the measurement in §4 was answered by the TypeScript process that was
already there.

### 2.3 The hard gates

Every one of these fails I4 closure on a single occurrence. Measured
across the final benchmark run and the re-run acceptance suites:

| gate | result | where it was observed |
|---|---|---|
| stale semantic result returned as CURRENT | **0** | 43 measured source/config transitions across five families, `stale_current_incidents=0` on every line |
| supported query returning a clean zero it should not | **0** | every warm anchor returned its expected references; see §4 |
| worktree contamination | **0** | `i4_fleet_acceptance`: context key, runtime entry, dedupe key, publication, crash and `LogicalIdentity` all diverge on `WorkspaceId` alone |
| duplicate logical relation from structural + semantic proof | **0** | per-language merge acceptance; `relation_rows` equals `confirmed` on every family |
| backend-specific identity leaking canonically | **0** | see §2.4 |
| dependency deep index | **0** | see §2.5 |
| generated/virtual source exposed as editable current source | **0** | see §2.6 |
| backend failure removing structural truth | **0** | Level B suites; `brainprint-i4-python-restart` answered 3 confirmed structural relations during the outage |
| one backend per client for one AnalysisContext | **held** | 10 clients, 5 contexts, 5 starts, `duplicate_starts=0` — §6 |

### 2.4 Canonical identity

No backend handle reaches a public Brainprint identity, and the proof is
the type system rather than a runtime scan. The public endpoint
vocabulary is `GraphEndpoint::{Resource, Symbol, External, Domain,
Logical}`; there is no variant that can carry a Pyright handle, a
TypeScript document identity, a Svelte generated-file identity, a Roslyn
`SymbolKey`/`ProjectId`/`DocumentId`, or a rust-analyzer `FileId`, crate
id or salsa key — so a leak is not a bug that could slip through review,
it is code that would not compile. Each family's acceptance additionally
asserts the negative directly (for Rust,
`no_backend_internal_identity_is_representable`).

`LogicalSymbolId` is the one identity added for a backend-shaped problem
(C# `partial class`), and it is derived from context key, project key,
qualified name, kind and arity — all facts a compiler states, none of
them a backend's internal handle.

### 2.5 Dependencies are not deep-indexed

Measured, per family, as the Workspace Resource inventory after a full
semantic refresh that visited dependency definitions:

| family | dependency visited during resolution | became a Resource |
|---|---|---|
| Python | `requests`, `abc`, `json`, `os` | no — one `ExternalEntity` per package/module |
| TS / JS | `@types/node` (`Buffer.from`), `node:path` | no — package + module identity only |
| Svelte | `$state` / `$props` runes, `node_modules` toolchain | no — asserted directly by the acceptance |
| C# | framework / NuGet assemblies | no — assembly identity as `ExternalEntity` |
| Rust | std / sysroot, registry crates | no — crate name + version, never a path |

The `-first-refresh` lines carry the inventory as `owners=`: 12 Python,
24 TS/JS, 6 Svelte, 15 C#, 9 Rust. Those are the fixture's own files.
`node_modules`, `target/`, the Cargo registry, the Rust sysroot and the
.NET reference assemblies contributed **zero** Resources, and nothing in
this task walked them to prove it — the inventory is the proof.

### 2.6 Generated and virtual source

| case | required | observed |
|---|---|---|
| Svelte generated → original | every relation on an exact `.svelte` span | 29/29 original spans, 0 generated, re-proved by `i4_svelte_acceptance` |
| Rust macro / virtual expansion | refused where unmappable, never a fake span | refused; a call inside a macro invocation anchors nothing, recorded as a limitation and not as an edge |
| C# decompiled / external location | `ExternalEntity`, never a Workspace Resource | held; assembly identity, no source imported |

One failure here is an I4 hard failure. There were none.

---

## 3. Accuracy: what the semantic tier closed

Counts, not a percentage — there is no honest denominator for "accuracy"
across five languages. `confirmed` is rows in `relation`;
`requires_semantics` is unresolved references whose recorded reason is
one a backend is needed for.

| family | confirmed | candidates | unresolved | requires_semantics | new confirmed |
|---|---|---|---|---|---|
| Python | 33 → **84** | 0 → 0 | 53 → **7** | 6 → **0** | +51 |
| TS / JS | 18 → **70** | 6 → **0** | 37 → **7** | 6 → **0** | +52 |
| Svelte | 6 → **30** | 0 → 0 | 2 → 2 | 0 → 0 | +24 |
| C# | 1 → **37** | 0 → 0 | 40 → **14** | 22 → **10** | +36 |
| Rust | 8 → **49** | 0 → 0 | 56 → **11** | 19 → **0** | +41 |

Reading these honestly:

- **Python, TS/JS and Rust drive `requires_semantics` to zero.** Every
  gap the structural tier labelled "a compiler has to answer this" was
  answered.
- **Svelte's 2 unresolved do not move** and its `requires_semantics` was
  already 0. Its container gaps are the recorded PARTIAL cases
  (template call sites, `$props()` destructuring), which are I3 anchor
  limitations rather than questions the backend refused.
- **C# leaves 10 of 22.** Those are the multi-target PARTIAL recorded in
  task 12: the official Roslyn LSP does not say *which* target framework
  answered, so the unanswered sites stay unresolved rather than being
  attributed to a world nobody named. That is the recorded coverage
  contract, not a regression against it.

**False positives: 0.** No relation was confirmed that the per-language
acceptance did not expect; every suite asserts its exact target set, and
the same-name traps (Python's six `run`s, TypeScript's overloads, Rust's
two traits both declaring `run`, C#'s partial declarations) are in the
fixtures precisely so a name-matching answer fails loudly.

**Conflicts: 0, over the sites listed above.** The denominator matters:
this is 0 conflicts across the 66 owners actually refreshed by a live
backend in this run, not 0 from a backend that was skipped. No family
was skipped — all five were installed.

**False zero: 0** in the mandatory supported cases. Every `-warm` line
records the anchor it measured and its reference count, and the harness
refuses to measure an anchor with no incoming edges when one with edges
exists, precisely so a fixture choice cannot be mistaken for a clean
empty answer.

---

## 4. Latency

`n` is the sample count. `p95` is reported only where n ≥ 20; below
that it would be the maximum wearing a label it has not earned, and the
tables say so rather than printing a number. All timing is monotonic.

### 4.1 Cold backend start (n = 5 each)

Start to the point the server can answer: spawn, handshake, protocol
negotiation, and — for C# and Rust — the project load or quiescence the
server announces.

| family | min | p50 | max | p95 | what "ready" includes |
|---|---|---|---|---|---|
| TypeScript / JS | 11.0 ms | **11.5 ms** | 193.2 ms | not measured (n<20) | spawn + handshake + encoding negotiation |
| Svelte | 379.6 ms | **416.8 ms** | 1026.9 ms | not measured (n<20) | Node + language server startup |
| C# | 2520.5 ms | **3160.8 ms** | 4666.4 ms | not measured (n<20) | + `OpenSolution` + announced project initialization |
| Rust | 3319.2 ms | **3754.2 ms** | 5942.0 ms | not measured (n<20) | + `cargo metadata` + announced quiescence |
| Python | 2711.3 ms | **3576.1 ms** | 7807.7 ms | not measured (n<20) | Node + pyright typeserver readiness |

React: **no cold start**, because there is no React backend.

The maxima are real and kept: the C#, Rust and Python outliers are the
same run in which several servers were competing for ten cores. Dropping
them would be measuring the machine's best mood.

### 4.2 First semantic refresh after cold start (n = 1)

Every owner in the fixture, refreshed once, evidence published.

| family | owners | evidence | elapsed | per owner |
|---|---|---|---|---|
| TypeScript / JS | 24 | 71 | 103 ms | 4.3 ms |
| Rust | 9 | 60 | 370 ms | 41 ms |
| Python | 12 | 63 | 476 ms | 40 ms |
| Svelte | 6 | 33 | 679 ms | 113 ms |
| C# | 15 | 51 | 1288 ms | 86 ms |

One sample, stated as one sample. This is a whole-fixture first pass,
not a steady-state figure.

### 4.3 Warm semantic query (n = 20 each)

The Agent-facing flow — locate the symbol, ask who reaches it, prepare
the current source — with **every backend shut down**.

| family | anchor | min | p50 | p95 | max | refs | backends running |
|---|---|---|---|---|---|---|---|
| Svelte | `count` | 2.31 ms | **2.48 ms** | 3.44 ms | 3.44 ms | 1 | 0 |
| C# | `IRunner` | 2.39 ms | **2.59 ms** | 3.24 ms | 3.24 ms | 2 | 0 |
| Python | `Base` | 2.53 ms | **2.81 ms** | 3.36 ms | 3.36 ms | 1 | 0 |
| TypeScript | `UserCard` (TSX) | 2.70 ms | **3.00 ms** | 5.74 ms | 5.74 ms | 1 | 0 |
| Rust | `Runner` | 2.45 ms | **3.33 ms** | 4.17 ms | 4.17 ms | 2 | 0 |

Two facts are worth separating. The **2–3 ms** is the query. The **0
backends running** is the architecture: a warm semantic answer is a read
of persisted canonical truth, and the language server that proved it is
not merely idle, it is gone.

### 4.4 Source save → semantic CURRENT (n = 10 each)

Withdraw the affected contributions, write, reconcile incrementally,
tell the backend, refresh, publish. The documented order — withdrawal
before structural replacement — is what the harness runs, not a
whole-Workspace rescan.

| family | min | p50 | max | p95 | stale-current |
|---|---|---|---|---|---|
| Svelte | 16.1 ms | **17.7 ms** | 26.6 ms | not measured (n<20) | 0 |
| TypeScript / JS | 19.6 ms | **25.4 ms** | 30.1 ms | not measured (n<20) | 0 |
| Python | 22.3 ms | **28.4 ms** | 35.7 ms | not measured (n<20) | 0 |
| Rust | 29.8 ms | **33.2 ms** | 36.1 ms | not measured (n<20) | 0 |
| C# | 34.6 ms | **37.2 ms** | 88.7 ms | not measured (n<20) | 0 |

Between the withdrawal and the refresh the harness asserts that nothing
affected still reads CURRENT. Across 50 save transitions: **0
incidents**.

### 4.5 Config / project change → semantic CURRENT

Not the same operation in every language, and not folded together.

| family | what changed | n | min | p50 | max |
|---|---|---|---|---|---|
| TypeScript / JS | `tsconfig.json` (the path-alias map itself) | 10 | 61.2 ms | **67.0 ms** | 92.8 ms |
| Python | `pyrightconfig.json` type-checking mode | 10 | 83.8 ms | **99.1 ms** | 119.6 ms |
| Rust | `crates/core/Cargo.toml` + project reload | 3 | 1183.4 ms | **1222.5 ms** | 1257.9 ms |
| C# | `.csproj` + solution reload | 3 | 70.4 ms | **1635.9 ms** | 1748.0 ms |
| Svelte | — | 0 | not measured | not measured | not measured |

Three rounds for C# and Rust rather than ten, and the reason is
recorded: a solution or Cargo reload is the expensive transition, and
repeating it ten times measures MSBuild and cargo rather than
Brainprint. C#'s first round is 70 ms because the reload found nothing
to re-read; the two that did are the 1.6–1.7 s figures, and both are
kept.

Svelte is `not measured`: the container's config basis is discovered
from its toolchain rather than from one Workspace file this harness can
move. Its freshness proof is the source transition in §4.4, and its
config handling is proved by `i4_svelte_acceptance`.

---

## 5. Resources and storage

### 5.1 RSS

`brainprint-engine` reports `ResourceUsage { rss_bytes: None }` for
every host, and that is the correct product answer: the Workspace denies
`unsafe_code`, so a child's resident size is not reachable, and a zero
would be a fabricated measurement. Task 14 refused to add process
scraping to product code and task 15 does not either. The measurement
lives in benchmark tooling instead:
`scripts/i4_final_acceptance/measure_rss.py`.

**Scope, stated:** the whole fixture-owned process tree, matched by
descent from the harness pid — never by process name, so an editor's own
language server cannot wander into the total. 47 samples at 250 ms while
all five families were held up.

| process | peak RSS |
|---|---|
| rust-analyzer | 372.4 MB |
| Roslyn language server (`microsoft.codeanalysis.languageserver`) | 115.0 MB |
| node (pyright typeserver) | 88.3 MB |
| node (svelte-language-server) | 69.7 MB |
| cargo / rustc children spawned by rust-analyzer's project load | 0.2 – 21.0 MB each, 19 of them, transient |

| total | value | what it means |
|---|---|---|
| **max simultaneous** | **560.8 MB across 5 processes** | the largest total actually observed in one sample — the figure to quote |
| sum of per-process peaks | 778.8 MB across 23 processes | an upper bound nobody paid at once; includes transient cargo/rustc children |

The five-family steady state on this machine is **~561 MB**. The
`cargo`/`rustc` children are rust-analyzer loading the project and are
gone afterwards; counting their peaks into a single total would
overstate the steady state by about 40%, which is why both numbers are
here and only one is called the answer.

### 5.2 CPU

**Not measured.** The harness cannot read its own or a child's CPU time
without `unsafe_code`, and the external sampler reads RSS from `ps`
without sampling CPU time reliably enough to report a distribution.
Building cross-platform process telemetry is not I4 semantic
correctness. Wrapping the harness in `/usr/bin/time -l` gives a
whole-run figure for anyone who wants one; no such figure is claimed
here.

### 5.3 Index size

`index.db` before and after semantic enrichment, and what the semantic
tier actually persisted:

| family | index.db before | after | delta | relation rows | semantic evidence rows | publications |
|---|---|---|---|---|---|---|
| Svelte | 335,872 B | 335,872 B | 0 | 30 | 29 | 6 |
| Python | 352,256 B | 352,256 B | 0 | 84 | 56 | 12 |
| Rust | 360,448 B | 360,448 B | 0 | 49 | 49 | 9 |
| TypeScript | 368,640 B | 368,640 B | 0 | 70 | 64 | 24 |
| C# | 368,640 B | 368,640 B | 0 | 37 | 34 | 15 |

The zero deltas are real and mean what they say: at this fixture size
the semantic tier's rows land inside pages SQLite had already allocated,
so the file does not grow at all. That is a fact about a 6-to-24-file
fixture and **no storage scaling claim follows from it**. The row counts
are the useful figure: a few dozen rows per fixture, and not one byte of
dependency source — which is the property this measurement exists to
establish.

### 5.4 What is not measured

| metric | status | reason |
|---|---|---|
| model input / output tokens | not measured | no model is involved in this harness; running a tokenizer over harness output would be a number about a tokenizer |
| per-backend CPU time | not measured | see §5.2 |
| I/O counters | not measured | no reproducible cross-platform reader; not I4 correctness |
| Svelte config → current | not measured | see §4.5 |
| p95 for cold / save / config | not measured | n = 3, 5 or 10; the rule is n ≥ 20 and it is not bent |
| storage at repository scale | not measured | the fixtures are correctness fixtures |

---

## 6. Multi-backend and multi-client

One `SemanticRuntimeSupervisor`, every installed family, measured rather
than asserted.

```text
families = python + typescript + svelte + csharp + rust
clients            10   (two independent callers per context)
contexts            5
live runtimes       5
backend starts      5
duplicate starts    0
restarts            0
crashes             0
React additional backends  0
```

Sequential cold start, time to each context READY, one sample:

| family | to ready |
|---|---|
| rust | 39.3 ms |
| typescript | 40.7 ms |
| csharp | 483.9 ms |
| svelte | 665.0 ms |
| python | 3730.9 ms |
| **all five ready** | **4959.9 ms** |

These are much lower than §4.1 for C# and Rust because the fleet
scenario acquires the context and does not load the project: this is
process topology, not project initialization. The two are different
measurements and are kept apart rather than averaged.

`10 clients → 5 contexts → 5 backends` is the central I4 runtime
requirement, and it held against real processes. Task 14's
`i4_fleet_acceptance` proves the same shape deterministically, including
the cases this benchmark does not create (crash isolation, capacity
eviction, worktree divergence).

**The full-fleet case is a benchmark scenario, not a startup policy.**
Nothing in Brainprint starts a backend nobody asked about.

---

## 7. Exploration economy available at I4

What an Agent-facing question costs today, measured through the ordinary
`InspectPreparer` path. I5 owns delivery budgets, projection and
continuation; none of that is measured or implemented here.

| family | tool calls | prepared ranges | prepared source | whole-file bytes avoided | duplicate bytes | `source_complete` |
|---|---|---|---|---|---|---|
| Python | 3 | 3 | 110 B | 71 B | 0 | true |
| TypeScript | 3 | 3 | 257 B | 209 B | 0 | true |
| Svelte | 3 | 4 | 37 B | 485 B | 0 | true |
| C# | 3 | 1 | 44 B | 77 B | 0 | true |
| Rust | 3 | 1 | 236 B | 713 B | 0 | true |

"Whole-file bytes avoided" is arithmetic on observations: the size of
the files the prepared ranges came from, minus the prepared bytes. It
is what reading those files whole would have cost this workflow — not a
claim about any Agent's behaviour.

`source_complete=true` everywhere is the I4 promise being kept: the
answer is not a path and a line number that force immediate
rediscovery, it carries the current editable source. Zero duplicate
bytes across all five: the preparer hands the same span once.

---

## 8. Recovery and degradation

Task 14 and the per-family suites already prove these deterministically;
task 15 records the observation rather than crashing five real servers
again for the sake of it.

| property | evidence | result |
|---|---|---|
| backend unavailable → Level B | every family's Level B test; the whole 1168-test suite runs with nothing installed | Resource, Symbol, Occurrence, Relation, search and source all remain; semantic gaps stay explicit |
| crash → restart → refresh only what needs it | `brainprint-i4-python-restart` | ready again 3775 ms, refresh after restart 288 ms, 1 owner refreshed, 0 crashes recorded, structural query answered 3 confirmed relations **during** the outage |
| persisted CURRENT with the runtime cold | §4.3, `i4_fleet_acceptance` | 2–3 ms answers, 0 backends running, starts counter unchanged |
| crash cannot restore invalidated truth | `a_crash_cannot_make_invalidated_truth_current_again` | held |
| capacity eviction does not dirty truth | `a_retired_runtime_leaves_everything_it_published_exactly_where_it_was` | held |

---

## 9. I3 → I4, on the one scenario that is comparable

`python-signature-impact` is the only workflow measured in the same
schema from I0 forward, so it is the only place a comparison is honest.
The historical lines are **preserved, not re-run and overwritten**;
`benchmarks/baselines/` still holds them.

### 9.1 Historical record (earlier machines, earlier commits)

| variant | calls | raw bytes | duplicate | prepared source |
|---|---|---|---|---|
| `basic-tools` | 6 | 1521 | 657 | — |
| `brainprint-i2` | 3 | 1032 | 168 | — |
| `brainprint-i3-index` | 1 | 864 | 0 | — |
| `brainprint-i3` (warm) | 3 | 657 | 0 | 533 |

### 9.2 This commit, this machine (n = 20 warm)

| variant | calls | raw bytes | duplicate | prepared source | p50 | p95 |
|---|---|---|---|---|---|---|
| `brainprint-i3-control` | 3 | 657 | 0 | 533 | 5 ms | 8 ms |
| `brainprint-i4-python` | 3 | 657 | 0 | 533 | **4 ms** | **5 ms** |

**The exploration cost did not move.** Same three calls, same 657 bytes,
same 533 prepared bytes, same zero duplicates. That is the point: I4 did
not buy its accuracy with a more expensive question.

### 9.3 What changed is the proof

| | I3 control | I4 semantic |
|---|---|---|
| confirmed relations | 9 | **14** |
| candidates | 0 | 0 |
| unresolved | 7 | **2** |
| requires-semantics | 1 | **0** |
| relation kinds | CALLS:3 + IMPORTS:5 + USES_ENV:1 | CALLS:**4** + IMPORTS:5 + USES_ENV:1 + **USES_TYPE:4** |
| remaining reasons | NO_STRUCTURAL_BINDING:4, POSSIBLY_SHADOWED:2, RECEIVER_TYPE_REQUIRED:1 | POSSIBLY_SHADOWED:2 |
| backend requests to answer the warm query | 0 | **0** |

Four type relations the structural tier could not state, one more call
edge, and the one `RECEIVER_TYPE_REQUIRED` gap closed. The two
`POSSIBLY_SHADOWED` that remain are recorded PARTIAL, not silence.

### 9.4 And what it cost

Kept in its own column rather than folded into the query line:

| cost the semantic tier adds | observed | samples |
|---|---|---|
| backend cold start | 2893 ms | 1 |
| first owner refresh | 236 ms (19 backend requests to refresh all owners) | 1 |
| save → current | 30 ms, 4 owners affected, 0 unrelated owners refreshed, 0 stale-current | 1 |
| config → current | 26 ms, 6 owners affected, 0 unrelated refreshed, 0 stale-current | 1 |
| crash → ready again | 3775 ms, then 288 ms to refresh the one owner that needed it | 1 |
| structural index build (unchanged by I4) | 36 ms over 7 files | 1 |

**Schema compatibility.** §9.1 and §9.2 share one scenario file, one
fixture snapshot and one JSONL schema, so `calls`, `raw bytes`,
`duplicate` and `prepared source` are directly comparable. The *timings*
are not: §9.1 was taken on earlier machines. The p50 of 5 ms vs 4 ms
above is I3-control vs I4 **on the same machine in the same run**, which
is the only latency comparison this report makes.

The §4 tables use a different harness and different fixtures, and are
not comparable with §9 at all.

---

## 10. Decisions deliberately not taken

**`RuntimePolicy.max_live_runtimes` stays `None`.** The mechanism is
complete and proved (task 14). The measurement now exists — five real
families cost ~561 MB simultaneously on this machine — but it is *one
fixture set on one 24 GiB Mac*, and turning that into a universal cap
would be inventing a product constant from a single observation. I7's
real-project measurement is what should inform deployment policy.

**`idle_timeout` stays at its current configurable value.** Same
reasoning. The cold-start figures in §4.1 say what re-warming costs
(11 ms for TypeScript, 3.8 s for Rust), which is exactly the input a
future tuning decision needs — but the decision needs representative
projects, not this fixture.

**No latency or memory threshold was invented.** §42 of the task
contract forbids retroactively deciding what "fast enough" means, and
nothing here does.

---

## 11. Known cost, accepted

Recorded rather than optimized, because correctness is intact and I4 is
allowed to finish with known cost:

- **rust-analyzer is 372 MB**, two-thirds of the five-family total, and
  its project load spawns nineteen transient cargo/rustc children.
- **Cold start is 2.5–3.8 s for Python, C# and Rust** and up to 7.8 s
  under contention.
- **A Cargo or solution reload is 1.2–1.7 s**, two orders of magnitude
  above a source save.
- **Svelte runs its own TypeScript 6.0.3** beside the standalone
  TypeScript 7.0.2 — two processes, two toolchains, by design.

None of these violates a locked I4 constraint. All of them are input to
later runtime tuning.

---

## 12. Deferred

| measurement | owner | why not here |
|---|---|---|
| Agent-facing projection, delivery budget, continuation, MCP/Skill | **I5** | the surfaces do not exist; measuring them now would be measuring nothing |
| `delivered_tokens`, client shaping | **I5** | same |
| Git/test/build command intelligence, bounded exec, compact diagnostics | **I6** | I4 claims a standalone *semantic/code-intelligence layer*, not a standalone coding workflow |
| real-project Claude / RTK / CodeGraph / Serena A/B | **I7** | #12 assigns it there, and it is only fair once I5 and I6 have supplied the Agent-facing surfaces. Comparing mature Agent tooling against an engine-only API would be an unfair comparison, not a measurement |
| model input/output tokens | **I7** | needs a real model run |

**This benchmark is not the #10 P0 benchmark.** It is the I4-specific
subset of it that can be measured honestly with the surfaces I4 built.

---

## 13. Conclusion

```text
I4 correctness: PASS

Supported semantic families:
  Python                  pyright-typeserver 1.1.414
  TypeScript / JavaScript TypeScript 7.0.2        (React/TSX rides on this)
  Svelte                  svelte-language-server 0.18.4 + TypeScript 6.0.3
  C#                      Roslyn LSP 5.4.0-2.26179.14
  Rust                    rust-analyzer 1.98.1

Hard correctness failures:  0 / 9
  stale-current incidents               0  (43 measured transitions)
  false zeros in supported cases        0
  false-positive relations              0
  merge conflicts                       0  (66 owners evaluated, all five families live)
  worktree contamination                0
  dependency deep index                 0
  generated-source identity leaks       0
  backend-identity leaks                0
  duplicate backend per client          0  (10 clients, 5 contexts, 5 starts)

Known PARTIAL limitations: unchanged from tasks 9-13.
  Python  type_resolution, inheritance (non-name bases)
  TS/JS   static_dispatch_target, overrides, type_resolution,
          implementation_target; JS type_resolution and inheritance
          UNSUPPORTED for want of an I3 anchor
  Svelte  template call sites, $props() destructuring,
          <script module> cross-scope reference
  C#      multi-target (10 of 22 requires-semantics sites stay open)
  Rust    compound/glob `use`, trait bounds, generic arguments,
          associated types, calls inside macro invocations

Runtime:
  shared context, several clients   verified (10 -> 5 -> 5, real processes)
  worktree isolation                verified
  degradation to Level B            verified for all five families
  crash / restart / cold reopen     verified

Benchmark:
  observed   cold start, first refresh, warm query, save->current,
             config->current, fleet startup, RSS, index size,
             accuracy delta, prepared/raw/duplicate source, call counts
  unknown    CPU time, I/O, model tokens, Svelte config->current,
             p95 where n < 20, repository-scale storage

Historical I3 -> I4 (python-signature-impact, same schema):
  exploration    3 calls / 657 raw bytes / 533 prepared bytes -- unchanged
  accuracy       confirmed 9 -> 14, unresolved 7 -> 2,
                 requires-semantics 1 -> 0, +4 USES_TYPE, +1 CALLS
  warm latency   5 ms -> 4 ms p50 (same machine, same run)
  added cost     2893 ms cold start, 236 ms first refresh,
                 30 ms save->current, 26 ms config->current

Deferred:
  I5  projection / delivery budget / MCP / Skill
  I6  command intelligence / standalone coding workflow
  I7  real-project Agent A/B, model token accounting
```

**What I4 proves.** Five language families answer binding, reference,
call, type and implementation questions through one canonical graph,
with one shared runtime per semantic world, with the current editable
source attached, without indexing a single dependency file, and without
ever presenting a stale or fabricated answer as current.

**What I4 does not prove.** That an Agent completes real tasks faster or
cheaper with Brainprint than without it. That measurement needs I5's
Agent-facing projection and I6's command intelligence to exist first,
and it belongs to I7.
