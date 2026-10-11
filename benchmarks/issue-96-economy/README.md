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
- `harness/run_all.sh` (r1: O1 N H M, O2 M H N), `harness/run_r2.sh` (r2: O1 H2 N, O2 N H2; M unchanged, not rerun).
- `harness/analyze96.py` — the #32 analyzer (`multi_compare.analyze`, `final_analyze` grades, CACHE_MISS)
  unchanged, plus one rule: a Bash call running `brainprint` is a Brainprint call (`BP:cli:*`), booked as
  BP_RESULT. Prices: the #32 fit (input $4, cache-write $8, cache-read $0.20, output $20 per 1M).
- `analysis/sessions_r1_r2.json` (per session / turn: cost, tokens by type, cost split, calls, grade);
  `analysis/M-*.telemetry.jsonl` (prefer-hook telemetry of the M sessions). No transcripts are kept.

## Result (USD per 4-task session; all 10 sessions cache class STATIC)

| arm | sessions | USD mean | each | correct | native calls / session | native after BP (total) | BP calls / bytes per session | SETUP (ToolSearch) | BP_RESULT | NATIVE_RESULT | OUTPUT |
|---|---|---|---|---|---|---|---|---|---|---|---|
| N | 4 | 0.525 | 0.550 / 0.533 / 0.488 / 0.530 | 16/16 | 15.5 | 0 | 0 | 0 | 0 | 0.190 | 0.188 |
| H (r1) | 2 | 0.514 | 0.648 / 0.381 | 8/8 | 8.0 | 13 | 7.5 / 8.2 KB | 0 | 0.051 | 0.165 | 0.153 |
| H2 (r2) | 2 | 0.449 | 0.486 / 0.412 | 8/8 | 12.0 | 20 | 5.0 / 6.3 KB | 0 | 0.037 | 0.122 | 0.140 |
| M (r1) | 2 | 0.950 | 0.946 / 0.954 | 8/8 | 12.5 | 25 | 10.0 / 60.8 KB | 0.123 | 0.315 | 0.135 | 0.203 |

Pairs (same order and round), H − N: r1 O1 +0.098, r1 O2 −0.152; H2 − N: r2 O1 −0.002, r2 O2 −0.118.

Per task (mean USD; position in the session in brackets where it decides):

| task | N | H + H2 | M |
|---|---|---|---|
| T1 change context (Rust callers + tests + rules) | 0.169 | 0.122 (H2 0.102) | 0.238 |
| T5 config explanation (native in N and H) | 0.114 | 0.112 | 0.198 |
| T6 callers of `fingerprint` | 0.137 | 0.133 | 0.275 |
| T7 resume | 0.106 (4th 0.078, 1st 0.134) | 0.115 (4th 0.114, 1st 0.117) | 0.239 |

- T1: H2 is below every N session (0.108 / 0.096 vs 0.159-0.184). The r1 H O1 T1 (0.194) guessed
  `brainprint context rules` and re-read three further-hop rows with `sed` because the compact row did not name
  their callee — both fixed for H2.
- T6: Brainprint answers `Partial` (`unconfirmed owners` reported), so the
  Agent re-checks with one text search, as `CLI.md` tells it to. Correct, not a saving: H ≈ N.
- T7: no saving. As the 4th task H costs +0.03-0.05 over N in both rounds; as the 1st it costs less.
- M reproduces #32 (0.950 vs #32's 0.998 / 0.974): schema load 0.123 and Brainprint results 0.315 per session.
- Sample: two sessions per H arm. H2 ≤ N in both of its pairs, but the variance between sessions of one arm is
  up to 0.27 USD (H r1); more replicates are needed before a release claim.

## Codex (capability, not economy)

`codex sandbox` (codex-cli 0.157.1, seatbelt) running the CLI against the isolated daemon: the default
read-only and `workspace-write` sandboxes refuse the daemon socket (`communication with brainprintd failed:
Operation not permitted`); with `sandbox_workspace_write.network_access=true` the answer arrives. An end-to-end
Codex session was not run: the user's `~/.codex/config.toml` default model is rejected for this account, and the
config was not changed. Codex economy: NOT_MEASURED.
