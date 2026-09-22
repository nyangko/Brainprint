#!/usr/bin/env python3
"""Measure the rust-analyzer LSP boundary for #19 task 13.

Nothing in Brainprint runs this. It exists so the production adapter is
written against what the server does rather than what its documentation
says: the handshake it answers, whether it needs document
synchronization, which requests it implements, what it says about
traits and impls, and -- the one that decides the whole lifecycle --
whether there is an observable barrier after a project reload.

    ./install.sh
    python3 probe.py [<workspace>]
"""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parent
FIXTURE = ROOT.parent.parent / "fixtures" / "workspaces" / "rust-semantic-spike"


class Server:
    """A single rust-analyzer over stdio."""

    def __init__(self, executable: str, root: pathlib.Path, env: dict | None = None):
        self.proc = subprocess.Popen(
            [executable],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=str(root),
            env={**os.environ, **(env or {})},
        )
        self.root = root
        self.next_id = 1
        self.responses: dict[int, dict] = {}
        self.notifications: list[dict] = []
        self.server_requests: list[dict] = []
        self.lock = threading.Lock()
        self.alive = True
        threading.Thread(target=self._read, daemon=True).start()
        threading.Thread(target=self._drain_stderr, daemon=True).start()

    def _drain_stderr(self):
        for line in self.proc.stderr:
            text = line.decode("utf-8", "replace").rstrip()
            if "ERROR" in text or "panic" in text or "WARN" in text:
                print(f"  stderr: {text}", flush=True)

    def _read(self):
        stream = self.proc.stdout
        while True:
            header = b""
            while not header.endswith(b"\r\n\r\n"):
                chunk = stream.read(1)
                if not chunk:
                    self.alive = False
                    return
                header += chunk
            length = 0
            for line in header.decode().split("\r\n"):
                if line.lower().startswith("content-length:"):
                    length = int(line.split(":", 1)[1])
            body = b""
            while len(body) < length:
                chunk = stream.read(length - len(body))
                if not chunk:
                    self.alive = False
                    return
                body += chunk
            message = json.loads(body)
            with self.lock:
                if "id" in message and "method" in message:
                    self.server_requests.append(message)
                    # Answer anything the server asks, so it does not
                    # block waiting for us.
                    self._send({"jsonrpc": "2.0", "id": message["id"], "result": None})
                elif "id" in message:
                    self.responses[message["id"]] = message
                else:
                    self.notifications.append(message)

    def _send(self, message: dict):
        body = json.dumps(message).encode()
        self.proc.stdin.write(
            f"Content-Length: {len(body)}\r\n\r\n".encode() + body
        )
        self.proc.stdin.flush()

    def notify(self, method: str, params=None):
        with self.lock:
            self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def request(self, method: str, params=None, timeout: float = 180.0):
        with self.lock:
            request_id = self.next_id
            self.next_id += 1
            self._send(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": method,
                    "params": params,
                }
            )
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self.lock:
                if request_id in self.responses:
                    return self.responses.pop(request_id)
            if not self.alive:
                return {"error": {"message": "connection closed"}}
            time.sleep(0.01)
        return {"error": {"message": f"timeout after {timeout}s"}}

    def seen(self, method: str) -> list[dict]:
        with self.lock:
            return [n for n in self.notifications if n.get("method") == method]

    def wait_for(self, predicate, timeout: float = 240.0):
        """Block until `predicate(notifications)` is true. No sleeps as
        a freshness mechanism -- this is how the barrier is *found*."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self.lock:
                snapshot = list(self.notifications)
            if predicate(snapshot):
                return True
            if not self.alive:
                return False
            time.sleep(0.02)
        return False


def uri(path: pathlib.Path) -> str:
    return path.resolve().as_uri()


def position_of(text: str, needle: str, name: str, occurrence: int = 0) -> dict:
    """The position of the last character of `name` inside the
    `occurrence`-th `needle`, which is how the adapter asks."""
    start = -1
    for _ in range(occurrence + 1):
        start = text.index(needle, start + 1)
    at = start + needle.index(name) + len(name) - 1
    line = text.count("\n", 0, at)
    column = at - (text.rfind("\n", 0, at) + 1)
    return {"line": line, "character": column}


def initialization_options() -> dict:
    """The P0 safe configuration: load the project, run nothing.

    `cargo.noDeps` is deliberately NOT set. It reads like an execution
    control and is not one -- it decides whether dependencies enter the
    crate graph, and with it on, every cross-crate answer in this
    fixture came back empty. Set `PROBE_NO_DEPS=1` to see that again.
    The execution controls are the other three.
    """
    return {
        "cargo": {
            "buildScripts": {"enable": False},
            "noDeps": bool(os.environ.get("PROBE_NO_DEPS")),
            "features": [],
        },
        "procMacro": {"enable": False, "attributes": {"enable": False}},
        "checkOnSave": False,
        "check": {"enable": False},
        "files": {"watcher": "client"},
    }


def short(value, limit: int = 240) -> str:
    text = json.dumps(value, ensure_ascii=False)
    return text if len(text) <= limit else text[:limit] + "…"


def relative(value, root: pathlib.Path) -> str:
    return short(value).replace(root.resolve().as_uri() + "/", "WS/")


def main() -> int:
    executable = subprocess.run(
        ["rustup", "which", "rust-analyzer"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    workspace = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else FIXTURE

    print("=" * 68)
    print("toolchain")
    print("=" * 68)
    print(f"  rust-analyzer   {executable}")
    for command in (
        [executable, "--version"],
        ["rustc", "--version"],
        ["cargo", "--version"],
    ):
        out = subprocess.run(command, capture_output=True, text=True)
        print(f"  {out.stdout.strip() or out.stderr.strip()}")
    sysroot = subprocess.run(
        ["rustc", "--print", "sysroot"], capture_output=True, text=True
    ).stdout.strip()
    src = pathlib.Path(sysroot) / "lib/rustlib/src/rust/library"
    print(f"  sysroot         {sysroot}")
    print(f"  rust-src        {'present' if src.is_dir() else 'ABSENT'}")
    host = subprocess.run(
        ["rustc", "--version", "--verbose"], capture_output=True, text=True
    ).stdout
    for line in host.splitlines():
        if line.startswith("host:"):
            print(f"  {line}")

    marker = pathlib.Path("/tmp/brainprint-build-rs-marker")
    marker.unlink(missing_ok=True)
    server = Server(executable, workspace, {"BRAINPRINT_BUILD_RS_MARKER": str(marker)})

    print()
    print("=" * 68)
    print("handshake")
    print("=" * 68)
    started = time.time()
    answer = server.request(
        "initialize",
        {
            "processId": os.getpid(),
            "rootUri": uri(workspace),
            "workspaceFolders": [
                {"uri": uri(workspace), "name": workspace.name}
            ],
            "initializationOptions": initialization_options(),
            "capabilities": {
                "general": {"positionEncodings": ["utf-16"]},
                "window": {"workDoneProgress": True, "showMessage": {}},
                "experimental": {"serverStatusNotification": True},
                "workspace": {
                    "workspaceFolders": True,
                    "configuration": True,
                    "didChangeWatchedFiles": {"dynamicRegistration": True},
                },
                "textDocument": {
                    "synchronization": {"didSave": False, "dynamicRegistration": False},
                    "definition": {"linkSupport": False},
                    "references": {},
                    "implementation": {"linkSupport": False},
                    "typeDefinition": {"linkSupport": False},
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": False},
                    "callHierarchy": {},
                    "hover": {"contentFormat": ["plaintext"]},
                },
            },
        },
    )
    result = answer.get("result", {})
    print(f"  serverInfo        {short(result.get('serverInfo'))}")
    caps = result.get("capabilities", {})
    print(f"  positionEncoding  {caps.get('positionEncoding')!r}")
    print(f"  textDocumentSync  {short(caps.get('textDocumentSync'))}")
    providers = sorted(
        key for key, value in caps.items() if key.endswith("Provider") and value
    )
    print(f"  providers ({len(providers)})    {', '.join(providers)}")
    for wanted in (
        "definitionProvider",
        "referencesProvider",
        "implementationProvider",
        "callHierarchyProvider",
        "documentSymbolProvider",
        "hoverProvider",
        "typeDefinitionProvider",
    ):
        print(f"    {wanted:<26} {caps.get(wanted) is not None and caps.get(wanted) is not False}")
    print(f"  experimental      {short(caps.get('experimental'))}")
    server.notify("initialized", {})

    print()
    print("=" * 68)
    print("project load barrier")
    print("=" * 68)
    def primed(notes):
        return any(
            n.get("method") == "$/progress"
            and isinstance(n.get("params", {}).get("value"), dict)
            and n["params"]["value"].get("kind") == "end"
            and str(n["params"].get("token", "")) == "rustAnalyzer/cachePriming"
            for n in notes
        )

    ok = server.wait_for(primed, timeout=240)
    elapsed = time.time() - started
    tokens = {}
    for note in server.seen("$/progress"):
        token = str(note["params"].get("token"))
        kind = note["params"].get("value", {}).get("kind")
        tokens.setdefault(token, []).append(kind)
    print(f"  indexing end seen  {ok}  after {elapsed:.1f}s")
    for token, kinds in tokens.items():
        print(f"    token {token!r}: {' -> '.join(kinds)}")
    status = server.seen("experimental/serverStatus")
    print(f"  serverStatus notes {len(status)}")
    for note in server.seen("window/showMessage") + server.seen("window/logMessage"):
        body = note.get("params", {}).get("message", "")
        if body:
            print(f"    message: {body[:300]}")
    for note in status[-3:]:
        print(f"    {short(note.get('params'))}")
    print(f"  server->client requests: "
          f"{sorted({r.get('method') for r in server.server_requests})}")

    print()
    print("=" * 68)
    print("trust: was anything executed?")
    print("=" * 68)
    print(f"  build.rs marker    {'EXECUTED' if marker.exists() else 'absent'}")

    lib = workspace / "crates/core/src/runner.rs"
    main_rs = workspace / "crates/app/src/main.rs"
    lib_text = lib.read_text()
    main_text = main_rs.read_text()

    print()
    print("=" * 68)
    print("does it need document synchronization?")
    print("=" * 68)
    ask = lambda method, path, text, needle, name, occurrence=0: server.request(
        method,
        {
            "textDocument": {"uri": uri(path)},
            "position": position_of(text, needle, name, occurrence),
        },
        timeout=90,
    )
    cold = ask("textDocument/definition", main_rs, main_text, "Worker::new(4)", "new")
    print(f"  definition before didOpen  {relative(cold.get('result'), workspace)}")
    for path, text in ((lib, lib_text), (main_rs, main_text)):
        server.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": uri(path),
                    "languageId": "rust",
                    "version": 1,
                    "text": text,
                }
            },
        )
    warm = ask("textDocument/definition", main_rs, main_text, "Worker::new(4)", "new")
    print(f"  definition after didOpen   {relative(warm.get('result'), workspace)}")

    print()
    print("=" * 68)
    print("what it answers")
    print("=" * 68)
    cases = [
        ("associated fn  Worker::new", main_rs, main_text, "Worker::new(4)", "new", 0),
        ("inherent       worker.execute", main_rs, main_text, "worker.execute()", "execute", 0),
        ("trait method   Runner::run", main_rs, main_text, "Runner::run(&worker)", "run", 0),
        ("trait method   Reporter::run", main_rs, main_text, "Reporter::run(&worker)", "run", 0),
        ("UFCS           <Worker as Runner>::run", main_rs, main_text, "as Runner>::run", "run", 0),
        ("same-name trap Idle.run", main_rs, main_text, "Idle.run()", "run", 0),
        ("dyn call       value.run", lib, lib_text, "value.run()", "run", 2),
        ("re-export      PublicModel", main_rs, main_text, "PublicModel::new(3)", "PublicModel", 0),
        ("alias re-export PublicWorker", main_rs, main_text, "PublicWorker::new(6)", "PublicWorker", 0),
        ("generic fn     identity", main_rs, main_text, "identity(boxed.value)", "identity", 0),
        ("macro          doubled!", main_rs, main_text, "bp_core::doubled!(21)", "doubled", 0),
        ("use path       bp_core::runner", main_rs, main_text, "use bp_core::runner::", "runner", 0),
    ]
    for label, path, text, needle, name, occurrence in cases:
        try:
            answer = ask("textDocument/definition", path, text, needle, name, occurrence)
        except ValueError:
            print(f"  {label:<34} (not in fixture)")
            continue
        payload = answer.get("result") if "result" in answer else answer.get("error")
        print(f"  {label:<34} {relative(payload, workspace)}")

    print()
    print("  -- implementation ----------------------------------------")
    contracts = workspace / "crates/contracts/src/lib.rs"
    contracts_text = contracts.read_text()
    server.notify(
        "textDocument/didOpen",
        {
            "textDocument": {
                "uri": uri(contracts),
                "languageId": "rust",
                "version": 1,
                "text": contracts_text,
            }
        },
    )
    for label, needle, name in (
        ("trait Runner", "pub trait Runner", "Runner"),
        ("trait member run", "fn run(&self) -> u32;", "run"),
    ):
        answer = ask("textDocument/implementation", contracts, contracts_text, needle, name)
        payload = answer.get("result") if "result" in answer else answer.get("error")
        print(f"  {label:<34} {relative(payload, workspace)}")

    print()
    print("  -- references --------------------------------------------")
    answer = server.request(
        "textDocument/references",
        {
            "textDocument": {"uri": uri(contracts)},
            "position": position_of(contracts_text, "pub trait Runner", "Runner"),
            "context": {"includeDeclaration": False},
        },
        timeout=90,
    )
    found = answer.get("result") or []
    print(f"  references to trait Runner: {len(found) if isinstance(found, list) else found}")
    for location in (found or [])[:6]:
        print(f"    {relative(location, workspace)}")

    print()
    print("  -- documentSymbol / hover --------------------------------")
    answer = server.request(
        "textDocument/documentSymbol", {"textDocument": {"uri": uri(lib)}}, timeout=90
    )
    symbols = answer.get("result") or []
    print(f"  documentSymbol(runner.rs): {len(symbols) if isinstance(symbols, list) else symbols}")
    answer = ask("textDocument/hover", main_rs, main_text, "consume_dyn(&Worker::new(2))", "consume_dyn")
    hover = answer.get("result")
    print(f"  hover(consume_dyn): {short(hover, 180)}")

    print()
    print("=" * 68)
    print("project reload barrier")
    print("=" * 68)
    before = len(server.seen("$/progress"))
    quiet_before = len(server.seen("experimental/serverStatus"))
    cargo = workspace / "crates/core/Cargo.toml"
    original = cargo.read_text()
    try:
        cargo.write_text(original.replace('extra = []', 'extra = []\nspare = []'))
        server.notify(
            "workspace/didChangeWatchedFiles",
            {"changes": [{"uri": uri(cargo), "type": 2}]},
        )
        reload_answer = server.request("rust-analyzer/reloadWorkspace", None, timeout=240)
        print(f"  reloadWorkspace -> {short(reload_answer.get('result', reload_answer.get('error')))}")
        def quiescent_again(notes):
            statuses = [
                n["params"]
                for n in notes
                if n.get("method") == "experimental/serverStatus"
            ]
            # false (working) and then true (settled) after the reload.
            tail = statuses[quiet_before:]
            return any(not s.get("quiescent") for s in tail) and tail and tail[-1].get(
                "quiescent"
            )

        settled = server.wait_for(quiescent_again, timeout=240)
        print(f"  serverStatus quiescent again: {settled}")
        moved = server.wait_for(
            lambda notes: sum(1 for n in notes if n.get("method") == "$/progress") > before + 2,
            timeout=240,
        )
        print(f"  progress after reload: {moved}")
        kinds = [
            (str(n["params"].get("token")), n["params"].get("value", {}).get("kind"))
            for n in server.seen("$/progress")[before:]
        ]
        print(f"  tokens: {sorted(set(kinds))[:8]}")
        status = server.seen("experimental/serverStatus")
        print(f"  serverStatus after reload: {short(status[-1].get('params')) if status else 'none'}")
    finally:
        cargo.write_text(original)

    print()
    print("=" * 68)
    print("does an edit reach semantics, and how")
    print("=" * 68)
    model = workspace / "crates/core/src/model.rs"
    model_text = model.read_text()
    server.notify(
        "textDocument/didOpen",
        {
            "textDocument": {
                "uri": uri(model),
                "languageId": "rust",
                "version": 1,
                "text": model_text,
            }
        },
    )
    before_edit = ask("textDocument/definition", main_rs, main_text, "identity(boxed.value)", "identity")
    print(f"  identity before edit  {relative(before_edit.get('result'), workspace)}")
    shifted = "// shifted\n// shifted\n" + model_text
    server.notify(
        "textDocument/didChange",
        {
            "textDocument": {"uri": uri(model), "version": 2},
            "contentChanges": [
                {
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 0},
                    },
                    "text": "// shifted\n// shifted\n",
                }
            ],
        },
    )
    after_edit = ask("textDocument/definition", main_rs, main_text, "identity(boxed.value)", "identity")
    print(f"  identity after didChange {relative(after_edit.get('result'), workspace)}")
    print(f"  (declaration should move down two lines if the edit landed)")

    print()
    print("=" * 68)
    print("cancellation and shutdown")
    print("=" * 68)
    print(f"  $/cancelRequest is a notification the client sends; no probe needed")
    print(f"  shutdown -> {short(server.request('shutdown', None, timeout=60).get('result', 'ok'))}")
    server.notify("exit", None)
    time.sleep(0.4)
    print(f"  process exited: {server.proc.poll() is not None}")
    print(f"  build.rs marker (final): {'EXECUTED' if marker.exists() else 'absent'}")
    target = workspace / "target"
    print(f"  target/ created: {target.exists()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
