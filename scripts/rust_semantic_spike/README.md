# The rust-analyzer boundary — and what #19 task 13 measured at it

```sh
./install.sh          # rustup component add rust-analyzer
python3 probe.py      # measure; nothing in Brainprint runs this
```

Nothing here is installed automatically and no production code reads it.
Brainprint is handed an explicit executable path; it never searches
`PATH`, never runs `rustup`, and never installs a toolchain, a component
or `rust-src`.

## The toolchain, as measured

```text
rust-analyzer   ~/.rustup/toolchains/stable-aarch64-apple-darwin/bin/rust-analyzer
                rust-analyzer 1.98.1 (48a229ce 2026-09-01)
rustc           1.98.1 (48a229cea 2026-09-01)
cargo           1.98.1 (797e8a9bc 2026-08-05)
host            aarch64-apple-darwin
sysroot         ~/.rustup/toolchains/stable-aarch64-apple-darwin
rust-src        ABSENT
```

rust-analyzer ships inside the toolchain and reports the *toolchain's*
version rather than its own upstream release number, so backend identity
and compiler identity move together here. That is convenient and not
guaranteed: a project-local or distro rust-analyzer would version
independently, which is why the fingerprint records both.

## The handshake

```text
serverInfo         {"name": "rust-analyzer", "version": "1.98.1 (48a229ce 2026-09-01)"}
positionEncoding   utf-16
textDocumentSync   {"openClose": true, "change": 2, "save": {}}
providers          22, including definition, references, implementation,
                   callHierarchy, documentSymbol, hover, typeDefinition
```

`change: 2` is Incremental, exactly as the C# server declares, so a
replacement carries the range it replaces.

## The barrier — `experimental/serverStatus`

This is the measurement the whole lifecycle depends on, and it has a
precondition: the notification is only sent when the client declares

```json
{"capabilities": {"experimental": {"serverStatusNotification": true}}}
```

With it declared, the server sends

```text
{"health": "ok", "quiescent": false, "message": null}   ← working
{"health": "ok", "quiescent": true,  "message": null}   ← settled
```

and does so again around a reload. So there *is* a deterministic public
freshness barrier: after `rust-analyzer/reloadWorkspace`, wait for
`quiescent` to go false and back to true. No sleep, no settle delay, no
time-based polling.

`$/progress` is also emitted — `rustAnalyzer/Fetching`, `Building
CrateGraph`, `Roots Scanned`, `cachePriming` — and `cachePriming` ending
is a usable secondary signal for the initial load (2.4s for this
fixture). It is weaker than `serverStatus` because the token set is not
a stable public contract.

## `cargo.noDeps` is not an execution control

The one finding that changes the recommended P0 configuration.

With `cargo.noDeps: true`, **every cross-crate answer in the fixture was
empty** — associated functions, inherent methods, trait methods, the
`dyn` call, both re-exports, the generic function, the macro. What still
worked was single-crate: `documentSymbol`, and `references` on a trait
that found only the supertrait bound in its own file.

```text
noDeps: true    12 of 12 definition probes -> []
noDeps: false   12 of 12 definition probes -> exact declarations
```

`noDeps` decides whether dependencies enter the crate graph. It reads
like a safety switch and is not one: a workspace member's dependency on
another member is a dependency, so turning it on severs the workspace
from itself. The execution controls are the other three, and they were
verified independently:

```text
cargo.buildScripts.enable = false     build.rs never ran
procMacro.enable          = false
check.enable              = false     no flycheck
checkOnSave               = false
```

With dependencies enabled and those three off, the marker `build.rs` in
`crates/core` did **not** execute and `target/` was **not** created.
Nothing was compiled. The no-network intent belongs to Cargo's own
offline mode, not to `noDeps`.

## What it answers

Measured on `fixtures/workspaces/rust-semantic-spike`, every one exact.

| site | answer |
|---|---|
| `Worker::new(4)` | `core/src/runner.rs` — the associated function |
| `worker.execute()` | `core/src/runner.rs` — the inherent method |
| `Runner::run(&worker)` | `core/src/runner.rs:24` — the **impl** member |
| `Reporter::run(&worker)` | `core/src/runner.rs:32` — the *other* impl member |
| `<Worker as Runner>::run` | `core/src/runner.rs:24` — same as `Runner::run` |
| `Idle.run()` | `core/src/runner.rs:48` — the inherent trap, kept apart |
| `value.run()` on `&dyn Runner` | `contracts/src/lib.rs:4` — the **trait declaration** |
| `PublicModel::new(3)` | `contracts/src/lib.rs` — through the re-export to `Model` |
| `PublicWorker::new(6)` | `core/src/runner.rs` — through the aliased re-export |
| `identity(..)` | `core/src/model.rs` |
| `bp_core::doubled!(21)` | `core/src/lib.rs` — the `macro_rules!` declaration |
| `use bp_core::runner::` | `core/src/runner.rs`, as a **whole-file range** |

Two of those decide design.

**Dispatch is readable from the answer.** A call on a concrete type
resolves to the *impl* member; the same call through `&dyn Runner`
resolves to the *trait* member. So the honest dispatch classification
does not need a Rust-specific vocabulary or a hover parse — it is the
difference between landing inside an `impl ... for ...` and landing
inside a `trait` body.

**A module answers as a Resource.** `use bp_core::runner` returns the
whole of `runner.rs` as a degenerate range covering the file, which is
the module rather than any declaration in it.

`textDocument/implementation` answers for both a trait and a trait
member, returning the set of implementations in both cases.

## Editing

An incremental `didChange` carrying the replaced range moved the answer
from line 17 to line 19 after two lines were inserted above it. So
document synchronization works and is the same contract the C# backend
already follows: Brainprint sends its own current Resource bytes, never
an editor buffer, and a change says what it replaces.

## What is not measured here

`rust-src` is absent on this machine, so standard-library navigation is
untested and is reported as such rather than assumed. Proc macros are
disabled by the P0 configuration, so anything they would generate is
absent by construction.
