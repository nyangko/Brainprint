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

## Tool-selection instruction experiment (base `ed2abd0`)

Instruction only, no product change. `harness/instr.md` is appended to the B1 worktree's
`CLAUDE.md` (never to A or B0). B0 = Brainprint as before, B1 = B0 + the instruction.

- `harness/run_instr.sh` (one run in a fresh worktree at the base commit with
  `benchmarks/i5-task14/` removed for A and B alike), `run_instr_all.sh` (T1 and T2, four rounds
  of A/B0/B1 in ABBA order = four runs per condition per task), `run_instr_t1.sh` (T1 block only),
  `harness/instr_compare.py` (per run: cache class, ToolSearch tokens, cost split, grade).
- `analysis/instr_tool_selection_final.json` — the 24 valid runs. Every run has cache class
  STATIC (first request reads only the shared static prefix, no later request reads cache it did not
  write in this run); runs in another class would be reported but never pooled.
- `analysis/instr_tool_selection_t1_degraded_observation.json` — 12 T1 runs in which the daemon's
  rust-analyzer could not start (the harness had dropped `RUSTUP_HOME`/`CARGO_HOME`): Brainprint
  returned 4 of 8 call sites and said so (`UnconfirmedCallerOwners: 3`). Kept as an observation, not pooled.
- Harness defects found and fixed during this experiment (their runs were discarded, none is in the
  numbers): (1) the per-run daemon restart lacked `R`/`SP`, so B runs had no daemon (transport_error);
  (2) the restart lacked `RUSTUP_HOME`/`CARGO_HOME` (degraded T1 above); (3) reusing a worktree path
  with a stale `brainprint init` registry entry makes `init` fail — reset `~/.brainprint` data of the
  isolated HOME between blocks; (4) a readiness guard now aborts a B run whose daemon does not
  answer a structural query; (5) a run must never be started twice (macOS has no `setsid`; use
  `start_new_session`).


## Multi-task session experiment (base `4b20e91`)

Harness/measurement only; no product code, packet contract or earlier result changed.

- **C1 as B's default execution instruction (B1).** The text of `harness/instr.md` (unchanged from the
  `ed2abd0` experiment) is appended to the *measured B worktree's* `CLAUDE.md` by `harness/run_multi.sh`.
  It is not written to the shipped `integrations/brainprint/SKILL.md` or to the repository `CLAUDE.md`
  (putting it into the shipped skill/bootstrap is a separate product decision). Correctness/freshness rules are
  kept in the text ("keep judging every answer by its currentness and coverage/limits, fall back to native tools").
- `harness/session.py` — one Claude Code process (`-p --input-format stream-json`), six user turns sent one after
  the other, each after the previous `result`; samples RSS of the Brainprint processes every 0.5 s.
  `harness/run_multi.sh` (one session in a fresh worktree at `4b20e91`, `benchmarks/i5-task14/` removed for A and B),
  `run_multi_all.sh` (8 sessions), `multi_compare.py` (per-turn cost split, grading, pairing).
- Tasks T1..T6 (`prompts.json`, ground truth in `gt.md`); orders `O1 = T1..T6` and `O2 = T6..T1`.
  Sessions: A and B1 x O1/O2 x 2 rounds (ABBA over the two rounds) = 4 sessions per condition.
- `analysis/multi_task_session_4b20e91.json` (+ `_summary.txt`) — per session and per turn: cost, tokens by type, cost split
  (BASE/SETUP/BP_RESULT/NATIVE_RESULT/OTHER/OUTPUT), ToolSearch calls/tokens, Brainprint/native calls and bytes, latency, RSS, grade.
- Measurement notes: `total_cost_usd` in the stream is *cumulative over the session*, `usage` is per turn (the turn cost is the
  difference; checked against tokens x fitted prices, residual < 0.01 USD). The grader attributes a bare `:NNN` to the last
  file named before it and was fixed once after the first A session showed false negatives (answers correct, grader too strict);
  `false_calls` (look-alike sites cited anywhere) is informational and was read by hand. Harness defects: a first attempt of
  session `B1-O1-r2` was discarded (a turn hit the 900 s limit while the machine slept: T6 never returned); its rerun aborted once
  on the stale-registry guard (defect 3 above) and was then run with a reset registry. Both are excluded; the numbers are from the
  completed rerun.
