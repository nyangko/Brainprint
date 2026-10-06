# #35 — Agent ↔ Brainprint integration contract candidates (2026-10-06)

Measurement only. The production MCP surface (exactly four tools, #25) is unchanged; every candidate runs as
`harness/proxy.py` in front of the shipped `brainprint-mcp` of product `0e3f182` (protocol 14), so typed outcomes,
currentness, coverage/gaps, continuation checks and source verification are the product's own. The proxy keeps no
cursor or session map: B's continuation token and E's handle are stateless encodings of what the caller would echo.

| arm | contract |
|---|---|
| N | no Brainprint (the #32 A arm) |
| A | shipped: four tools, shipped hooks + SKILL.md (the #32 B arm) |
| B | reduced typed four tools: no `workspace_*` (startup cwd, the product's own fallback), no correlation fields (integration-injected), no `max_items`/`max_bytes` (profiles stay), continuation as one opaque `bpc1.` string |
| C | 14 typed operation tools, each with exactly its operation's fields and the shipped vocabulary/docs |
| D | `brainprint.call {operation, arguments}` + `brainprint.contract {operation}` (lazy per-operation contract = C's schema, validated before execution) |
| E | the shipped four + `brainprint.expand {handle}`: projected answers compact (no source text, evidence spans, hashes; every outcome/currentness/coverage/gap field kept) with a stateless handle that re-runs the request and reports `unchanged`/`changed` |

C/D/E rename tools, so their SKILL.md lists their tools (`harness/skills/<arm>/SKILL.md`, otherwise the shipped
text); the hook bridge and its bootstrap text are the shipped ones for every arm.

## Phase 1 — deterministic (no model)

`harness/phase1.py` -> `analysis/phase1.json`, `analysis/phase1_tables.md`. Exact UTF-8 bytes of `tools/list` as
served and of every argument/result of the eight #35 workloads (D adds one contract lookup per operation per
workload, E one expand where the workload needs source). Warm-up pass first (discarded) so every arm sees the same
warm daemon. Tokens: NOT_MEASURED.

| arm | tools | schema B | contract B (name+desc+schema) |
|---|---|---|---|
| A | 4 | 24,069 (= #25 record) | 25,209 |
| B | 4 | 14,625 | 15,765 |
| C | 14 | 35,524 | 37,214 |
| D | 2 | 686 | 1,247 (+ 1.1–5.8 KB per looked-up operation contract) |
| E | 5 | 24,150 | 25,507 |

Workload tool I/O vs A: B +0–2.4%, C −0.3–0%, D +3–89% (lookups, 1–8 extra round trips), E +0–71% (expand on
every source need; summaries alone are 29–34% smaller for inspect/change/impact). Payload parity with A: B, C, D
and E-expanded 100% of A-equivalent answers equal (B modulo the opaque token); E expand `unchanged` throughout.
Invalid calls 0 in the scripted replay.

## Phase 2 — Claude Code (2.1.290, `claude-opus-5-5`, medium)

`harness/run_contract.sh` = `run_final.sh` of #32 per arm (same four tasks T5/T6/T1/T7, prompts, clone at
`6775dac`, Working State, flags, env allowlist), round 1: all six arms × {O1, O2}; round 2 (confirmation of the
leading candidate): N, A, B × {O1, O2} in reversed arm order. `harness/analyze_arms.sh` runs the unchanged #32
analyzer per arm (N as its baseline) -> `analysis/phase2_N_vs_<arm>.json` / `_calls.jsonl`;
`analysis/phase2_proxy_calls.jsonl` is the proxy's own call log (invalid / lookup / expand / result bytes).
Preflight per session (`harness/preflight.sh` of #32): 19/19 PASS.

Harness defect, discarded: the first `N-O2-r1` ran with Brainprint connected (arm A had overwritten the shared
`mcp-A.json`, the native arm's empty MCP config). Its result was removed, the per-arm config renamed
(`mcp-arm-<X>.json`), and `N-O2-r1` run again (0 Brainprint references). N sessions in the tables are all clean.

USD per four-task session (client `total_cost_usd`; split as #32: BASE / SETUP=ToolSearch / BP_RESULT /
NATIVE_RESULT / OUTPUT):

| arm | n | mean [sessions] | −N | SETUP | BP_RESULT | NATIVE | BP calls / KB | ToolSearch tok | correct |
|---|---|---|---|---|---|---|---|---|---|
| N | 4 | 0.556 [0.543 0.618 0.541 0.521] | | 0 | 0 | 0.208 | 0 | 0 | 16/16 |
| A | 4 | 1.000 [1.048 0.885 1.165 0.901] | +0.444 | 0.102 | 0.380 | 0.156 | 13.3 / 68.7 | 8.7k | 15/16 |
| B | 4 | 0.878 [0.783 1.070 0.817 0.842] | +0.322 | 0.075 | 0.296 | 0.160 | 10.5 / 54.8 | 6.5k | 16/16 |
| C | 2 | 0.858 [0.771 0.945] | +0.302 | 0.110 | 0.255 | 0.140 | 8.5 / 46.7 | 10.1k | 8/8 |
| D | 2 | 1.046 [0.963 1.129] | +0.490 | 0.009 | 0.481 | 0.148 | 16 / 86.7 | 0.7k | 8/8 |
| E | 2 | 0.901 [1.025 0.777] | +0.345 | 0.122 | 0.284 | 0.153 | 9 / 46.8 | 10.6k | 8/8 |

(Full tables incl. tokens, per task, auto-class: `analysis/phase2_tables.md`.)

- Every candidate is above N in every session; the smallest pair difference of any candidate is +0.24 (B O1-r1).
- B vs A (same rounds): lower in 3 of 4 pairs (1.070 vs 0.885 in O1-r2); range 0.78–1.07 overlaps A's 0.89–1.17.
- D: base schema −98%, but 7 contract lookups and 2 invalid calls in 2 sessions, and the agent made the most
  Brainprint calls (16/session); SETUP falls by 0.09, BP_RESULT rises by 0.10.
- E: 0 expands in Phase 2 (the agent never expanded; it read native files after compact answers instead —
  FALLBACK_PARTIAL 11 in 2 sessions vs A's 12 in 4); SETUP rises (fifth tool).
- C: no invalid calls; 14 tools cost more ToolSearch than A (10.1k vs 8.7k tokens).
- A's one wrong answer (A-O1-r1 T5, wrong file:line of the version constant) is the same off-by-one class as #32.

## Decision

STOP (issue hard stop). No candidate brings Brainprint near the native arm: the lowest, B/C, still cost
+0.30–0.32 USD per session over N (+54–57%). B is the only candidate measured in both rounds and is lower than the
shipped A on average (−0.12) but not in every pair, and its range overlaps A's — not a clear winner. No production
contract change is proposed; no further candidate is added.
