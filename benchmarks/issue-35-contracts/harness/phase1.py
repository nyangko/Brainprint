"""#35 Phase 1: deterministic contract/workload bytes for candidates A-E (no model, no provider).

usage: phase1.py <proxy.py> <real brainprint-mcp> <workspace> <work-item-uuid> <out.json>
(run with XDG_RUNTIME_DIR/HOME of the isolated daemon serving <workspace>)

Per candidate: the exact tools/list it serves (name + description + inputSchema bytes), then the eight #35
workloads as the calls that candidate needs: D adds one brainprint.contract lookup per operation per workload,
E adds one brainprint.expand where the workload needs source/evidence detail (inspect and change context). Every
continuation an answer offers is followed (up to 3 pages). Bytes are exact UTF-8 bytes of the JSON arguments and
of the result text; no token estimate. Parity: the payload of every A-equivalent call (C, D and E-expanded) must
equal A's; B's may differ only by injected correlation (retained-context references) and the opaque token.
"""

import json
import os
import subprocess
import sys

PROXY, REAL, WS, WI, OUT = sys.argv[1:6]
PY = sys.executable
CANDIDATES = ["A", "B", "C", "D", "E"]
DETAIL = {"inspect", "context_change"}
OPS = {  # op -> (real tool, mode)
    "find_target": ("brainprint.find", "target"), "find_files": ("brainprint.find", "files"),
    "find_text": ("brainprint.find", "text"), "inspect": ("brainprint.inspect", None),
    "relations_direct": ("brainprint.relations", "direct"), "relations_impact": ("brainprint.relations", "impact"),
    "context_change": ("brainprint.context", "change"), "context_resume": ("brainprint.context", "resume"),
    "context_rules": ("brainprint.context", "rules"), "context_work_items": ("brainprint.context", "work_items"),
    "context_lineage": ("brainprint.context", "lineage"), "context_handoffs": ("brainprint.context", "handoffs"),
    "context_structure": ("brainprint.context", "structure"), "context_status": ("brainprint.context", "status"),
}
SIG = {"kind": "structural", "intent": "public_signature_change"}
FP = "@fingerprint_util"  # resolved from find_target at run time


def workloads():
    lwc = {"symbol_name": "load_workspace_config"}
    return {
        "W1 inspect": [("inspect", lwc)],
        "W2 find>inspect": [("find_target", {"symbol_name": "fingerprint"}), ("inspect", {"symbol_id": FP})],
        "W3 find>relations>inspect": [("find_target", {"symbol_name": "fingerprint"}),
                                      ("relations_direct", {"symbol_id": FP, "direction": "incoming",
                                                            "kinds": ["calls"]}),
                                      ("inspect", {"symbol_id": FP})],
        "W4 change": [("find_target", lwc), ("relations_impact", dict(lwc, change=SIG)), ("inspect", lwc),
                      ("context_change", dict(lwc, change=SIG))],
        "W5 resume": [("context_resume", {"work_item": WI}), ("context_rules", {}),
                      ("inspect", {"symbol_name": "now_unix_ms"})],
        "W6 low-frequency": [("context_structure", {"grouping": {"by": "directory_depth", "root": "crates",
                                                                  "depth": 2}}),
                             ("context_lineage", {"lineage_of": "decision",
                                                  "id": "00000000-0000-4000-8000-000000000000"})],
        "W7 inspect x10": [("inspect", lwc)] * 10,
        "W8 mixed": [("find_target", {"symbol_name": "fingerprint"}), ("inspect", {"symbol_id": FP}),
                     ("relations_direct", {"symbol_id": FP, "direction": "incoming", "kinds": ["calls"]}),
                     ("relations_impact", dict(lwc, change=SIG)), ("context_change", dict(lwc, change=SIG)),
                     ("context_resume", {"work_item": WI}), ("context_rules", {}),
                     ("find_text", {"pattern": "CONFIG_FORMAT_VERSION"}), ("inspect", lwc)],
    }


class Client:
    def __init__(self, cand):
        env = dict(os.environ, PROXY_LOG="")
        self.p = subprocess.Popen([PY, PROXY, cand, REAL], cwd=WS, env=env, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, text=True, bufsize=1)
        self.i = 0
        self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                "clientInfo": {"name": "phase1", "version": "0"}})
        self.tools = self.rpc("tools/list", {})["tools"]

    def rpc(self, method, params):
        self.i += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.i, "method": method, "params": params}) + "\n")
        self.p.stdin.flush()
        while True:
            msg = json.loads(self.p.stdout.readline())
            if msg.get("id") == self.i:
                return msg["result"]

    def close(self):
        self.p.stdin.close()
        self.p.wait()


def b(value):
    return len(json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode())


def payload(text):
    try:
        env = json.loads(text)
    except ValueError:
        return None
    return env.get("payload")


def strip_volatile(p):
    """Continuation (A structured / B opaque) differs only in representation."""
    d = json.loads(json.dumps(p))
    body = body_of({"payload": d})
    if body.get("continuation") is not None:
        body["continuation"] = "<continuation>"
    return json.dumps(d, sort_keys=True)


def body_of(env):
    body = env.get("payload")
    while isinstance(body, dict) and len(body) == 1 and "page" not in body:
        body = next(iter(body.values()))
    return body if isinstance(body, dict) else {}


def find_fp(envs):
    items = [i for env in envs for i in body_of(env).get("page", {}).get("evidence", [])]
    for item in items:
        full = item.get("Full", {})
        if "Symbol" in full and full["Symbol"]["path_rel"] == "crates/agent/src/util.rs":
            return full["Symbol"]["symbol"]["id"]
    raise SystemExit("fingerprint in util.rs not found")


def call_for(cand, op, args, looked_up):
    """The tools/call(s) this candidate needs for one operation: [(name, arguments, kind)]."""
    tool, mode = OPS[op]
    real = dict(args, **({"mode": mode} if mode else {}))
    if cand in ("A", "B", "E"):
        return [(tool, real, "call")]
    if cand == "C":
        return [("brainprint." + op, dict(args), "call")]
    if cand == "D":
        calls = []
        if op not in looked_up:
            looked_up.add(op)
            calls.append(("brainprint.contract", {"operation": op}, "lookup"))
        calls.append(("brainprint.call", {"operation": op, "arguments": dict(args)}, "call"))
        return calls
    raise SystemExit(cand)


def run_workload(client, cand, steps):
    rows, looked_up, fp = [], set(), None
    for op, args in steps:
        args = {k: (fp if v == FP else v) for k, v in args.items()}
        pages, seen = 0, []
        while True:
            for name, arguments, kind in call_for(cand, op, args, looked_up):
                res = client.rpc("tools/call", {"name": name, "arguments": arguments})
                text = res["content"][0]["text"]
                row = {"op": op, "tool": name, "kind": kind, "args_bytes": b(arguments),
                       "result_bytes": len(text.encode()), "is_error": bool(res.get("isError"))}
                if kind == "call":
                    row["payload"] = strip_volatile(payload(text))
                    env = json.loads(text) if text.startswith("{") else {}
                    if cand == "E" and op in DETAIL and env.get("handle"):
                        res2 = client.rpc("tools/call", {"name": "brainprint.expand",
                                                         "arguments": {"handle": env["handle"]}})
                        t2 = res2["content"][0]["text"]
                        rows.append(row)
                        row = {"op": op, "tool": "brainprint.expand", "kind": "expand",
                               "args_bytes": b({"handle": env["handle"]}), "result_bytes": len(t2.encode()),
                               "is_error": bool(res2.get("isError")),
                               "payload": strip_volatile(payload(t2)),
                               "handle_status": json.loads(t2).get("handle_status")}
                    last = env
                rows.append(row)
            seen.append(last)
            cont = body_of(last).get("continuation")
            if not cont or pages >= 3:
                if op == "find_target" and fp is None and "fingerprint" in json.dumps(args):
                    fp = find_fp(seen)
                break
            pages += 1
            args = dict(args, continuation=cont)
    return rows


def main():
    out = {"candidates": {}}
    # Warm-up (discarded): the first queries activate the runtime and the semantic backend, which changes
    # their answers (pages, owner confirmation). Every measured candidate then sees the same warm state.
    warm = Client("A")
    for steps in workloads().values():
        run_workload(warm, "A", steps)
    warm.close()
    for cand in CANDIDATES:
        client = Client(cand)
        contracts = [{"name": t["name"], "description_bytes": len(t.get("description", "").encode()),
                      "schema_bytes": b(t["inputSchema"]), "contract_bytes": b(t)} for t in client.tools]
        wl = {}
        for name, steps in workloads().items():
            wl[name] = run_workload(client, cand, steps)
        client.close()
        out["candidates"][cand] = {"tools": contracts, "workloads": wl}
    # parity against A, per workload and position of the A-equivalent payload
    for cand in CANDIDATES[1:]:
        for name in out["candidates"]["A"]["workloads"]:
            a = [r["payload"] for r in out["candidates"]["A"]["workloads"][name] if r["kind"] == "call"]
            rows = out["candidates"][cand]["workloads"][name]
            c = [r["payload"] for r in rows if r["kind"] == ("expand" if cand == "E" else "call")
                 or (cand == "E" and r["kind"] == "call" and r["op"] not in DETAIL)]
            if cand == "E":
                c = []
                for i, r in enumerate(rows):
                    if r["kind"] == "expand":
                        c.append(r["payload"])
                    elif r["kind"] == "call" and not (i + 1 < len(rows) and rows[i + 1]["kind"] == "expand"):
                        c.append(None)  # compact by design; checked separately
            same = [x is None or x == y for x, y in zip(c, a)]
            out["candidates"][cand].setdefault("parity", {})[name] = {
                "a_calls": len(a), "candidate_calls": len(c), "equal": sum(same), "compared": len(same)}
    for cand in out["candidates"].values():
        for rows in cand["workloads"].values():
            for r in rows:
                r.pop("payload", None)
    json.dump(out, open(OUT, "w"), indent=1)


if __name__ == "__main__":
    main()
