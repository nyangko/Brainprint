<p align="center">
  <img src="./assets/brainprint-hero.svg" alt="Brainprint" width="100%" />
</p>

<p align="center">
  <strong>A local-first context runtime that keeps current, structured project truth for AI coding agents.</strong>
</p>

<p align="center">
  English · <a href="./docs/README.ko.md">한국어</a> · <a href="./docs/README.ja.md">日本語</a>
</p>

# Brainprint

AI coding agents spend many tool calls and much context rediscovering things ordinary software can prepare: repository structure, unchanged source, callers and imports, project rules, and where the previous session left off.

Brainprint is a **local daemon** (`brainprintd`) that indexes a Workspace, keeps that index current while files change, and serves deterministic answers (resources, symbols, relations, impact, rules, decisions, Working State) to agents over MCP and to people over a CLI, a TUI, and a local Web UI.

> If Brainprint can know, derive, sort, deduplicate, prepare, or verify something deterministically, the agent should not have to spend reasoning on it again.

The project stays the source of truth. Brainprint is not a source backup, a Git replacement, an autonomous project manager, or a conversation archive. It prepares facts; the agent still decides, edits, and verifies.

Principles the implementation follows:

- **Correctness before savings.** `NOT_CURRENT`, `PARTIAL`, unresolved, unsupported and truncated results stay explicit. Hiding uncertainty to save tokens counts as failure.
- **Shared truth, not shared giant context.** Several agents on one Workspace share one daemon and one index instead of each re-exploring.
- **Deterministic work belongs in software.** Sorting, deduplication, bounding, pagination and continuation happen in Brainprint, not in the model.

## Status

**Pre-release. Brainprint 0.1.0 is implemented but not yet accepted or released.** Interfaces, storage and protocol (currently protocol 14) may still change.

| Workstream ([#12](https://github.com/nyangko/Brainprint/issues/12)) | Status |
| --- | --- |
| I0 — Benchmark Harness / Skeleton | Complete |
| I1 — Core Runtime Foundation | Complete |
| I2 — Structural Intelligence | Complete |
| I3 — Relation Graph | Complete |
| I4 — Semantic Backends | Complete |
| I5 — Project Intelligence + Projection + MCP/Skill | Complete |
| I6 — Command Intelligence | Complete |
| I7 — UX / Recovery / Acceptance | In progress — final acceptance gate [#77](https://github.com/nyangko/Brainprint/issues/77), not yet accepted |

## What 0.1.0 guarantees — and what it does not

**0.1.0 aims to guarantee:**

- local-first, shared current truth about a Workspace, held by one daemon per user;
- Project / Workspace / Working State continuity across sessions and agents;
- deterministic preparation of Resources, Symbols, Relations, Policies, Decisions and Working State;
- explicit freshness / currentness / coverage / partial / unsupported semantics on every answer;
- minimum-sufficient, bounded and resumable (continuation) delivery;
- fact-based measurement of its own cost (see [Benchmarks](#benchmark-and-economy-results)).

**0.1.0 does not guarantee:**

- lower model/provider cost (USD) than native-only exploration on every workload — the measurements so far show the opposite (see below);
- fewer native tool calls on every client;
- that MCP / hook / tool-schema overhead is always amortized;
- that a reduction in bytes delivered turns into a token or USD saving.

## Install and build from source

There are no prebuilt binaries or packages yet. Build from source:

```sh
git clone https://github.com/nyangko/Brainprint.git
cd Brainprint
cargo build --release
```

- Toolchain: `rust-toolchain.toml` pins the `stable` channel (with `clippy`, `rustfmt`); the minimum supported Rust is **1.88** (`rust-version` in `Cargo.toml`).
- The Web UI page is committed prebuilt (`web/build/index.html`) and embedded into the `brainprint` binary, so Node is **not** needed to build. Only rebuild it (`cd web && npm ci && npm run build`) if you change `web/src`.
- CI (`.github/workflows/acceptance.yml`) runs fmt, clippy, build and the full test suite on Linux, macOS and Windows.

The build produces four binaries in `target/release/`; put them on your `PATH`:

| Binary | Role |
| --- | --- |
| `brainprintd` | The global daemon, one per user. Runs in the foreground until Ctrl+C. |
| `brainprint` | The CLI (also hosts the TUI and the Web UI). |
| `brainprint-mcp` | MCP server over stdio; a thin adapter to the daemon exposing four tools. |
| `brainprint-agent` | Optional client hook bridge for Claude Code, Codex CLI and Gemini CLI. |

## Quick start

```sh
brainprintd &                 # 1. start the daemon (the CLI never auto-starts it)
brainprint install            # 2. create ~/.brainprint/config.toml and ~/.brainprint/data/global.db
brainprint init ~/src/myrepo  # 3. attach a Workspace: creates <repo>/.brainprint/ and runs the initial index
brainprint status ~/src/myrepo
```

If the daemon is not running, commands fail with `brainprintd is not running -- start it, then retry`. The CLI and daemon must speak the same protocol version (use binaries from the same build); a mismatch is reported, not tolerated.

After `init`, the daemon watches the Workspace and keeps the index current; ordinary edits need no further command. `status <path>` shows identity, revision/generation, `index: current`, `runtime: active`, `watcher: attached`, and each semantic backend's availability.

A first query (most query commands take a target selector, a `--budget` and a `--retention`):

```sh
cd ~/src/myrepo
brainprint inspect --symbol-name build_profile --budget compact --retention fresh
brainprint impact  --symbol-name build_profile --change public-signature --budget compact --retention fresh
brainprint find text --literal build_profile --search-budget compact
```

## CLI reference

Every command has `--help`. Workspace-scoped commands default to the current directory (`[PATH]` or `--workspace`, canonicalized to an absolute path). `--json` prints the machine-readable response.

**Lifecycle and recovery**

| Command | What it does |
| --- | --- |
| `install` | Create the global config and `global.db`. |
| `status [PATH] [--json]` | Daemon status; with a path, the Workspace's compact status. Read-only. |
| `init [PATH]` | Create `.brainprint/` and attach the Workspace (initial index + watcher). On a detached Workspace it re-attaches. |
| `doctor [PATH] [--json]` | Read-only diagnosis: databases and schemas, identity binding, index currentness, runtime/watcher, semantic backends. Never repairs. |
| `sync [PATH] [--json]` | Reconcile the index with the filesystem now (after a checkout/rebase or while the daemon was stopped). Never modifies source. |
| `rebuild [PATH] [--json]` | Rebuild the rebuildable index (Resources, Symbols, Relations, semantic publications) from current source. Identity, config, durable knowledge, Working State and source are kept. |
| `uninit [PATH] [--json]` | Detach: stop the watcher and runtime. Source, `.brainprint/`, durable knowledge and index are kept; `init` attaches again. |
| `tui [--workspace] [--locale]` | Keyboard-first terminal view. |
| `web [--workspace] [--port 7470] [--locale]` | Local read-only Web UI on `127.0.0.1`. |

**Queries** (all accept `--workspace`, `--json`, `--client-id`, `--session-id`)

| Command | What it answers |
| --- | --- |
| `find target <selector>` | Candidates for a selector. Reads no source. |
| `find files [--directory --recursive --path-prefix --role --language --kind --limit]` | Resource inventory from the index (default limit 200). Reads no source. |
| `find text --literal\|--regex … --search-budget compact\|standard\|wide` | Explicit source-text search, bounded by results/files/bytes/deadline. Never an automatic fallback. |
| `inspect <selector>` | Exact current declaration source plus direct relations in both directions. |
| `relations <selector> --direction outgoing\|incoming\|both [--kind …]` | Direct confirmed relations, one hop, unpaged. |
| `impact <selector> --change <kind>` | What a declared change would affect (`public-signature`, `rename`, `module-move`, `base-interface`, `delete`, `domain-contract`), including related tests. |
| `context change <selector>` | Context for an edit at a target. |
| `context resume --work-item <id>` | Context to resume a WorkItem's handoff. |
| `knowledge rules \| work-items --status … \| lineage \| handoffs --work-item …` | Applicable rules; WorkItems by status; one-hop Policy/Decision lineage; handoff history. |
| `structure --group-…` | Structural summary grouped by exactly one dimension: `--group-path LABEL=PREFIX` (repeatable), `--group-directory --root R --depth N`, `--group-resource-role`, `--group-resource-language`, `--group-resource-kind`. |

Target selectors: `--resource-id`, `--resource-path`, `--resource-basename`, `--resource-prefix`, `--symbol-id`, `--qualified`, `--symbol-name`, `--partial-symbol`, or `--target-json`; narrowed with `--in-resource`, `--symbol-kind`, `--language`.

Delivery options on `find target`, `inspect`, `impact`, `context change|resume`:

- `--budget compact|standard|wide` (required), optionally `--budget-items N` / `--budget-bytes N`;
- `--retention retained|fresh|disabled` (required): `retained` = earlier acknowledged payloads are still in the caller's context, `fresh` = new/compacted context, send everything in full, `disabled` = no reuse ledger;
- `--continuation <token>` to fetch the next page after `MORE_AVAILABLE`.

**Working State and verification** (JSON input on stdin or `--input FILE`)

| Command | What it does |
| --- | --- |
| `work start` / `work result` | Record caller-observed Git state for a WorkItem (OPEN → ACTIVE, then partial/complete/abandon). Runs no Git. |
| `verification start --idempotency-key K` | Start a daemon-managed verification Job (`{"commands":[{"label","argv","cwd","env","timeout_secs","capture"}]}`; argv is executed without a shell) and return at once. |
| `verification poll <JOB_ID> [--after N --limit N]` / `verification cancel <JOB_ID>` | Read a Job's events / cancel it. |
| `artifact read <HANDLE> --stream stdout\|stderr --part head\|tail` | Read a retained head/tail chunk of captured command output (ephemeral). |

## MCP and thin Skill integration

`brainprint-mcp` speaks MCP over stdio and exposes four tools:

| Tool | Use |
| --- | --- |
| `brainprint.find` | Location: exact/search target, file listing, or explicit text search (`mode`: target, files, text). |
| `brainprint.inspect` | Exact current source and direct relations of one resolved target. |
| `brainprint.relations` | Direct relations of one anchor, or the impact of a declared change. |
| `brainprint.context` | Edit context, WorkItem resume, applicable rules, WorkItem/Decision/Policy history, structure. |

It never ranks, resolves ambiguity or picks a Working State on its own; typed outcomes such as `NOT_INITIALIZED` or partial coverage are returned as-is. Each call may pass `workspace_path` or `workspace_id`; without either, the server's startup working directory is used. The daemon must already be running.

Register it with any MCP client as a stdio server, for example a project `.mcp.json`:

```json
{
  "mcpServers": {
    "brainprint": { "type": "stdio", "command": "/absolute/path/to/brainprint-mcp", "args": [] }
  }
}
```

The thin Skill is [`integrations/brainprint/SKILL.md`](./integrations/brainprint/SKILL.md): prefer Brainprint for project facts when it answers current and complete; fall back to native tools when it reports partial, stale, unsupported or ambiguous. Install it the way your client loads skills/instructions (for example copy it into the client's skills directory). There is no `brainprint.run` tool.

### Client hook bridge (optional)

`brainprint-agent` translates client hook payloads into a common event and can steer the agent toward Brainprint for supported exploration:

```sh
brainprint-agent config --client claude-code --mode prefer   # prints a hook config fragment; never edits config
brainprint-agent probe  --client claude-code                 # reports the installed client and the capability matrix
```

- Clients: `claude-code`, `codex-cli`, `gemini-cli`. Ready-made fragments live in `integrations/brainprint/clients/`.
- Modes: `observe`, `prefer` (the dogfood default), `guard` (opt-in; can block a native exploration; `bypass-once --session S` allows exactly one).
- Merge the fragment into the client's hook settings yourself.
- Optional local adoption telemetry: set `BRAINPRINT_ADOPTION_TELEMETRY_PATH` to a file to append one JSONL line per event (labels, counts, digests; no prompt, source or tool-result body). Unset = disabled (default).

## TUI and Web UI

- `brainprint tui` — keyboard-first terminal view: status, inspect, relations, impact, Working State, and doctor/sync/rebuild/uninit, over the same daemon queries as the CLI. Quitting never stops the daemon.
- `brainprint web` — local Web UI (Overview; Explorer: inspect / relations / impact; Context: Working State, rules, decisions). Binds **127.0.0.1 only**, default port **7470** (`--port 0` picks a free port), read-only; stopping it never stops the daemon.
- Locale: `--locale en|ko`; an unsupported tag falls back to English. Default comes from the global config:

```toml
# ~/.brainprint/config.toml
format_version = 1
[ui]
locale = "ko"
```

## Supported languages and capability boundary

Baseline: **Python, TypeScript/JavaScript, React (TSX/JSX), Svelte, C#, Rust.** Other files are indexed as resources (path, role, kind) without symbols.

Two layers:

1. **Structural (always on, built in):** tree-sitter extraction of symbols, occurrences, imports and structural relations, with explicit unresolved/candidate evidence where a name cannot be bound structurally.
2. **Semantic (optional, external):** language-server backends you install yourself. Brainprint never downloads or searches for them. With no locator configured, `status`/`doctor` show each backend as `unavailable: no backend locator in the Workspace or global config`, and answers use structural results with explicit coverage limits and gaps.

Backend locators go in `~/.brainprint/config.toml` or, per Workspace (overriding the global entry), in `<workspace>/.brainprint/config.toml`:

```toml
[semantic_backends.python]      # install_root contains node_modules/pyright-typeserver
install_root = "/opt/bp/pyright"
node = "/usr/local/bin/node"    # optional, default "node"

[semantic_backends.typescript]  # install_root contains node_modules/typescript
install_root = "/opt/bp/typescript"

[semantic_backends.svelte]      # install_root contains node_modules/svelte-language-server
install_root = "/opt/bp/svelte"

[semantic_backends.csharp]      # install_root contains packages/microsoft.codeanalysis.languageserver.<rid>/<version>
install_root = "/opt/bp/roslyn"

[semantic_backends.rust]        # path to a rust-analyzer executable
executable = "/opt/bp/rust-analyzer"
```

**C# and Rust load the project** (MSBuild evaluation; Cargo build scripts and proc macros), so they run only after an explicit per-Workspace trust decision in `<workspace>/.brainprint/config.toml`:

```toml
project_execution_trust = "Trusted"
```

Absent means Untrusted; there is deliberately no global equivalent. An untrusted C#/Rust backend is reported unavailable, never started. Backends run as separate child processes; that is crash isolation, not a security boundary.

## Freshness, coverage and continuation

Every answer states how far it can be trusted:

- **Currentness** — `Current` when the index matches the Workspace revision the runtime holds; `NOT_CURRENT` otherwise. Brainprint fails closed rather than presenting stale facts as current.
- **Coverage** — complete / partial / unsupported, with the limits that caused it (for example unresolved evidence, unattributed gaps, a reverse scope that cannot be enumerated). "No result under complete coverage" and "no result with incomplete coverage" are different answers.
- **Bounding** — `PARTIAL`, `TRUNCATED`, `MORE_AVAILABLE` plus a `CONTINUATION` token. Pass the token back with `--continuation` (CLI) or the continuation parameter (MCP) for the next page.
- **Locations** — internal spans are 0-based; JSON evidence sites carry `line_1based`, and human source headers such as `[4:1-5:50]` are 1-based `line:column`. The human column is **byte offset + 1**, not an editor's visual column on non-ASCII lines.

## Recovery

| Situation | Command |
| --- | --- |
| Something looks wrong | `brainprint doctor [PATH]` (read-only) |
| Branch switch / rebase / edits while the daemon was down | `brainprint sync [PATH]` |
| Index suspected corrupt or inconsistent | `brainprint rebuild [PATH]` |
| Stop managing a Workspace for now | `brainprint uninit [PATH]`, later `brainprint init [PATH]` |
| Daemon not running | start `brainprintd` |

None of these modify source files. There is no `uninstall` command; to remove Brainprint completely, stop the daemon and delete `~/.brainprint/` and each Workspace's `.brainprint/`.

## Storage and data locations

| Location | Contents |
| --- | --- |
| `~/.brainprint/config.toml` | Global config (`format_version`, `[semantic_backends.*]`, `[ui]`). |
| `~/.brainprint/data/global.db` | Project/Workspace registry, user policies and preferences, blueprints. |
| `~/.brainprint/logs/`, `~/.brainprint/cache/` | Logs (e.g. backend logs) and cache. |
| `$XDG_RUNTIME_DIR/brainprint/` or `~/.brainprint/runtime/` | Daemon lock, IPC socket (`brainprintd.sock`; a short path under the OS temp dir if the full path is too long for a Unix socket), ephemeral verification artifacts. |
| `<workspace>/.brainprint/workspace.toml` | Workspace identity. |
| `<workspace>/.brainprint/config.toml` | Workspace config: `extra_excluded_directory_names`, `[semantic_backends.*]` overrides, `project_execution_trust`. |
| `<workspace>/.brainprint/data/project.db` | Durable project knowledge (policies, decisions, project state). |
| `<workspace>/.brainprint/data/workspace.db` | Working State, WorkItems, verification jobs. |
| `<workspace>/.brainprint/data/index.db` | The rebuildable index (resources, symbols, occurrences, relations, semantic publications). |

On Windows the IPC endpoint is a named pipe and home is `%USERPROFILE%`.

**Source ownership.** The source tree remains the source of truth. `.brainprint/` stores structured understanding (identities, symbols with signatures and spans, relations, fingerprints, knowledge, state), not a copy of file bodies — a schema test (`no_schema_stores_raw_source_body`) enforces this. Source text shown in answers is read from the current file at query time. Add `.brainprint/` to your `.gitignore`.

Discovery always skips `.git`, `.brainprint`, `node_modules`, `venv`, `.venv`, `virtualenv`, and skips `target`, `bin`, `obj`, `build`, `dist` when project markers show they are build output. `.gitignore` is **not** parsed; add more directory names (names, not globs) with:

```toml
# <workspace>/.brainprint/config.toml
extra_excluded_directory_names = ["vendor", "third_party"]
```

### Measured footprint (one example, not a guarantee)

On this repository (579 files, macOS, release build, no semantic backend configured): daemon idle RSS ~6 MiB; ~16 MiB steady with one Workspace after the initial index; `init` + initial index ~7.7 s; `.brainprint/` ~60 MB (`index.db` ~30 MB + WAL ~30 MB). Your numbers will differ with repository size and configured backends.

## Local-first and privacy

- Daemon IPC is a local Unix socket (Windows: named pipe). The only TCP listener is the optional Web UI, bound to `127.0.0.1`.
- Brainprint contains no HTTP client and sends no telemetry anywhere. The only optional telemetry is the `brainprint-agent` JSONL file you enable with `BRAINPRINT_ADOPTION_TELEMETRY_PATH`, written locally.
- External processes are only the semantic backends you configure and the verification commands you explicitly submit; what those programs do is up to them.
- Nothing is uploaded; there is no account or cloud component.

## Benchmark and economy results

Brainprint measures itself against native-only exploration (the same agent with Read/Grep/Glob/shell and no Brainprint). Results so far with Claude Code on this repository:

| Benchmark | Result |
| --- | --- |
| [#32](https://github.com/nyangko/Brainprint/issues/32) I5 economy (`benchmarks/i5-task14/`) | Brainprint integration path cost **more**: native $0.579/session vs Brainprint $0.974/session (+68%), although native exploration calls fell 41% and native result bytes fell 44%. |
| [#35](https://github.com/nyangko/Brainprint/issues/35) MCP contract alternatives A–E (`benchmarks/issue-35-contracts/`) | Every tested contract cost more than native (+$0.30 to +$0.49 per session). |
| [#74](https://github.com/nyangko/Brainprint/issues/74) first-route exact substitution (`benchmarks/issue-74-first-route/`) | The substitution candidate cost more than native ($0.538 vs $0.500 per session). |
| [#75](https://github.com/nyangko/Brainprint/issues/75) adaptive routing on a real project (`benchmarks/issue-75-adaptive-routing/`) | In every measured class and condition: native < adaptive routing < forced Brainprint. No production router ships. |

So **Brainprint 0.1.0 does not deliver lower provider cost than native-only exploration** in these measurements. What it offers today is the shared, current, explicitly-qualified project truth and continuity listed under [guarantees](#what-010-guarantees--and-what-it-does-not). Raw data, harnesses and per-task tables are in `benchmarks/`.

## Known limitations

1. **Compact rows are terse, not prose.** Relation rows name endpoints by id, a relation gap is located as `<resource id>:<line>`, and evidence without a source span (target selection, coverage reports) prints in Rust `Debug` form. Coverage is always stated (`Incoming: 4 confirmed, coverage Partial (…)`, `boundary edges: None found -- coverage incomplete`), and every line number shown is the 1-based editor line ([#78](https://github.com/nyangko/Brainprint/issues/78)).
2. **Columns are bytes.** The human column is byte offset + 1, not a visual column on non-ASCII lines.
3. **`wide` budget can be too large for some MCP clients.** Its output may exceed a client's tool-output limit; use `compact`/`standard` and continuation.
4. **Text search can truncate on large ignored trees.** Whole-Workspace `find text` on repos with large git-ignored vendor directories can return `TRUNCATED` ([#63](https://github.com/nyangko/Brainprint/issues/63)), because `.gitignore` is not parsed. Workaround: `extra_excluded_directory_names`, or `--path-prefix`.
5. **Semantic backends are separate installs.** Without them, results are structural with explicit gaps; C#/Rust additionally need `project_execution_trust = "Trusted"`.
6. **No cost-saving guarantee.** See [Benchmarks](#benchmark-and-economy-results).
7. **Manual setup.** No packages, no auto-start of the daemon, no service unit, no client auto-configuration; `find files` listings are capped (default 200).

## Roadmap

| Version | Primary axis |
| --- | --- |
| 0.1.0 | Token & Context Economy — the baseline above |
| 0.2.0 | Deterministic Work Offload |
| 0.3.0 | Rules & Project Intelligence |
| 0.4.0 | Language & Ecosystem Expansion |
| 0.5.0 | Persona & Role Awareness (presentation only, never truth) |
| 1.0.0 | Stable release after a separate hardening gate |

- [#12 — P0 implementation roadmap](https://github.com/nyangko/Brainprint/issues/12)
- [#18 — 0.1.0–0.5.0 → 1.0.0 product roadmap](https://github.com/nyangko/Brainprint/issues/18)

## Documentation

This README is the 0.1.0 documentation baseline; the GitHub Wiki is not set up. Command details: `brainprint <command> --help`. Agent instructions: `integrations/brainprint/SKILL.md`. Benchmark reports: `benchmarks/`.

## License

GPL-3.0-only. See [LICENSE](./LICENSE).
