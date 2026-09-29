# I5 Task 14 — rerun at base `278f4cb` (2026-09-29)

Execution contract: issue #32. This directory holds the compact, reproducible
artifact of the rerun. No raw transcripts or model reasoning are stored.

- `ab_summary.json` — per-run metrics of the Claude Code A/B (10 runs), the
  first-route/adoption/economy counters, the hook telemetry aggregates and the
  answer grades. The final answers themselves are not kept.
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
