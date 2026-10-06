"""#35 integration-contract candidates as a stdio MCP proxy in front of the real `brainprint-mcp`.

usage: proxy.py <A|B|C|D|E> <real brainprint-mcp binary>

Measurement fixture only -- not a product surface. Every call is translated into a call of the shipped
four-tool server, so typed outcomes, currentness, coverage, gaps, continuation checks and source verification
are the product's own. The proxy keeps no cursor/session map: B's continuation token and E's handle are
stateless encodings of what the caller would otherwise echo.

  A  the shipped four tools, forwarded unchanged (proxy overhead control).
  B  same four tools; workspace/correlation/max_* removed from the schema (workspace = startup cwd, the
     product's own fallback; correlation injected by the integration), continuation as one opaque string.
  C  one typed tool per operation (14), each with exactly its operation's fields and the shared vocabulary.
  D  `brainprint.call {operation, arguments}` + `brainprint.contract {operation}` (lazy per-operation contract,
     the C schema); arguments validated against that contract before execution.
  E  the shipped four tools; projected answers come back compact (no source text, no evidence spans, no
     hashes) with a stateless handle; `brainprint.expand {handle}` re-runs the request and returns the full
     answer, saying whether it is unchanged since the summary.

Every call is logged as one JSON line to $PROXY_LOG when set (candidate, tool, argument/result bytes,
invalid, lookup, expand, latency).
"""

import base64
import copy
import hashlib
import json
import os
import subprocess
import sys
import time
import uuid

import jsonschema

CANDIDATE = sys.argv[1]
REAL = sys.argv[2]
LOG = os.environ.get("PROXY_LOG")
SESSION = uuid.uuid4().hex

TARGET = ["partial_symbol_name", "qualified_symbol_name", "resource_basename", "resource_id", "resource_path",
          "resource_prefix", "symbol_id", "symbol_in_resource", "symbol_kind", "symbol_language", "symbol_name",
          "target_json"]
DELIVERY = ["budget_profile", "max_bytes", "max_items", "continuation"]
WORKSPACE = ["workspace_path", "workspace_id"]
CORRELATION = ["client_id", "session_id", "external_task_id", "external_subtask_id"]

# operation -> (real tool, mode, own fields, required, description)
OPS = {
    "find_target": ("brainprint.find", "target", TARGET + DELIVERY, [],
                    "Locate a Symbol/Resource by exact or search target."),
    "find_files": ("brainprint.find", "files",
                   ["directory", "recursive", "path_prefix", "role", "language", "kind", "limit"], [],
                   "List files from the index."),
    "find_text": ("brainprint.find", "text",
                  ["pattern", "regex", "case_insensitive", "path_prefix", "search_budget_profile", "with_preview"],
                  ["pattern"], "Explicit text search (never an automatic fallback from a structured miss)."),
    "inspect": ("brainprint.inspect", None, TARGET + DELIVERY, [],
                "The resolved target's exact current declaration source (or a typed SourceUnavailable reason) "
                "plus both directions of its direct relations."),
    "relations_direct": ("brainprint.relations", "direct", TARGET + ["direction", "kinds"], ["direction"],
                         "One anchor's confirmed relations, one hop, unpaged, no source."),
    "relations_impact": ("brainprint.relations", "impact", TARGET + ["change"] + DELIVERY, ["change"],
                         "The I3 traversal for a declared ChangeKind (stated explicitly, never inferred)."),
    "context_change": ("brainprint.context", "change", TARGET + ["change", "work_item"] + DELIVERY, [],
                       "Context for an edit at a target: source, impact/direct relations, rules."),
    "context_resume": ("brainprint.context", "resume", ["work_item"] + TARGET + DELIVERY, ["work_item"],
                       "Resume an explicit WorkItem: Working State, handoff, rules, optional target."),
    "context_rules": ("brainprint.context", "rules", [], [], "Currently applicable Policy."),
    "context_work_items": ("brainprint.context", "work_items", ["statuses", "limit"], ["statuses"],
                           "WorkItems by status."),
    "context_lineage": ("brainprint.context", "lineage", ["lineage_of", "id"], ["lineage_of", "id"],
                        "One Policy/Decision's one-hop history."),
    "context_handoffs": ("brainprint.context", "handoffs", ["work_item", "limit"], ["work_item"],
                         "A WorkItem's handoff history."),
    "context_structure": ("brainprint.context", "structure",
                          ["grouping", "resource_scope", "relation_kinds", "include_ungrouped", "include_cycles",
                           "member_sample_limit"], [], "Grouped structural summary."),
    "context_status": ("brainprint.context", "status", [], [], "The daemon's own status."),
}
PROJECTED = ("Inspect", "Context", "Impact")


def log(**row):
    if LOG:
        with open(LOG, "a") as f:
            f.write(json.dumps({"t": time.time(), "candidate": CANDIDATE, **row}) + "\n")


def jbytes(value):
    return len(json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode())


# ------------------------------------------------------------------ real server


class Real:
    def __init__(self):
        self.p = subprocess.Popen([REAL], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
        self.i = 0
        init = self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                       "clientInfo": {"name": "issue35-proxy", "version": "0"}})
        self.init = init["result"]
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        self.tools = {t["name"]: t for t in self.rpc("tools/list", {})["result"]["tools"]}

    def rpc(self, method, params):
        self.i += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.i, "method": method, "params": params}) + "\n")
        self.p.stdin.flush()
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise SystemExit("real brainprint-mcp closed")
            msg = json.loads(line)
            if msg.get("id") == self.i:
                return msg

    def call(self, name, arguments):
        msg = self.rpc("tools/call", {"name": name, "arguments": arguments})
        if "error" in msg:
            return {"content": [{"type": "text", "text": json.dumps(msg["error"])}], "isError": True}
        return msg["result"]


# ------------------------------------------------------------------ schemas


def prune_defs(schema):
    """Keep only the $defs reachable from the properties."""
    defs = schema.get("$defs", {})
    keep, todo = set(), [schema.get("properties", {})]
    while todo:
        node = todo.pop()
        text = json.dumps(node)
        for name in defs:
            if f'"#/$defs/{name}"' in text and name not in keep:
                keep.add(name)
                todo.append(defs[name])
    if keep:
        schema["$defs"] = {k: v for k, v in defs.items() if k in keep}
    else:
        schema.pop("$defs", None)
    return schema


def op_schema(real, op):
    tool, _mode, own, required, _ = OPS[op]
    base = real.tools[tool]["inputSchema"]
    s = copy.deepcopy(base)
    props = {k: v for k, v in base["properties"].items() if k in own + WORKSPACE + CORRELATION}
    s["properties"] = props
    s["required"] = required
    if not required:
        s.pop("required")
    s.pop("title", None)
    return prune_defs(s)


OPAQUE = {"type": ["string", "null"],
          "description": "Opaque token from a previous answer with `more_available: true`; echo it verbatim."}


def reduced(real, name):
    s = copy.deepcopy(real.tools[name]["inputSchema"])
    for key in WORKSPACE + CORRELATION + ["max_bytes", "max_items"]:
        s["properties"].pop(key, None)
    if "continuation" in s["properties"]:
        s["properties"]["continuation"] = OPAQUE
    return prune_defs(s)


def tool_list(real):
    a = [real.tools[n] for n in sorted(real.tools)]
    if CANDIDATE == "A":
        return a
    if CANDIDATE == "B":
        return [{**t, "inputSchema": reduced(real, t["name"])} for t in a]
    if CANDIDATE == "C":
        return [{"name": f"brainprint.{op}", "description": OPS[op][4], "inputSchema": op_schema(real, op)}
                for op in OPS]
    if CANDIDATE == "D":
        ops = sorted(OPS)
        return [
            {"name": "brainprint.call",
             "description": "Run one Brainprint operation (indexed, current project truth). `arguments` must match "
                            "the operation's contract from brainprint.contract. Operations: " + ", ".join(ops) + ".",
             "inputSchema": {"type": "object", "required": ["operation"],
                             "properties": {"operation": {"type": "string", "enum": ops},
                                            "arguments": {"type": "object"}}}},
            {"name": "brainprint.contract",
             "description": "The exact argument contract (JSON Schema) of one brainprint.call operation.",
             "inputSchema": {"type": "object", "required": ["operation"],
                             "properties": {"operation": {"type": "string", "enum": ops}}}},
        ]
    if CANDIDATE == "E":
        return a + [{"name": "brainprint.expand",
                     "description": "Full answer behind a compact Brainprint answer's `handle`: source text, evidence "
                                    "spans and verification hashes. Re-runs the request; says whether it changed.",
                     "inputSchema": {"type": "object", "required": ["handle"],
                                     "properties": {"handle": {"type": "string"}}}}]
    raise SystemExit("unknown candidate " + CANDIDATE)


# ------------------------------------------------------------------ results


def text_result(text, is_error=False, structured=None):
    out = {"content": [{"type": "text", "text": text}], "isError": is_error}
    if structured is not None:
        out["structuredContent"] = structured
    return out


def rewrite(result, fn):
    """Apply fn to the JSON envelope of a real result; keep text and structuredContent identical."""
    try:
        env = json.loads(result["content"][0]["text"])
    except (KeyError, IndexError, ValueError):
        return result
    env = fn(env)
    return text_result(json.dumps(env, separators=(",", ":"), ensure_ascii=False), result.get("isError", False), env)


def inner(env):
    """(outer variant, answer body): `{"Inspect": {...}}` or `{"Find": {"Target": {...}}}`."""
    key, body = None, env.get("payload")
    while isinstance(body, dict) and len(body) == 1 and "page" not in body:
        k, v = next(iter(body.items()))
        if not isinstance(v, dict):
            break
        key = key or k
        body = v
    return (key, body) if key else (None, None)


def opaque_out(env):
    _, body = inner(env)
    if body and isinstance(body.get("continuation"), dict):
        raw = json.dumps(body["continuation"], separators=(",", ":")).encode()
        body["continuation"] = "bpc1." + base64.urlsafe_b64encode(raw).decode().rstrip("=")
    return env


def opaque_in(args):
    token = args.get("continuation")
    if isinstance(token, str):
        if not token.startswith("bpc1."):
            raise ValueError("continuation is not a token this server issued")
        raw = token[5:] + "=" * (-len(token[5:]) % 4)
        args["continuation"] = json.loads(base64.urlsafe_b64decode(raw))
    return args


def digest(env):
    return hashlib.sha256(json.dumps(env.get("payload"), sort_keys=True).encode()).hexdigest()[:32]


def compact(env, tool, args):
    """E: summary + handle. Removes detail fields only; every outcome/currentness/coverage/gap field stays."""
    key, body = inner(env)
    if key not in PROJECTED and not (key == "Find" and "page" in body):
        return env
    full_digest = digest(env)
    for item in body.get("page", {}).get("evidence", []):
        if not isinstance(item, dict) or "Full" not in item:
            continue
        kind, v = next(iter(item["Full"].items()))
        if not isinstance(v, dict):
            continue
        if kind == "CurrentSource":
            src = v.pop("source", "")
            v["source_omitted"] = {"bytes": len(src.encode()), "lines": src.count("\n") + 1}
            v["verification"] = {"currentness": v.get("verification", {}).get("currentness")}
        elif kind == "Relation":
            v.pop("evidence", None)
        elif kind == "TargetSelection":
            v["incomplete_coverage"] = len(v.get("incomplete_coverage", []))
    eco = body.get("economy")
    if isinstance(eco, dict):
        body["economy"] = {"more_available": eco.get("more_available"), "omitted_items": eco.get("omitted_items")}
    handle = {"t": tool, "a": args, "d": full_digest}
    env["handle"] = "bph1." + base64.urlsafe_b64encode(json.dumps(handle, separators=(",", ":")).encode()) \
        .decode().rstrip("=")
    env["compact"] = "source text, evidence spans and hashes omitted; brainprint.expand(handle) returns them"
    return env


# ------------------------------------------------------------------ calls


def forward(real, name, args):
    """B only: the integration injects the correlation the Agent no longer supplies."""
    if CANDIDATE == "B":
        args = dict(args, client_id="claude-code", session_id=SESSION)
    return real.call(name, args)


def handle_call(real, name, args):
    meta = {"invalid": False, "lookup": False, "expand": False}
    if CANDIDATE == "A":
        return real.call(name, args), meta
    if CANDIDATE == "B":
        try:
            args = opaque_in(dict(args))
        except ValueError as error:
            meta["invalid"] = True
            return text_result("INVALID_ARGUMENTS: " + str(error), True), meta
        return rewrite(forward(real, name, args), opaque_out), meta
    if CANDIDATE == "C":
        op = name.removeprefix("brainprint.")
        if op not in OPS:
            meta["invalid"] = True
            return text_result("unknown tool " + name, True), meta
        tool, mode, *_ = OPS[op]
        real_args = dict(args, **({"mode": mode} if mode else {}))
        return forward(real, tool, real_args), meta
    if CANDIDATE == "D":
        op = args.get("operation")
        if name == "brainprint.contract":
            meta["lookup"] = True
            if op not in OPS:
                meta["invalid"] = True
                return text_result("unknown operation", True), meta
            body = {"operation": op, "description": OPS[op][4], "arguments_schema": op_schema(real, op)}
            return text_result(json.dumps(body, separators=(",", ":"))), meta
        if name != "brainprint.call" or op not in OPS:
            meta["invalid"] = True
            return text_result("unknown operation; operations: " + ", ".join(sorted(OPS)), True), meta
        arguments = args.get("arguments") or {}
        try:
            jsonschema.Draft202012Validator(op_schema(real, op)).validate(arguments)
        except jsonschema.ValidationError as error:
            meta["invalid"] = True
            return text_result(f"INVALID_ARGUMENTS for {op}: {error.message} (at {list(error.absolute_path)}); "
                               f"brainprint.contract returns the contract", True), meta
        tool, mode, *_ = OPS[op]
        return forward(real, tool, dict(arguments, **({"mode": mode} if mode else {}))), meta
    if CANDIDATE == "E":
        if name == "brainprint.expand":
            meta["expand"] = True
            token = args.get("handle", "")
            try:
                raw = token[5:] + "=" * (-len(token[5:]) % 4)
                handle = json.loads(base64.urlsafe_b64decode(raw))
                assert token.startswith("bph1.")
            except Exception:
                meta["invalid"] = True
                return text_result("INVALID_ARGUMENTS: not a handle this server issued", True), meta
            result = real.call(handle["t"], handle["a"])

            def mark(env):
                env["handle_status"] = "unchanged" if digest(env) == handle["d"] else \
                    "changed: the answer differs from the summary it was issued with; this is the current one"
                return env
            return rewrite(result, mark), meta
        return rewrite(real.call(name, args), lambda env: compact(env, name, args)), meta
    raise SystemExit("unknown candidate")


def main():
    real = Real()
    tools = tool_list(real)
    names = {t["name"] for t in tools}
    for line in sys.stdin:
        if not line.strip():
            continue
        msg = json.loads(line)
        method, mid = msg.get("method"), msg.get("id")
        if mid is None:
            continue  # notifications
        if method == "initialize":
            result = {"protocolVersion": msg["params"].get("protocolVersion", "2025-06-18"),
                      "capabilities": {"tools": {}},
                      "serverInfo": real.init.get("serverInfo"),
                      "instructions": real.init.get("instructions")}
        elif method == "tools/list":
            result = {"tools": tools}
        elif method == "ping":
            result = {}
        elif method == "tools/call":
            name = msg["params"]["name"]
            args = msg["params"].get("arguments") or {}
            start = time.time()
            if name not in names:
                out, meta = text_result("unknown tool " + name, True), {"invalid": True, "lookup": False,
                                                                       "expand": False}
            else:
                out, meta = handle_call(real, name, args)
            text = out["content"][0]["text"] if out.get("content") else ""
            log(tool=name, args_bytes=jbytes(args), result_bytes=len(text.encode()), is_error=out.get("isError"),
                latency_s=round(time.time() - start, 3), **meta)
            result = out
        else:
            sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid,
                                         "error": {"code": -32601, "message": "method not found"}}) + "\n")
            sys.stdout.flush()
            continue
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
