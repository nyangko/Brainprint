# #75 — adaptive minimum-work routing · N vs B vs R (2026-10-06)

Measurement only; no production routing, MCP, hook, DB or schema change. Workload, routing table, directives and
graders were committed in `00c95e4` (`manifest.json`, `harness/prompts_*.json`) before any measured run; no dry
run of a model session was made, so there is no superseded batch.

- product `0e3f182` (protocol 14; master `crates/`, `integrations/`, `web/` identical), clone base `6775dac`
  (#32/#35/#74), Claude Code 2.1.291, `claude-opus-5-5`, effort medium, #35 flags / env allowlist / allow-deny lists.
- **N**: no Brainprint (`WORKING_STATE.md`). **B**: shipped surface (brainprint-mcp four tools, shipped Claude hooks in
  prefer mode, SKILL.md, Brainprint Working State) + `Route: BRAINPRINT` on every request. **R**: the same surface as B
  for every request (also NATIVE ones) + the fixed per-request directive `Route: REUSE | NATIVE | BRAINPRINT`
  (`manifest.json` → `arms.R.route`). Directive tokens are part of B/R cost.
- Tasks: class A = A1 file list, A2 bounded read, A3 literal search, A4 symbol location; class B = #32 T5, T6, T1;
  class C = T7 (resume the Working State WorkItem), W6/W1 (repeat of T6/T1 late in the same session),
  and T7/T6 in a second and a third Agent (new Claude process, same clone / daemon / index / Working State).
- Sessions per arm and round: S1 = A1 T5 A2 T6 A3 T1 A4 T7 W6 W1 (cold = turn 1, W* = long-session reuse, rest
  warm), S2 = S3 = T7 T6 A1. 2 rounds (r1 N B R, r2 R B N) → 18 sessions, 96 turns, all exit 0, git status clean.
- Analysis: `harness/analyze.py` over the #32 analyzer (`multi_compare.analyze`, `final_analyze` graders unchanged;
  A-task graders in the manifest). Observed route per turn: `REUSE` (no exploration call), `NATIVE`, `BRAINPRINT`,
  `FALLBACK(NATIVE)` (native exploration after the turn's first Brainprint call = equivalent native rediscovery).

## By class (USD per round = sum over the class's turns in one round; r1 / r2)

| class | arm | USD r1 / r2 | correct | BP calls | native calls | native after BP | cache write | cache read | output | wall s |
|---|---|---|---|---|---|---|---|---|---|---|
| A direct | N | 0.225 / 0.204 | 12/12 | 0 | 12 | 0 | 28,419 | 712,597 | 2,954 | 51.6 |
| | B | 0.466 / 0.498 | 10/12 | 12 | 0 | 0 | 64,897 | 1,808,251 | 4,158 | 80.6 |
| | R | 0.269 / 0.284 | 12/12 | 0 | 12 | 0 | 31,073 | 1,236,019 | 2,846 | 52.3 |
| B composite | N | 0.798 / 0.752 | 10/10 | 0 | 52 | 0 | 87,963 | 1,433,398 | 27,978 | 275.5 |
| | B | 1.908 / 2.227 | 7/10 | 52 | 21 | 21 | 328,809 | 3,890,261 | 36,321 | 431.2 |
| | R | 1.363 / 1.658 | 9/10 | 35 | 40 | 40 | 203,298 | 3,450,735 | 35,175 | 377.8 |
| C continuity | N | 0.355 / 0.374 | 10/10 | 0 | 23 | 0 | 29,900 | 957,061 | 14,911 | 158.1 |
| | B | 0.810 / 0.886 | 7/10 | 31 | 12 | 12 | 88,544 | 2,911,401 | 20,237 | 230.3 |
| | R | 0.698 / 0.748 | 10/10 | 28 | 14 | 14 | 86,827 | 1,951,923 | 17,998 | 187.2 |
| **all** | N | 1.378 / 1.330 | 32/32 | 0 | 87 | 0 | | | | 242.6 / round |
| | B | 3.184 / 3.611 | 24/32 | 95 | 33 | 33 | | | | 371.0 / round |
| | R | 2.330 / 2.689 | 31/32 | 63 | 66 | 54 | | | | 308.6 / round |

Uncached input: N 108, B 138, R 142 tokens per round (all arms ~0). Columns other than USD/wall are two-round sums.

## By condition (two-round sums)

| condition | N USD | B USD | R USD | N / B / R correct | R observed route |
|---|---|---|---|---|---|
| cold (S1 turn 1, A1) | 0.118 | 0.202 | 0.126 | 2/2 · 2/2 · 2/2 | NATIVE 2 |
| warm (S1 turns 2–8) | 1.312 | 3.532 | 2.365 | 14/14 · 8/14 · 13/14 | NATIVE 6, FALLBACK(NATIVE) 8 |
| long session reuse (W6, W1) | 0.198 | 0.385 | 0.198 | 4/4 · 2/4 · 4/4 | REUSE 4 |
| second Agent (S2) | 0.636 | 1.540 | 1.213 | 6/6 · 6/6 · 6/6 | FALLBACK(NATIVE) 4, NATIVE 2 |
| third Agent (S3) | 0.445 | 1.137 | 1.118 | 6/6 · 6/6 · 6/6 | FALLBACK(NATIVE) 4, NATIVE 2 |

## Routing (R) and violations

- Route violations: 0 in R and B (no Brainprint call / ToolSearch in an R NATIVE turn; no native-first BRAINPRINT
  turn; no exploration in an R REUSE turn). `REUSE MISS` declared: 0.
- R BRAINPRINT turns (class B + T7): 16 of 16 ended `FALLBACK(NATIVE)` — Brainprint first, then native
  exploration of the same facts (54 native calls after a Brainprint call). B: of its 20 composite/continuity turns
  12 fell back, 6 stayed Brainprint-only, 2 needed no exploration.
- R NATIVE turns (12): native only, 0 Brainprint calls, but each turn still carries the registered MCP/hook surface:
  R class A costs 0.269 / 0.284 vs N 0.225 / 0.204 (cache read 1.24 M vs 0.71 M tokens).
- R REUSE turns (W6, W1): 4/4 answered from context without exploration; N, without any directive, answered 2 of 4 from
  context and 2 with one native call each; cost equal (0.198 vs 0.198).

## Correctness

- N 32/32. R 31/32: R-r2-S1 T5 cites `config.rs:18` for `CONFIG_FORMAT_VERSION` (truth :19) — Brainprint's 0-based
  line passed through, the same off-by-one class as #32 / #35 A-O1-r1.
- B 24/32: all 8 failures in B-r2-S1 (T5, T6, A3, T1, A4, T7, W6, W1), the same 0-based-line class throughout the
  session (`util.rs:14`, `telemetry.rs:24`, `config.rs:18/40/133`); the other five B sessions are 16/16 correct.
- Currentness: read-only clones, no edits; every Brainprint answer reported Current.

## Overhead (not model cost)

- Brainprint RSS peak (daemon + MCP + agent) 34–39 MiB per B/R session; `.brainprint/` 58–59 MiB per clone.
- Shipped hook telemetry (B and R): decisions `allow` and `advise` only (no deny), p50 1.6–2.3 ms, max 70 ms (`analysis/hook_telemetry_summary.json`).
- Index build / init time: outside the sessions, NOT_MEASURED here. CPU / I/O: NOT_MEASURED.

## Observed trade-off (no single winner score)

- **A direct lookup:** N < R < B. R routed every direct lookup native with 0 Brainprint calls (routing success);
  it still costs +0.04–0.08 per round over N for the registered surface. B costs ~2.2× N.
- **B composite:** N < R < B. Brainprint did not replace native exploration: every R composite turn fell back to
  native after Brainprint (40 native calls vs N 52), so R paid for both. R −0.56 USD/round vs B, +0.74 vs N.
- **C continuity:** N < R < B. Working State resume (T7) and second/third Agent cost more with Brainprint than with
  N's `WORKING_STATE.md` file and native search (T7: N 0.53, R 1.25, B 1.31 for 6 runs). Long-session repeated facts:
  R = N (both reuse from context).
- **R vs B:** R is cheaper than B in every class and condition (−0.86 / −0.92 USD per round) and more correct
  (31/32 vs 24/32). **R vs N:** R is more expensive than N in every class and condition (+0.95 / +1.36 per round).
  No interval where Brainprint overhead amortized against N was observed (agent2 → agent3 R 1.213 → 1.118 vs N
  0.636 → 0.445).
- Wall time per round: N 243 s, R 309 s, B 371 s — no cost/time trade-off in favour of B or R.

By #75's interpretation rule: adaptive routing is better than forced Brainprint; it is not cheaper than native in
any measured class or condition. No production change; production adaptive routing stays a separate explicit
decision.
