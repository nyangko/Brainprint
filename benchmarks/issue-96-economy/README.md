# #96 — token economy: CLI path (H) vs native (N) vs shipped MCP (M) (2026-10-11)

Product baseline: RC `300b2ffc` (kept unchanged as the technical baseline). Product under test: the
working tree on top of it, with the #96 CLI change (`crates/cli/src/query/{render,exec,target,delivery}.rs`)
and `integrations/brainprint/CLI.md`. Measured snapshot: `6775dac` without `benchmarks/i5-task14`, the same
as #32/#35/#74/#75, so `harness/gt.md` (copied unchanged) stays the ground truth.

| arm | model-visible surface |
|---|---|
| N | native tools only; T7 gets `WORKING_STATE.md` (the #32 arm A) |
| M | shipped: `brainprint-mcp` (4 tools), Claude prefer hooks, `SKILL.md` (the #32 arm B) |
| H / H2 | no MCP, no hooks, no skill; `brainprint` on PATH; `integrations/brainprint/CLI.md` appended to the snapshot's `CLAUDE.md` |

H2 is H after the r1 review: further hops say `-> callee`, and `CLI.md` names `brainprint knowledge rules`.

- `harness/run.sh` — one session = one Claude Code process, the #32 four tasks in a row (`prompts_final.json`,
  O1 = T5 T6 T1 T7, O2 = T7 T1 T6 T5). Claude Code 2.1.296, `claude-opus-5-5`, `--effort medium`,
  `--setting-sources project`, `--strict-mcp-config`, `env -i` allowlist, the #32 allow/deny tool lists.
  Brainprint runs under an isolated `HOME`/`XDG_RUNTIME_DIR` (`BRAINPRINT_NO_AUTOSTART=1`); the user's daemon,
  DB and installed binaries are never reached. H's `brainprint` is a wrapper that only pins that runtime.
- `harness/run_all.sh` (r1: O1 N H M, O2 M H N), `harness/run_r2.sh` (r2: O1 H2 N, O2 N H2; M unchanged, not rerun),
  `harness/run_r34.sh` (r3/r4: H2 vs N, pre-registered before running).
- `harness/analyze96.py` — the #32 analyzer (`multi_compare.analyze`, `final_analyze` grades, CACHE_MISS)
  unchanged, plus one rule: a Bash call running `brainprint` is a Brainprint call (`BP:cli:*`), booked as
  BP_RESULT. Prices: the #32 fit (input $4, cache-write $8, cache-read $0.20, output $20 per 1M).
- `analysis/sessions_r1_r4.json` (per session / turn: cost, tokens by type, cost split, calls, grade);
  `analysis/M-*.telemetry.jsonl` (prefer-hook telemetry of the M sessions). No transcripts are kept.

## Result (USD per 4-task session; all 18 sessions cache class STATIC)

Rounds: r1 (N H M), r2 (H2 N), r3 and r4 (H2 N, `harness/run_r34.sh`, arm order alternated, product `b0d2222`
unchanged). H2 vs N is the pre-registered comparison; H and M are r1 only.

| arm | sessions | USD mean (sd) [min-max] | correct | native calls / session | native after BP (total) | BP calls / bytes per session | SETUP (ToolSearch) | BP_RESULT | NATIVE_RESULT | OUTPUT |
|---|---|---|---|---|---|---|---|---|---|---|
| N | 8 | **0.563** (0.059) [0.488-0.667] | 31/32 | 15.3 | 0 | 0 | 0 | 0 | 0.226 | 0.191 |
| H2 | 6 | **0.457** (0.053) [0.409-0.520] | 24/24 | 10.3 | 49 | 4.7 / 6.0 KB | 0 | 0.047 | 0.120 | 0.145 |
| H (r1) | 2 | 0.514 [0.381-0.648] | 8/8 | 8.0 | 13 | 7.5 / 8.2 KB | 0 | 0.051 | 0.165 | 0.153 |
| M (r1) | 2 | 0.950 [0.946-0.954] | 8/8 | 12.5 | 25 | 10.0 / 60.8 KB | 0.123 | 0.315 | 0.135 | 0.203 |

H2 − N per pair (same order and round): r2 O1 −0.002, r2 O2 −0.118, r3 O1 −0.062, r3 O2 −0.129, r4 O1 −0.111,
r4 O2 −0.259. H2 below N in **6/6** pairs; mean −0.114 (sd 0.085), paired t = −3.25 (5 df). Round 1's H (before
the r1 fixes) was above N in one of its two pairs (+0.098).

Tokens per session (mean): N cache-write 33.6k / cache-read 517k / output 9.5k; H2 27.6k / 454k / 7.3k;
M 67.5k / 1.03M / 10.2k.

Per task, H2 vs N (mean USD, 6 vs 8 turns):

| task | N | H2 | M (r1) |
|---|---|---|---|
| T1 change context (Rust callers + tests + rules) | 0.173 | **0.111** | 0.238 |
| T5 config explanation (native in both N and H2) | 0.124 | 0.107 | 0.198 |
| T6 callers of `fingerprint` | 0.161 | 0.132 | 0.275 |
| T7 resume | 0.105 | 0.107 | 0.239 |

- T1 is the clear Brainprint-eligible gain (answer-ready callers with owners, `-> callee` on further hops).
  The r1 H O1 T1 (0.194) guessed `brainprint context rules` and re-read three further-hop rows with `sed`
  because the row did not name the callee; both were fixed for H2.
- T6: Brainprint answers `Partial` (`unconfirmed owners` reported), so the Agent re-checks with text search as
  `CLI.md` tells it to. Cheaper than N here, but it still pays for both.
- T7 resume: **no gain** (0.107 vs 0.105). T5 uses no Brainprint in either arm; its gap is session context,
  not a Brainprint effect.
- Native exploration after a Brainprint call: 49 calls over 6 H2 sessions remain, mostly Partial re-checks.
- Correctness: H2 24/24. N 31/32: `N-O2-r4` T5 cites `config.rs:18` for `CONFIG_FORMAT_VERSION` (line 19)
  after reading the file natively -- the native arm's own off-by-one, not a Brainprint answer.
- M reproduces #32 (0.950 vs #32's 0.998 / 0.974): schema load 0.123 and Brainprint results 0.315 per session.

## Codex (capability, not economy)

`codex sandbox` (codex-cli 0.157.1, seatbelt) running the CLI against the isolated daemon: the default
read-only and `workspace-write` sandboxes refuse the daemon socket (`communication with brainprintd failed:
Operation not permitted`); with `sandbox_workspace_write.network_access=true` the answer arrives. An end-to-end
Codex session was not run: the user's `~/.codex/config.toml` default model is rejected for this account, and the
config was not changed. Codex economy: NOT_MEASURED.
