# The pinned TypeScript semantic backend

`npm install` here, and nowhere else.

```sh
npm install
cargo test -p brainprint-engine --test typescript_semantic_lsp -- --ignored --nocapture
```

What this directory is, and why it is a directory rather than a
`PATH` lookup:

* **`typescript`** is the backend. #19 task 10 measured `7.0.2`, whose
  language server is a native binary at
  `node_modules/@typescript/typescript-<os>-<arch>/lib/tsc`, launched as
  `<exe> --lsp --stdio`. Brainprint launches that executable directly, so
  no Node runtime is involved at analysis time.
* **`@types/node`** is not the backend. It is the one external dependency
  the committed fixture imports, so that "an external package resolves to
  an `ExternalEntity` and its source is never indexed" is a claim about a
  real `node_modules` tree rather than about a mock.

Nothing here is installed automatically. A machine without this
directory still gets every structural answer I2/I3 has -- that is Level
B, and it is a supported state, not a failure.
