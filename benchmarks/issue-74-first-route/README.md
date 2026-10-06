# #74 — first-route exact short-circuit · native tool → Brainprint (2026-10-06)

Measurement only. Production behaviour is unchanged: no change to `brainprint-agent`, the #26 guard, the #30 Gateway,
the four-tool MCP surface (#25), the DB or the schema. Arm S is a test-only Claude Code `PreToolUse` command hook
(`harness/s_hook.py`) that calls the shipped `brainprint` CLI (existing local IPC) of product `0e3f182`
(protocol 14; master `7be6060` has the same `crates/` and `integrations/`).

| arm | model-visible | Brainprint |
|---|---|---|
| N | native tools | none (the #35 N arm unchanged) |
| S | native tools only: no MCP server, no SKILL.md, no shipped hooks, no `brainprint` on the Agent's PATH | daemon indexes the clone; pre-tool hook short-circuits Read/Grep/Glob only when exact + current + complete |

## Phase 0 — client capability (`analysis/phase0.json`)

Claude Code 2.1.291 (docs: code.claude.com/docs/en/hooks) and Codex CLI 0.157.1 (`codex-rs/hooks/schema/generated/*`
at tag `rust-v0.157.1`):

| | Claude Code 2.1.291 | Codex 0.157.1 |
|---|---|---|
| pre-tool event before execution | yes (no PostToolUse fired for denied calls) | yes (no PostToolUse fired for the denied call) |
| exact native input | `tool_input` (Read `file_path/offset/limit`, Grep `pattern/path/output_mode/-i/...`, Glob `pattern/path`) | `tool_input.command` — every exploration observed was `Bash` (`rg --files`, `rg -n`, `cat`) |
| block native execution | `permissionDecision: deny` | `permissionDecision: deny` / `decision: block` |
| synthetic successful result | none documented in PreToolUse; `PostToolUse.updatedToolOutput` runs after the tool | none; `updatedMCPToolOutput` is MCP-only |
| substitute delivery | `permissionDecisionReason` → tool_result `is_error: true`, text `PreToolUse:<Tool> hook error: <reason>`; `additionalContext` on deny is also delivered (transcript attachment next to the error) | `permissionDecisionReason` reached the model |
| deterministic fixture | correct answer, 2 turns, no retry, no other route (C1–C4) | correct answer, no retry (X2) |
| subagent attribution | `agent_id`/`agent_type` documented; NOT_MEASURED (`Agent`/`Task` are disallowed in the #32 discipline) | `agent_id`/`agent_type` in input schema; NOT_MEASURED |

C3 (real Brainprint): Read `crates/agent/src/util.rs` 15–30 → 1 Brainprint query (18,606 B internal), 731 B
substitute, hook 131 ms, native Read not executed, answer correct, no retry. C4 (natural prompt): two Grep calls both
substituted (3 queries), no retry, no reroute, answer correct.

Verdict: Claude Code PASS (deny + reason is usable; the client labels it a hook error, the Agent treats it as the
answer). Codex: the mechanism works, but its only native exploration route is shell (`Bash`), and mapping shell to
the P0 actions is out of scope (#74: no arbitrary shell, no I6) — Codex S = unsupported for this gate; Phase 2 is
Claude Code only, the #32/#35 client.

## Phase 1 — deterministic correctness (`analysis/phase1.json`, `phase1_native.json`)

`harness/phase1.py` feeds synthetic PreToolUse events (fixture workspace with hidden, gitignored, docs and nested
files) to the hook; `harness/phase1_native.py` runs every static substitute case as the real Claude Code native tool
(PostToolUse logger) and compares the facts.

| action | cases | substitute | native fallback (closed reason) |
|---|---|---|---|
| SOURCE_READ | 10 | exact bounded range; method inside impl | RANGE_NOT_COVERED (larger, different, declaration end line, no source spans), UNBOUNDED_RANGE, MISSING, OUTSIDE_WORKSPACE, UNSUPPORTED_ARG |
| TEXT_SEARCH | 18 | literal, regex, case-sensitive, `-i`, count, single file, no match, cwd drift, hidden dir, gitignored dir | UNSUPPORTED_ARG (`-C`, `glob`, `type`), UNSUPPORTED_PATTERN (anchors), MULTILINE_MATCH, UNIVERSE_MISMATCH (root, cwd drift to root), OUTSIDE_WORKSPACE |
| PROJECT_TREE_DISCOVERY | 9 | `**/*.rs`, `**/*`, `*.rs`, changed scope, hidden dir, gitignored dir | UNIVERSE_MISMATCH (root: `.git`), UNSUPPORTED_GLOB (path in pattern, alternation) |
| unrelated (Bash, ToolSearch, Edit) | 3 | — | UNRELATED, 0 Brainprint queries |
| mutations ×{immediate, settled} | 14 | M1 settled, M2, M3, M4: equal to disk at hook time | M1 immediate RANGE_NOT_COVERED; unreadable file NOT_CURRENT; unreadable dir UNIVERSE_UNREADABLE |

Final run: expectation 40/40, substitutes vs real native output 17/17 equal, stale substitutes 0, false-complete 0,
Brainprint queries for unrelated actions 0. Fallback for files Brainprint does not cover (`.txt`) is
RANGE_NOT_COVERED, not a substitute.

Found and fixed during Phase 1 (adapter defects, before any economy run; first run kept in `phase1_superseded/`):
native Glob's universe is `rg --files --hidden --no-ignore` (incl. `.git`), native Grep's is `--hidden --glob '!.git'`
(gitignore respected) — the first adapter used plain `rg --files` and produced a false short-circuit (root `**/*.rs`
missed `.hidden/h.rs`, `ignored/x.rs`) and a wrong slash-free `*.rs` depth; both now measured against the client's
own output.

## Phase 2 — Claude Code economy (`analysis/phase2_*`)

`harness/run_s.sh` = #35 `run_contract.sh` arm N unchanged (clone at `6775dac` without `benchmarks/i5-task14`,
`WORKING_STATE.md`, `env -i` allowlist, `claude-opus-5-5`, effort medium, same allow/deny lists, #32 prompts
T5/T6/T1/T7, `session.py`); S adds only the daemon + the hook. Two rounds × {O1, O2}, arm order alternated
(`run_s_all.sh`). Analyzer: #32 `final_analyze.py` unchanged (N staged as its A, S as its B) + `harness/s_analyze.py`.

| session | USD | output tok | cache write | cache read | uncached in | correct | native req / exec | native result B | substitutes | BP queries / B | hook ms | wall s |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| N-O1-r1 | 0.4977 | 10,069 | 25,840 | 447,337 | 40 | 4/4 | 13 / 13 | 26,855 | | | | 103.7 |
| N-O1-r2 | 0.4904 | 8,289 | 28,927 | 465,270 | 34 | 4/4 | 13 / 13 | 32,500 | | | | 93.7 |
| N-O2-r1 | 0.5188 | 9,415 | 29,147 | 485,912 | 38 | 4/4 | 16 / 16 | 28,631 | | | | 105.2 |
| N-O2-r2 | 0.4928 | 8,649 | 25,965 | 559,369 | 46 | 4/4 | 19 / 19 | 25,192 | | | | 91.7 |
| S-O1-r1 | 0.5275 | 10,018 | 28,288 | 503,365 | 36 | 4/4 | 14 / 14 | 31,011 | 0 | 1 / 76,814 | 46 | 101.8 |
| S-O1-r2 | 0.5969 | 11,361 | 31,712 | 579,067 | 40 | 4/4 | 16 / 16 | 32,258 | 0 | 1 / 76,814 | 67 | 108.4 |
| S-O2-r1 | 0.5006 | 9,389 | 26,305 | 511,064 | 40 | 4/4 | 16 / 16 | 25,308 | 0 | 312 / 780,792 | 4,719 | 102.4 |
| S-O2-r2 | 0.5259 | 9,351 | 30,200 | 485,497 | 38 | 4/4 | 15 / 14 | 30,007 | 1 (TEXT_SEARCH, 469 B) | 154 / 497,002 | 2,414 | 97.7 |

- Mean USD/session: N 0.4999, S 0.5377 (S−N +0.038). Pairs S−N: O1-r1 +0.030, O1-r2 +0.107, O2-r1 −0.018,
  O2-r2 +0.033 — S above N in 3 of 4.
- Correctness 16/16 both. False short-circuit 0 (the one substitute equals ripgrep on the clone). Immediate retry 0,
  equivalent native rediscovery 0.
- Native execution: N 61 calls; S 61 requested, 60 executed (−1 over four sessions). The first exploration route was
  `Bash` in 4/4 S sessions (3/4 N); Bash is 52 of S's 61 native calls (N 46/61) — outside P0.
- P0 events in S: 9 of 61. Fallbacks: UNIVERSE_MISMATCH 4 (root scope: native Grep also searches `.brainprint/` —
  Brainprint's own databases, present only because Brainprint is installed — and `web/build/`, which Brainprint
  does not index), RANGE_NOT_COVERED 3 (T5 `crates/engine/src/config.rs` 143+190 / 140+193 lines: a span across many declarations, comments and blank lines that Brainprint source spans do not cover line-for-line), UNSUPPORTED_ARG 1
  (`glob`).
- Root-scope proof costs: 154–312 Brainprint queries, 0.5–0.8 MB, 1.5–1.7 s per hook (one `find files` page per
  directory: the CLI caps listings at 200 with no continuation); hook time is not model cost.
- `split_usd_session_mean` (#32 categories): NATIVE_RESULT N 0.174 / S 0.193, OUTPUT N 0.182 / S 0.201 — the S
  surplus is Agent-behaviour variance (S made more post-processing Bash: 30 vs 20), not hook payload (469 B total).
- Not measured: per-hook model latency (client does not report), subagent attribution.

Discarded (harness defects, not in the tables): the first Phase 2 attempt (`N-O1-r1`, `S-O1-r1`) — the hook took the
event `cwd` as the Workspace root (the Agent's `cd crates/agent` turned a root search into OUTSIDE_WORKSPACE) and the
inventory query hit the 200-entry cap; both fixed (Phase 1 cases G15/G16) and the batch rerun from scratch.

## Decision

STOP (#74 hard stops 4/5). Phase 0 PASS (Claude Code), Phase 1 PASS, Phase 2: S end-to-end cost is not below N
(0.538 vs 0.500 per session; above N in 3 of 4 pairs) and native execution barely changes (60 vs 61) because the
Agent's exploration under the #32 tasks is compound shell, and the Read/Grep calls it does make are mostly not
exactly substitutable (whole-file ranges, root scopes whose native universe includes `.brainprint/`). No production
change; #26/#30 unchanged; shipped four-tool MCP kept; no further hook/schema/payload candidate.
