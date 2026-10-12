## Brainprint (CLI)

Use native read/search for files and text. Use `brainprint` CLI only for:

- Confirmed callers/importers with path:line and owner:
  `brainprint relations --symbol-name NAME --in-path FILE --direction incoming`
- Source, callers, and tests before signature changes:
  `brainprint context change --symbol-name NAME --in-path FILE --change public-signature`
- Project rules: `brainprint knowledge rules`

For resuming, read `WORKING_STATE.md` with native tools if present.
`brainprint context resume` exists, but is not the economic default.

`--in-path` disambiguates; lines are 1-based. Coverage rules:
- `Partial` may omit sites; confirmed rows remain valid. For **explicitly
  confirmed-only** questions, cite those rows with the caveat, without
  automatic native re-search.
- For **all** callers, absence claims or potentially breaking changes,
  verify with one scoped native search and relevant tests. Partial zero
  is never proof of no callers; text search cannot prove dynamic dispatch.
- `TRUNCATED`/`MORE_AVAILABLE`: continue only if omitted rows matter.
  `unconfirmed owners` are not confirmed edges.
