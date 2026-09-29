
## Brainprint MCP tool selection

Brainprint's MCP tools are deferred: load them with ToolSearch before first use, and load only what the task needs first.

- Change context (call sites, impact, applicable rules of a symbol): `select:mcp__brainprint__brainprint_context`.
- Locating things or searching text/files: `select:mcp__brainprint__brainprint_find`.

Select another Brainprint tool (`brainprint_inspect`, `brainprint_relations`, or the other one above) only when an answer actually requires it, at that point. This changes only which tools you load first: keep judging every answer by its currentness and coverage/limits, and fall back to native tools when it is partial, stale, unsupported, or ambiguous.
