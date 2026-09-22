# The pinned Svelte semantic backend

`npm install` here, and nowhere else.

```sh
npm install
cargo test -p brainprint-engine --test svelte_semantic_lsp -- --ignored --nocapture
```

What #19 task 11 measured, and why this directory looks the way it does.

## The toolchain

```text
svelte-language-server  0.18.4
svelte2tsx              0.7.61
svelte                  5.57.1
typescript              6.0.3
launch                  node <here>/node_modules/svelte-language-server/bin/server.js --stdio
```

## Why TypeScript 6, next to the TS/JS backend's TypeScript 7

Not an oversight, and not symmetry avoidance. `svelte-language-server`
0.18.4 declares `peerDependencies.typescript` as `^5.9.2 || ^6.0.2`, so
npm *refuses* to install it beside `typescript@7.0.2` at all. Forced
through with `--legacy-peer-deps`, the server dies on module load:

```text
TypeError: Cannot read properties of undefined (reading 'useCaseSensitiveFileNames')
    at new FileMap (.../lib/documents/fileCollection.js:13:70)
```

TypeScript 7 is the native port and no longer exposes the classic
`ts.sys` CommonJS shape the Svelte tooling reaches for. So Brainprint
runs two semantic backends with two different TypeScript versions:
`typescript-go` 7.0.2 answers `.ts`/`.tsx`/`.js`/`.jsx`, and this
directory's TypeScript 6.0.3 rides inside the Svelte language server and
answers `.svelte`. `SemanticBackendKind::Svelte` exists for exactly this.

## Trust

The server executes `svelte.config.js` on startup -- measured, with a
config that wrote a marker file. Brainprint therefore sends
`initializationOptions.isTrusted: false`, which is the server's own
public switch (`configLoader.setDisabled(!isTrusted)`), and project
config code does not run. Every P0 capability this tier claims was
measured to answer identically with the config present, absent, broken,
and blocked.

Nothing here is installed automatically. A machine without this
directory still gets every structural answer I2/I3 has -- that is Level
B, and it is a supported state, not a failure.
