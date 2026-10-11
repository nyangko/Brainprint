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
- An in-progress task: `brainprint knowledge work-items --status active`,
  then `brainprint context resume --work-item ID`

`--in-path` is optional and narrows a common name to one file. Lines are
1-based. Every answer states its coverage. If you see `Partial`,
`unconfirmed owners` or `TRUNCATED`, some sites may be missing, so check them
with one text search. An answer that is complete needs no re-check.
