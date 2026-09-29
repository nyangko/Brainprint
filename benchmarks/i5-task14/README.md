# I5 Task 14 — rerun at base `278f4cb` (2026-09-29)

Execution contract: issue #32. This directory holds the compact, reproducible
artifact of the rerun. No raw transcripts or model reasoning are stored.

- `ab_summary_r1_278f4cb.json` — per-run metrics of the first Claude Code A/B (10 runs, base `278f4cb`), the
  first-route/adoption/economy counters, the hook telemetry aggregates and the
  answer grades. The final answers themselves are not kept.
- `ab_summary_r2_4e270fc.json` — the same A/B rerun, same harness, after the T1/T2 fixes
  (`4e270fc`, lint fix `f1bd8e6`). One A run (`T2-A-r2`) lists one Brainprint call:
  the agent ran `which brainprint` and the analyzer classified it; the A arm has no
  Brainprint tool.
- `harness/` — the scripts that produced every number. Absolute paths are
  templated as `@T14_ROOT@`; instantiate with
  `sed -i '' "s#@T14_ROOT@#$T14_ROOT#g" harness/*` (GNU sed: drop the `''`).

## Reproduce

1. Clone the repository at `278f4cb` to `$T14_ROOT/clean/repo` and build
   `cargo build --release --locked` with `env -i`, a `PATH` that contains
   only `~/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin`, and an empty `HOME`
   (no RTK / CodeGraph / Serena / Headroom / claude-mem / OpenViking).
2. `harness/benv.sh` isolates `HOME` and `XDG_RUNTIME_DIR`; `harness/startd.py`
   starts `brainprintd` and times `status`-ready.
3. `brainprint install`, then `brainprint init <workspace>` on a clone of the
   repository. Trusted Level A needs `project_execution_trust = "Trusted"` in
   the Workspace `.brainprint/config.toml` and `[semantic_backends.rust]
   executable = ".../rust-analyzer"` in the global `config.toml`; restart the
   daemon after editing either.
4. Product scenarios: `qsuite.py` (10 CLI operations), `mcpc.py` (MCP tools),
   `callers.py` + `sem.py` (target-centric callers and the owner-demand flow),
   `reuse.py` + `t14_reuse_probe.rs` (retained wire bytes and same-revision
   source reads), `s4.py` (revision change), `s6.py` (10 sessions × 2
   Workspaces), `foot.py` (cold activation footprint).
5. A/B: one detached clone per run, `run_all.sh` (order A→B→B→A per task, then
   one `guard` run per task), `analyze.py` → `out/summary.json`. Claude Code
   2.1.284, `claude-opus-5-5`, `--effort medium`, `--setting-sources project`,
   `--strict-mcp-config`. Ground truth: `gt.md`.
6. Regression: `regress.sh` (fmt, clippy `-D warnings`, build, test in the
   reference-free environment). The repository gate is the GitHub Actions
   all-OS Acceptance + I0 smoke on the commit.

## Measurement notes

- The first A/B attempt used a wrong binary path (the MCP server was
  `failed`, hooks could not find `brainprint-agent`, telemetry 0). Those ten
  runs are invalid, were discarded, and every A and B run was repeated with the
  corrected path. The numbers here are from the repeat only.
- All runs shared one machine that also ran unrelated work; wall-time and RSS
  are observations, not thresholds. No target number is LOCKED, so none is
  asserted.
- Rerun 2 (`4e270fc`): the isolated `HOME` must start with a fresh Brainprint
  registry when worktree paths repeat (`brainprint init` refuses a path whose
  registry entry points at a deleted project home). Keep the global
  `config.toml`, delete the rest of `~/.brainprint`, `install`, then `init`.
- Analyzer counts: `sequence` lists tool calls in order; `readAfterBP` counts a
  native Read of a path that appeared in earlier Brainprint output, whether or
  not Brainprint delivered that file's content (a location-only `find text`
  hit counts).

## Cost analysis (base `2997729`)

- `analysis/cost_breakdown_r1_r2.json` — every API request and tool call of both A/B sets
  (`out_278f4cb` = rerun 1, `out` = rerun 2): cache-write/read tokens, the tokens each tool
  result added to the context (the next request's cache-write), marginal USD, and the
  per-run category split (BASE / SETUP / BP_RESULT / NATIVE_RESULT / OTHER / OUTPUT).
- `analysis/cost_breakdown_pairs.json` — 8 extra runs, same worktree back to back, to see
  what the prompt cache does to the same conversation.
- `harness/breakdown.py` builds both files. Prices are fitted from the runs' own
  `total_cost_usd` (input $4, cache-write $8, cache-read $0.20, output $20 per 1M tokens);
  the fit reproduces all 20 totals exactly. Per-request `output_tokens` in the stream are
  mid-stream values, so output is priced per run.
- `harness/run_e.sh`, `run_pairs.sh` — measurement-only run variants. `ENABLE_TOOL_SEARCH=false`
  was tried and discarded: it loads every deferred built-in tool too (cache-write 56k, $0.525).
- `harness/owner_cost.py` (caller-owner refresh time/RSS through the product CLI) and
  `harness/t2_estimate.py` (enclosing-symbol/source additions to `find text`, inspect packet sizes).
- Contamination note: worktrees at `5430637` or later contain `benchmarks/i5-task14/`. Rerun 2
  and the pair runs therefore had harness files in every Grep/`find text` result (about 2.5 KB of a
  4.6 KB T1 Grep, and 4 of 10 `find text` matches). Rerun 1 (base `278f4cb`) did not. Remove the
  directory from each worktree before the next A/B.

