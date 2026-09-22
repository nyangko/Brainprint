# Python semantic backend transport probe (#19 task 5)

A decision spike, not Brainprint runtime code. It answers one question with
executable evidence: which Pyright artifact and protocol should the Python
semantic backend of tasks 6-9 launch?

## Run it

```sh
cd scripts/python_semantic_spike
npm install          # pinned pyright + pyright-typeserver, project-local
node probe.js        # both servers
node probe.js tsp    # type server only
```

`npm install` writes only into this directory (`node_modules` is gitignored).
No global install is needed and Brainprint's Cargo build never sees it.

The probe drives `fixtures/workspaces/python-semantic-spike` and prints a
redacted report -- machine-local paths are replaced with `<FIXTURE>`,
`<MODULES>`, `<SITE-PACKAGES>` and `<PATH>` before anything reaches stdout.

## What it checks

* stdio JSON-RPC handshake, clean shutdown, behaviour after a malformed frame
* TSP protocol version negotiation and the full TSP request surface
* whether one process serves both TSP type queries and LSP navigation
* snapshot currentness, and that a stale snapshot is rejected rather than answered
* import resolution: relative, absolute, stdlib, installed package, missing
* declared / computed / expected type
* definition, declaration, typeDefinition, references, call hierarchy
* implementation and type hierarchy (both unsupported -- proven, not assumed)
* filesystem truth vs `didOpen` overlay, and `didClose` returning to disk
* source coordinates: UTF-16 vs codepoint vs byte offsets on non-ASCII source
* `$/cancelRequest` behaviour and reusability afterwards
* cold start, warm query latency, RSS, process count

## Outcome

`pyright-typeserver` is a strict superset of `pyright-langserver`: it answers
the same LSP navigation requests *and* the `typeServer/*` type, import and
snapshot requests on one process and one connection. One process, one
protocol family. The decision record lives in issue #19.

The single sharpest result is the coordinate probe. Pyright speaks UTF-16
code units; Brainprint's `SourceSpan` is byte-based. Feeding a byte offset to
the backend on `pkg/wide.py` does not fail -- it lands on a different real
symbol and answers confidently about it. The adapter conversion has to be
exact.
