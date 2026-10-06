#!/usr/bin/env python3
"""#74 Phase 2: first-route specifics per session, on top of the #32 analyzer (cost/tokens/grades).

usage: s_analyze.py <out_s dir> <wts dir> <out.json>
Per session, from <id>.stream.jsonl (+ <id>.hook.jsonl for S):
- native tool calls requested by the Agent, how many actually executed (S: minus substituted), executed result bytes;
- S: hook events, substitutes per action, fallbacks per closed reason, Brainprint internal queries/bytes/ms,
  hook round-trip ms, Agent-visible substitute bytes;
- immediate retry: the Agent's next tool call after a substitute has the same tool + input;
- rediscovery (auto, heuristic): a later executed native call in the same turn that targets the substituted
  Read's file / Grep's pattern / Glob's scope again (Read/Grep/Glob input, or the string inside a Bash command);
- false short-circuit: each substitute re-checked against ripgrep (the engine of Claude's Grep/Glob, Phase 1) and
  the file on disk in the session's own read-only clone.
"""

import json
import os
import re
import subprocess
import sys

RG = "/Users/pixel/.local/bin/claude"
NATIVE = {"Read", "Grep", "Glob", "Bash"}


def events(path):
    for l in open(path):
        try:
            yield json.loads(l)
        except ValueError:
            pass


def rg(ws, args):
    return subprocess.run(["rg"] + args, executable=RG, cwd=ws, capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout


def truth(ws, tool, ti):
    """Native facts for a substituted call, in the substitute's own line format."""
    rel = lambda p: os.path.relpath(os.path.join(ws, p or "."), ws)
    if tool == "Read":
        ls = open(os.path.join(ws, ti["file_path"])).read().split("\n")
        return [f"{n + 1:6}\t{ls[n]}" for n in range(ti["offset"] - 1, ti["offset"] - 1 + ti["limit"])]
    if tool == "Glob":
        ext = re.match(r"^(?:\*\*/)?\*(\.[A-Za-z0-9_]+)?$", ti["pattern"]).group(1) or ""
        return sorted(p for p in rg(ws, ["--files", "--hidden", "--no-ignore", "--", rel(ti.get("path"))]).splitlines() if p.endswith(ext))
    r, mode = rel(ti.get("path")), ti.get("output_mode", "files_with_matches")
    flags = ["--hidden", "--glob", "!.git"] + (["-i"] if ti.get("-i") else [])
    if mode == "files_with_matches":
        return sorted(rg(ws, flags + ["-l", "--", ti["pattern"], r]).splitlines())
    if mode == "count":
        out = rg(ws, flags + ["-c", "--with-filename", "--", ti["pattern"], r]).splitlines()
        return sorted(out)
    out = rg(ws, flags + ["-n", "--with-filename", "--no-heading", "--", ti["pattern"], r]).splitlines()
    return sorted(out, key=lambda l: (l.split(":", 2)[0], int(l.split(":", 2)[1])))


def sub_facts(tool, ti, sub):
    body = sub.split("\n")[1:]
    if sub.endswith(" no matches"):
        return []
    if tool == "Read":
        return body
    if tool == "Grep" and ti.get("output_mode") == "content":
        return sorted(body, key=lambda l: (l.split(":", 2)[0], int(l.split(":", 2)[1])))
    return sorted(body)


def target_of(tool, ti):
    return {"Read": ti.get("file_path"), "Grep": ti.get("pattern"), "Glob": ti.get("path") or ti.get("pattern")}[tool]


def main():
    d, wts, outp = sys.argv[1:4]
    res = {}
    for f in sorted(os.listdir(d)):
        if not f.endswith(".stream.jsonl"):
            continue
        sid = f.split(".")[0]
        ws = os.path.realpath(os.path.join(wts, sid))
        calls, results, turn, order = {}, {}, 0, []
        cost = None
        for e in events(os.path.join(d, f)):
            if e.get("type") == "assistant":
                for c in e["message"]["content"]:
                    if c.get("type") == "tool_use":
                        calls[c["id"]] = {"tool": c["name"], "input": c["input"], "turn": turn}
                        order.append(c["id"])
            elif e.get("type") == "user" and isinstance(e["message"]["content"], list):
                for c in e["message"]["content"]:
                    if isinstance(c, dict) and c.get("type") == "tool_result":
                        txt = c["content"] if isinstance(c["content"], str) else json.dumps(c["content"])
                        results[c["tool_use_id"]] = {"bytes": len(txt.encode()), "error": bool(c.get("is_error")), "text": txt}
            elif e.get("type") == "result":
                turn += 1
        hooks = {}
        hp = os.path.join(d, sid + ".hook.jsonl")
        if os.path.exists(hp):
            for h in events(hp):
                hooks[h["tool_use_id"]] = h
        subs = {k for k, h in hooks.items() if h["decision"] == "SUBSTITUTE"}
        nat = [k for k in order if calls[k]["tool"] in NATIVE]
        executed = [k for k in nat if k not in subs]
        row = {"native_requested": len(nat), "native_executed": len(executed),
               "native_executed_by_tool": {t: sum(1 for k in executed if calls[k]["tool"] == t) for t in sorted(NATIVE)},
               "native_executed_result_bytes": sum(results.get(k, {}).get("bytes", 0) for k in executed),
               "first_exploration_route": calls[nat[0]]["tool"] if nat else None}
        if hooks:
            hv = list(hooks.values())
            p0 = [h for h in hv if h["action"] != "UNRELATED"]
            row.update({
                "hook_events": len(hv), "p0_events": len(p0), "substitutes": len(subs),
                "substitutes_by_action": {a: sum(1 for k in subs if hooks[k]["action"] == a) for a in ("SOURCE_READ", "TEXT_SEARCH", "PROJECT_TREE_DISCOVERY")},
                "fallback_reasons": {r: sum(1 for h in p0 if h["reason"] == r) for r in sorted({h["reason"] for h in p0 if h["reason"]})},
                "bp_queries": sum(h["bp_queries"] for h in hv), "bp_bytes": sum(h["bp_bytes"] for h in hv), "bp_ms": sum(h["bp_ms"] for h in hv),
                "unrelated_bp_queries": sum(h["bp_queries"] for h in hv if h["action"] == "UNRELATED"),
                "hook_ms_total": sum(h["hook_ms"] for h in hv), "hook_ms_p0_max": max([h["hook_ms"] for h in p0] or [0]),
                "substitute_bytes_hook": sum(hooks[k]["sub_bytes"] for k in subs),
                "substitute_bytes_agent_visible": sum(results.get(k, {}).get("bytes", 0) for k in subs),
            })
            retry, redisc, fsc = [], [], []
            for k in subs:
                i = order.index(k)
                c = calls[k]
                if i + 1 < len(order) and calls[order[i + 1]]["tool"] == c["tool"] and calls[order[i + 1]]["input"] == c["input"]:
                    retry.append(k)
                tgt = target_of(c["tool"], c["input"])
                rel_tgt = os.path.relpath(tgt, ws) if tgt and tgt.startswith("/") else tgt
                for j in order[i + 1:]:
                    cj = calls[j]
                    if j in subs or cj["turn"] != c["turn"] or cj["tool"] not in NATIVE:
                        continue
                    blob = json.dumps(cj["input"])
                    if tgt and (tgt in blob or (rel_tgt and rel_tgt in blob)):
                        redisc.append({"after": c["tool"], "target": rel_tgt, "by": cj["tool"], "input": cj["input"]})
                try:
                    ok = truth(ws, c["tool"], c["input"]) == sub_facts(c["tool"], c["input"], hooks_sub(results[k]["text"]))
                except Exception as ex:  # an unverifiable substitute counts as false
                    ok = f"UNVERIFIED {ex.__class__.__name__}"
                if ok is not True:
                    fsc.append({"tool": c["tool"], "input": c["input"], "check": ok})
            row.update({"immediate_retry": len(retry), "rediscovery_auto": redisc, "false_short_circuit": fsc,
                        "substitutes_detail": [{"tool": calls[k]["tool"], "input": calls[k]["input"], "bytes": results.get(k, {}).get("bytes")} for k in subs]})
        res[sid] = row
    json.dump(res, open(outp, "w"), indent=1)
    for sid, r in res.items():
        print(sid, {k: v for k, v in r.items() if k not in ("substitutes_detail", "rediscovery_auto", "false_short_circuit")},
              "rediscovery", len(r.get("rediscovery_auto", [])), "fsc", len(r.get("false_short_circuit", [])))


def hooks_sub(text):
    """The substitute as delivered: the client prefixes `PreToolUse:<Tool> hook error: `."""
    return re.sub(r"^PreToolUse:\w+ hook error: ", "", text)


if __name__ == "__main__":
    main()
