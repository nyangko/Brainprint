#!/usr/bin/env python3
"""#74 Phase 1 ground truth: run every static SUBSTITUTE case of phase1.json as the REAL Claude Code native tool
(no S hook; a PostToolUse logger captures `tool_response`) and compare facts with the substitute.

usage: phase1_native.py <fixture-workspace> <phase1.json> <out.json>
Facts compared: Read -> (line number, text) list; Grep files -> path set, content -> (path, line, text) set,
count -> {path: n}; Glob -> path set. A substitute whose facts differ is a false short-circuit.
"""

import json
import os
import re
import subprocess
import sys

W, P1, OUT = os.path.realpath(sys.argv[1]), sys.argv[2], sys.argv[3]
LOGGER = "import json,sys,os;e=json.load(sys.stdin);open(os.environ['N_LOG'],'a').write(json.dumps(e)+'\\n')"


def rel(path):
    return os.path.relpath(os.path.join(W, path), W)


def facts_native(tool, ti, resp):
    if tool == "Read":
        f = resp["file"]
        return [(f["startLine"] + i, t) for i, t in enumerate(f["content"].split("\n"))]
    if tool == "Glob":
        return sorted(rel(x) for x in resp["filenames"])
    mode = resp.get("mode")
    if mode == "files_with_matches":
        return sorted(rel(x) for x in resp["filenames"])
    if mode == "count":
        return sorted(tuple(l.rsplit(":", 1)) for l in resp["content"].splitlines() if l and ":" in l)
    out = []
    for l in resp["content"].splitlines():
        m = re.match(r"^(.*?):(\d+):(.*)$", l)
        if m:
            path = m.group(1) if ti["path"].endswith(".rs") is False else ti["path"]
            out.append((rel(path), int(m.group(2)), m.group(3)))
        elif l:  # single-file content mode prints "line:text" without the path
            n, t = l.split(":", 1)
            out.append((rel(ti["path"]), int(n), t))
    return sorted(out)


def facts_sub(tool, ti, sub):
    body = sub.split("\n")[1:]
    if tool == "Read":
        return [(int(l.split("\t", 1)[0]), l.split("\t", 1)[1]) for l in body]
    if tool == "Glob":
        return sorted(body)
    if sub.endswith(" no matches"):
        return []
    mode = ti.get("output_mode", "files_with_matches")
    if mode == "files_with_matches":
        return sorted(body)
    if mode == "count":
        return sorted(tuple(l.rsplit(":", 1)) for l in body)
    return sorted((m.group(1), int(m.group(2)), m.group(3)) for m in (re.match(r"^(.*?):(\d+):(.*)$", l) for l in body))


def main():
    rows = [r for r in json.load(open(P1)) if r["decision"] == "SUBSTITUTE" and not r["id"].startswith("M") and "drifted" not in r["id"]]  # drifted-cwd cases: compared in phase1.py output by hand
    calls = "\n".join(f"{i + 1}. {r['tool']} with input {json.dumps(r['input'])}" for i, r in enumerate(rows))
    prompt = ("Make exactly these tool calls, in this order, each exactly once with exactly the given input "
              "(no other tools, no changes to the inputs), then reply with only the word done.\n" + calls)
    log = OUT + ".post.jsonl"
    open(log, "w").close()
    settings = OUT + ".settings.json"
    json.dump({"hooks": {"PostToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": f"python3 -c \"{LOGGER}\""}]}]}}, open(settings, "w"))
    mcp = OUT + ".mcp.json"
    json.dump({"mcpServers": {}}, open(mcp, "w"))
    subprocess.run(["claude", "-p", prompt, "--model", "claude-opus-5-5", "--effort", "medium", "--setting-sources", "project",
                    "--settings", settings, "--strict-mcp-config", "--mcp-config", mcp, "--permission-mode", "dontAsk",
                    "--allowedTools", "Read", "Grep", "Glob", "--output-format", "json"],
                   cwd=W, env=dict(os.environ, N_LOG=log), stdin=subprocess.DEVNULL, capture_output=True, text=True)
    posts = [json.loads(l) for l in open(log)]
    out = []
    for r in rows:
        hit = next((e for e in posts if e["tool_name"] == r["tool"] and e["tool_input"] == r["input"]), None)
        if not hit:
            out.append({"id": r["id"], "verdict": "NATIVE_CALL_NOT_OBSERVED"})
            continue
        nat, sub = facts_native(r["tool"], r["input"], hit["tool_response"]), facts_sub(r["tool"], r["input"], r["substitute"])
        out.append({"id": r["id"], "verdict": "EQUAL" if [list(x) if isinstance(x, tuple) else x for x in nat] == [list(x) if isinstance(x, tuple) else x for x in sub] else "FALSE_SHORT_CIRCUIT",
                    "native_facts": nat, "substitute_facts": sub, "native_response_bytes": len(json.dumps(hit["tool_response"]).encode()),
                    "substitute_bytes": r["sub_bytes"]})
    json.dump(out, open(OUT, "w"), indent=1, default=list)
    for o in out:
        print(f"{o['id'][:50]:50} {o['verdict']}")


if __name__ == "__main__":
    main()
