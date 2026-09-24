---
name: brainprint
description: Prefer Brainprint's indexed project truth (find/inspect/relations/context) over raw file discovery when it can answer current and complete. Canonical repository source; Task 13 owns client-specific installation.
---

# Brainprint

Brainprint keeps indexed, current truth about this project (structure,
relations, current source, Policy/Decision history, Working State) behind
four MCP tools, backed by a local `brainprintd` daemon. Prefer it for
project facts when it can answer current and complete.

## Tools

- **`brainprint.find`** — location: an exact/search target, a file
  listing, or explicit text search.
- **`brainprint.inspect`** — exact current source and direct relations of
  one resolved target, for understanding it.
- **`brainprint.relations`** — direct dependencies of one anchor, or the
  impact of a declared change.
- **`brainprint.context`** — an edit's context, resuming a WorkItem,
  applicable rules, WorkItem/Decision/Policy history, and structure.

## When to use it

Don't repeat `Read`/`Grep`/`Glob`/shell discovery for a fact Brainprint
already returned as current and complete — use that answer.

Fall back to native tools when Brainprint reports a result as partial,
stale, unsupported, or ambiguous, or when the user explicitly asks for
raw/native verification.

Editing remains your job: Brainprint answers questions about the project,
it does not make or apply changes. `brainprint.run` (command execution)
does not exist yet.
