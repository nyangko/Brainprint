<p align="center">
  <img src="./assets/brainprint-hero.svg" alt="Brainprint" width="100%" />
</p>

<p align="center">
  <strong>A local-first context runtime that helps AI coding agents read less, rediscover less, and spend their reasoning on the work that actually needs judgment.</strong>
</p>

<p align="center">
  English · <a href="./docs/README.ko.md">한국어</a> · <a href="./docs/README.ja.md">日本語</a>
</p>

# Brainprint

AI coding agents are good at reasoning, but they repeatedly spend context and tool calls on work ordinary software can do more cheaply:

- rediscovering repository structure,
- rereading unchanged source,
- tracing the same callers and imports,
- reconstructing project state after a new session or compaction,
- sorting, deduplicating, grouping, and filtering raw tool output,
- repeating Git/test/build exploration that could have been prepared once.

Brainprint is being built as a **persistent, local-first context runtime** between the project and the agent.

Its job is not to replace the agent's judgment. Its job is to make sure the agent receives the smallest sufficient, current, structured view of the project so the model can spend its reasoning on implementation, trade-offs, and verification.

> **If Brainprint can know, derive, sort, deduplicate, prepare, or verify something deterministically, the agent should not have to spend reasoning tokens doing it again.**

## The core idea

Traditional agent workflows often look like this:

```text
Agent
  → ls / find
  → rg
  → read files
  → reconstruct imports and callers
  → sort / dedupe / filter raw results
  → inspect Git state
  → rediscover project rules
  → finally start reasoning about the change
```

Brainprint aims to move the support work out of the model:

```text
Project / Workspace
        ↓
    brainprintd
        ↓
  fresh structured truth
        ↓
  prepare / dedupe / bound
        ↓
      MCP / CLI
        ↓
      Agent
        ↓
  judgment / editing / verification
```

The project remains the source of truth. Brainprint is not a source backup, Git replacement, autonomous project manager, or general-purpose conversation archive.

## Product principles

### 1. Substitution, not addition

Brainprint should replace repeated exploration, not add another tool call before the agent performs the same `ls/find/rg/read/git` work anyway.

### 2. Prepare enough to work

Returning only a filename or line number is not enough when Brainprint already knows the exact current source range, direct consumers, relevant tests, and uncertainty around the result.

### 3. Deterministic work belongs in Brainprint

The runtime should handle cheap, repeatable operations such as:

```text
sort
dedupe
filter
group
count
set intersection
range merge
overlap removal
stable ordering
candidate bounding
pagination
continuation
diff
known-state comparison
budget enforcement
deterministic output compaction
```

Sending hundreds of raw items to the model and asking it to sort and reduce them is not token optimization; it is moving ordinary compute into the most expensive layer.

### 4. Shared truth, not shared giant context

Multiple agents can share the same current project/workspace truth without copying a huge parent transcript into every worker.

### 5. Correctness before savings

`STALE`, `PARTIAL`, `UNRESOLVED`, `UNSUPPORTED`, and truncated results must remain explicit. Brainprint must not hide uncertainty just to reduce tokens.

### 6. The agent keeps the skilled work

Brainprint prepares facts, evidence, state, rules, and verification targets.

The agent remains responsible for:

- understanding the problem,
- choosing a change strategy,
- writing code,
- evaluating trade-offs,
- interpreting uncertainty,
- making the final verification judgment.

## Architecture

Brainprint is designed around one global local daemon with workspace-scoped runtimes.

```text
Codex / Claude / Gemini / other agents
                │
          MCP / thin Skill
                │
                ▼
          global brainprintd
                │
        ┌───────┼────────┐
        ▼       ▼        ▼
   Workspace A  B        C
        │
        ├─ Resource / Symbol / Occurrence
        ├─ Relation / unresolved / candidates
        ├─ freshness / revision / generation
        ├─ current source preparation
        ├─ project / working state
        └─ command intelligence
```

The intended model is:

- **Project truth is shared.**
- **Task context is not.**
- Expensive parsing/semantic work should be reusable across agents on the same workspace.
- WorkItem / Role / Persona metadata may shape later projections, but they must never change source facts, relations, or freshness.

## Brainprint 0.1.0 → 0.5.0 → 1.0.0

Brainprint will evolve through five pre-1.0 product stages, then enter a separate stabilization/hardening gate before 1.0.0.

These versions describe the **primary improvement axis** of each release. They do not mean that a required baseline capability is absent until a later version.

| Version | Primary goal | Core question |
| --- | --- | --- |
| **0.1.0** | **Token & Context Economy** | How much repeated reading, searching, context, and support work can we remove while remaining correct? |
| **0.2.0** | **Deterministic Work Offload** | What ordinary computation can software perform so the LLM does not have to? |
| **0.3.0** | **Rules & Project Intelligence** | Can the agent receive only the project rules, decisions, and working state relevant to the current task? |
| **0.4.0** | **Language & Ecosystem Expansion** | How much more of the real software system can Brainprint understand accurately? |
| **0.5.0** | **Persona & Role Awareness** | Can presentation adapt to the worker without changing project truth? |
| **1.0.0** | **Stable Release / Hardening** | Is Brainprint correct, stable, optimized, maintainable, recoverable, and documented enough to call stable? |

The detailed release goals are tracked in [#18 — Brainprint 0.1.0–0.5.0 → 1.0.0 product roadmap](https://github.com/nyangko/Brainprint/issues/18).

### 0.1.0 — Token & Context Economy

The first practical release is not intended to be an indexing demo. It must be usable on real projects.

The 0.1.0 baseline includes:

- local daemon and workspace identity,
- structural code intelligence,
- Relation Graph and impact evidence,
- current-source preparation,
- semantic backend baseline,
- project rules / decisions / working-state baseline,
- request-shaped context projection,
- Git/test/lint/typecheck/build intelligence,
- high-level MCP + thin Skill,
- deterministic sort/dedupe/filter/bounding baseline,
- multi-agent shared truth,
- real-project benchmark and dogfooding.

### 0.2.0 — Deterministic Work Offload

Expand the 0.1.0 baseline so the model performs less mechanical work: result shaping, repeated diagnostics reduction, deterministic diff/state comparison, delivery reuse, verification deltas, and measured adaptive optimizations where they prove useful.

### 0.3.0 — Rules & Project Intelligence

Strengthen project policy, decisions, blueprints, working-state lineage, precedence, conflict handling, handoff/resume, and compact applicable-rule projection.

The goal is to stop making every new agent rediscover **how this project is supposed to be worked on**.

### 0.4.0 — Language & Ecosystem Expansion

0.1.0 already targets a practical baseline for Python, TypeScript/JavaScript, React, Svelte, C#, and Rust.

1.3 expands semantic depth and ecosystem understanding: additional languages, framework adapters, ORM/database relations, routes, events, queues, cache/config semantics, and cross-project relations where evidence is reliable.

### 0.5.0 — Persona & Role Awareness

Persona is intentionally last.

Role/Persona may change **what evidence is emphasized or how much detail is projected**, but it must never change project truth.

```text
Truth
  → task/context selection
  → optional Role/Persona adjustment
```

Not:

```text
Persona
  → truth interpretation
```

If Persona provides little measurable value, it stays small.

### 1.0.0 — Stable Release

1.0.0 is not another feature bucket after 0.5.0. It is the first release Brainprint will call stable only after separate hardening.

Before promotion to 1.0.0, the project must verify:

- correctness and major bug closure on real projects,
- regression coverage for core workflows,
- acceptable CPU/RAM/I/O/index-size and long-running background cost,
- measured token/context/tool-call improvements,
- multi-agent stability and recovery behavior,
- migration/upgrade paths,
- code quality and maintainability of critical modules,
- stable enough CLI/MCP/public contracts,
- README/Wiki/API behavior consistency,
- honest language/feature coverage and known limitations.

0.5.0 completion alone is **not** sufficient to tag 1.0.0.

## Current implementation status

Brainprint 0.1.0 is currently under active implementation.

The implementation roadmap is tracked in [#12 — P0 implementation roadmap](https://github.com/nyangko/Brainprint/issues/12).

Current workstreams:

| Workstream | Status |
| --- | --- |
| I0 — Benchmark Harness / Skeleton | Complete |
| I1 — Core Runtime Foundation | Complete |
| I2 — Structural Intelligence | Complete |
| I3 — Relation Graph | In progress |
| I4 — Semantic Backends | Planned |
| I5 — Project Intelligence + Projection + MCP/Skill | Planned |
| I6 — Command Intelligence | Planned |
| I7 — UX / Recovery / Acceptance | Planned |

The implementation is intentionally being built bottom-up: first trustworthy project facts and freshness, then relations and semantics, then projection and agent-facing interfaces.

## What 0.1.0 should feel like

For a request such as:

```text
"Change this method signature and update everything affected."
```

the target experience is not:

```text
Agent → search → read → search → inspect imports → search callers
      → find tests → reread source → reconstruct state → edit
```

It should be closer to:

```text
Brainprint
  → current target source
  → confirmed callers / references / type consumers
  → relevant current source ranges
  → related tests
  → applicable project rules
  → unresolved / unsupported gaps
  → current workspace state

Agent
  → decide
  → edit
  → verify
```

## How success is measured

Feature count is not the primary success metric.

Brainprint should measure whether it actually reduces:

- agent tool calls,
- repeated unchanged reads,
- raw source bytes delivered,
- context/token volume,
- duplicated analysis across agents,
- avoidable agent-side sort/dedupe/filter/group/count work,
- support-work ratio in long-running tasks.

At the same time it must track:

- correctness and missed dependencies,
- false-positive relations,
- stale/partial errors,
- latency,
- CPU/RAM/I/O,
- index size.

Token savings that come from hiding uncertainty are considered failure.

## Data model and storage philosophy

Brainprint stores structured understanding, not a second copy of the project.

Examples of retained knowledge:

- stable project/workspace/resource identity,
- symbols and occurrences,
- canonical relations,
- unresolved/candidate evidence,
- fingerprints and revision state,
- project decisions and working state.

Original source, images, audio, video, and other project assets remain external resources referenced by identity/location/fingerprint. Rebuildable indexes should remain disposable.

## Agent integration

The intended 0.1.0 integration model is:

```text
Agent plugin / extension
        │
   thin Skill + MCP
        │
        ▼
   brainprintd
```

Agent-specific packaging may differ, but the Core should remain shared. Codex, Claude, Gemini, and other clients should not each run a separate project intelligence engine for the same workspace.

Marketplace-specific packaging and richer one-click distribution can evolve after the 0.1.0 integration contract is proven.

## Documentation

Documentation will evolve during pre-1.0 development, but README/Wiki will receive a final audit against the **actual accepted implementation** before 1.0.0.

The Wiki is expected to cover:

- architecture and identity model,
- freshness/revision/generation,
- structural and semantic intelligence,
- Relation Graph,
- project/working state,
- context projection and task packets,
- MCP / Skill / agent integration,
- CLI,
- storage/data policy,
- multi-agent concurrency,
- recovery/troubleshooting,
- benchmarks and acceptance,
- language coverage,
- design decisions and known limitations,
- developer/contributor guidance.

## Design and implementation tracking

- [#12 — P0 implementation roadmap](https://github.com/nyangko/Brainprint/issues/12)
- [#18 — Brainprint 0.1.0–0.5.0 → 1.0.0 product roadmap](https://github.com/nyangko/Brainprint/issues/18)

The design issues remain the source of truth for architectural decisions while implementation issues track executable work.

## Current maturity

**Active pre-1.0 development. Current target: 0.1.0.**

Interfaces, storage details, and integration contracts may still change throughout 0.x development. Brainprint will not be promoted to 1.0.0 merely because the planned feature stages are complete: correctness, bugs, performance, resource usage, code quality, upgrade/recovery, public contracts, and documentation must pass a separate stabilization/hardening review.
