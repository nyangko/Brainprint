## Brainprint (CLI)

Read files and search text with your usual tools. Run the `brainprint` CLI
through the shell only when you need these:

- Callers or importers of a symbol, each with `path:line` and its enclosing
  function:
  `brainprint relations --symbol-name NAME --in-path FILE --direction incoming`
- Before changing a signature, its source, its direct and further callers
  (`-> callee` marks a further hop), and the tests found for them:
  `brainprint context change --symbol-name NAME --in-path FILE --change public-signature`
- Project rules and policies: `brainprint knowledge rules`

To resume an in-progress task, read or search `WORKING_STATE.md` with your
usual tools. `brainprint context resume` still exists but costs more than
that read, so it is not the default.

`--in-path` is optional and narrows a common name to one file. Lines are
1-based. Every answer states its coverage. Confirmed rows are positive
evidence; `Partial` means the list might omit other sites, not that the
reported rows are unconfirmed.

- If the task **explicitly requests confirmed sites only**, return the
  confirmed rows with their `Partial` caveat. Do not repeat text search
  solely because the answer is partial.
- If the task needs **all callers**, a negative/absence claim, or a
  change that could break an unlisted caller (especially public APIs),
  verify with one scoped native text search and relevant tests. Never
  treat a partial zero as proof that there are no callers. Native search
  does not prove dynamic dispatch complete either.
- `TRUNCATED`/`MORE_AVAILABLE` means delivery may omit items. Request
  continuation only when those items matter; do not call the list
  complete without them. `unconfirmed owners` are not confirmed edges.

For simple known-file lookups, use native read/search first. A complete,
current Brainprint answer needs no routine re-check.
