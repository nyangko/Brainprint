#!/usr/bin/env python3
"""#74 arm S: test-only first-route short-circuit (Claude Code PreToolUse command hook).

Reads one PreToolUse event on stdin. For the three P0 actions only (Read = SOURCE_READ, Grep = TEXT_SEARCH,
Glob = PROJECT_TREE_DISCOVERY) it asks the shipped `brainprint` CLI (existing local IPC) and, when the answer is
proven exact + current + complete for exactly the requested range/pattern/scope, denies the native call and puts
the minimum substitute in `permissionDecisionReason` (the only channel the client offers before execution; it
reaches the model as an `is_error` tool result). Anything not proven -> exit 0 without output: the native call
runs unchanged. Every other tool -> exit 0 before any Brainprint query.

Universe proof for Grep/Glob: Claude's Grep/Glob are ripgrep (embedded in the claude binary). The hook enumerates
the native tool's own ripgrep file universe for the scope (`rg --files` + that tool's flags, local, not model-visible) and requires it to equal
Brainprint's indexed File inventory; that enumeration is logged as `local_universe_ms`.

env: S_WS (the Brainprint Workspace root; default: event cwd), S_BP (argv prefix to run the brainprint CLI, JSON list), S_RG (claude binary hosting rg), S_LOG (jsonl).
"""

import json
import os
import re
import subprocess
import sys
import time

CWD = None
GLOB_RE = re.compile(r"^(\*\*/)?\*(\.[A-Za-z0-9_]+)?$")


class Native(Exception):
    """A closed factual reason the native call must run."""


def bp(args, ws, log):
    t = time.time()
    out = subprocess.run(json.loads(os.environ["S_BP"]) + args + ["--workspace", ws, "--json"], capture_output=True, text=True)
    log["bp_queries"] += 1
    log["bp_bytes"] += len(out.stdout.encode())
    log["bp_ms"] += round((time.time() - t) * 1000)
    try:
        resp = json.loads(out.stdout)["outcome"]
    except (ValueError, KeyError):
        raise Native("BP_UNAVAILABLE")
    if "Ok" not in resp:
        raise Native("BP_" + next(iter(resp)).upper())
    return resp["Ok"]


# The file universe each native tool searches (measured against Claude Code 2.1.291's own Grep/Glob output, Phase 1):
# Grep = hidden files, .gitignore respected, .git excluded; Glob = hidden and ignored files, .git included.
UNIVERSE = {"Grep": ["--hidden", "--glob", "!.git"], "Glob": ["--hidden", "--no-ignore"]}


def rg_files(ws, rel, log, tool):
    t = time.time()
    out = subprocess.run(["rg", "--files"] + UNIVERSE[tool] + ["--", rel or "."], stdin=subprocess.DEVNULL, executable=os.environ["S_RG"], cwd=ws, capture_output=True, text=True)
    if out.returncode == 1 and not out.stdout:
        return set()
    log["local_universe_ms"] = round((time.time() - t) * 1000)
    if out.returncode not in (0, 1) or out.stderr.strip():
        raise Native("UNIVERSE_UNREADABLE")
    return {os.path.normpath(p) for p in out.stdout.splitlines()}


def scope(ws, path):
    """Workspace-relative scope ('' = root) of an absolute or cwd-relative path (cwd = the event's, which
    follows the Agent's shell `cd`)."""
    real = os.path.realpath(os.path.join(CWD, path or "."))
    if real != ws and not real.startswith(ws + os.sep):
        raise Native("OUTSIDE_WORKSPACE")
    if not os.path.exists(real):
        raise Native("MISSING")
    return os.path.relpath(real, ws) if real != ws else ""


def bp_inventory(ws, universe, log):
    """Brainprint's indexed Files in every directory that holds a file of the local universe (one non-recursive
    page per directory: `find files` caps at 200 and has no continuation)."""
    inv = set()
    for d in sorted({os.path.dirname(p) for p in universe}):
        files = bp(["find", "files", "--limit", "200", "--kind", "file", "--directory", d], ws, log)["Find"]["Files"]
        if files["truncated"] or len(files["entries"]) >= 200:
            raise Native("BP_TRUNCATED")
        if files["currentness"] != "Current":
            raise Native("NOT_CURRENT")
        inv |= {e["path_rel"] for e in files["entries"] if e["state"] == "Active"}
    return inv


def source_read(ws, ti, log):
    if set(ti) - {"file_path", "offset", "limit"}:
        raise Native("UNSUPPORTED_ARG")
    if "offset" not in ti or "limit" not in ti:
        raise Native("UNBOUNDED_RANGE")
    rel = scope(ws, ti["file_path"])
    if os.path.isdir(os.path.join(ws, rel)):
        raise Native("NOT_A_FILE")
    first, last = ti["offset"] - 1, ti["offset"] + ti["limit"] - 2  # Brainprint lines are 0-based
    insp = bp(["inspect", "--resource-path", rel, "--budget", "wide", "--retention", "disabled"], ws, log)["Inspect"]
    if insp["currentness"] != "Current":
        raise Native("NOT_CURRENT")
    if insp["more_available"]:
        raise Native("PARTIAL")
    lines = {}
    for ev in insp["page"]["evidence"]:
        cs = ev.get("Full", {}).get("CurrentSource")
        if not cs or cs["path_rel"] != rel:
            continue
        src, s, e = cs["source"].split("\n"), cs["span"]["start"], cs["span"]["end"]
        for i, text in enumerate(src):
            n = s["line"] + i
            # a whole line is proven only when the span holds its start and continues past its end
            if (i > 0 or s["column"] == 0) and n < e["line"]:
                lines[n] = text
    if any(n not in lines for n in range(first, last + 1)):
        raise Native("RANGE_NOT_COVERED")
    body = "\n".join(f"{n + 1:6}\t{lines[n]}" for n in range(first, last + 1))
    return f"Brainprint current source, {rel} lines {first + 1}-{last + 1}:\n{body}"


def text_search(ws, ti, log):
    extra = set(ti) - {"pattern", "path", "output_mode", "-i", "-n"}
    if extra:
        raise Native("UNSUPPORTED_ARG")
    mode = ti.get("output_mode", "files_with_matches")
    if mode not in ("files_with_matches", "content", "count") or ti.get("-n") is False:
        raise Native("UNSUPPORTED_ARG")
    pat = ti["pattern"]
    # rg is line-oriented with multi-line ^/$; Brainprint matches over whole text -> only anchor/newline-free patterns
    if re.search(r"[\^$]|\\n|\\A|\\z|\(\?", pat):
        raise Native("UNSUPPORTED_PATTERN")
    rel = scope(ws, ti.get("path"))
    universe = rg_files(ws, rel, log, "Grep")
    if universe != bp_inventory(ws, universe, log) if os.path.isdir(os.path.join(ws, rel)) else universe != {rel}:
        raise Native("UNIVERSE_MISMATCH")
    args = ["find", "text", "--regex", pat, "--search-budget", "wide", "--preview", "--max-results", "100000",
            "--max-files", "100000", "--max-search-bytes", "1000000000", "--max-file-bytes", "1000000000"]
    args += ["--case-insensitive"] if ti.get("-i") else []
    args += ["--path-prefix", rel] if rel else []
    t = bp(args, ws, log)["Find"]["Text"]
    sc = t["scope"]
    if t["structural_currentness"] != "Current":
        raise Native("NOT_CURRENT")
    if any(sc[k] for k in ("binary_skipped", "oversized_skipped", "unreadable", "changed_during_scan")) or sc["budget_exhausted"]:
        raise Native("INCOMPLETE_COVERAGE")
    if sc["files_scanned"] != len(universe):
        raise Native("UNIVERSE_MISMATCH")
    if t["status"] not in ("Found", "NotFound"):
        raise Native("BP_" + t["status"].upper())
    hits = {}
    for m in t["matches"]:
        if m["span"]["start"]["line"] != m["span"]["end"]["line"]:
            raise Native("MULTILINE_MATCH")
        if rel and m["path_rel"] != rel and not m["path_rel"].startswith(rel + "/"):
            raise Native("SCOPE_MISMATCH")
        hits.setdefault(m["path_rel"], {})[m["span"]["start"]["line"]] = m["preview"]
    head = f"Brainprint current complete search of {rel or '.'} ({len(universe)} files):"
    if not hits:
        return head + " no matches"
    if mode == "files_with_matches":
        return head + "\n" + "\n".join(sorted(hits))
    if mode == "count":
        return head + "\n" + "\n".join(f"{p}:{len(ls)}" for p, ls in sorted(hits.items()))
    return head + "\n" + "\n".join(f"{p}:{n + 1}:{ls[n]}" for p, ls in sorted(hits.items()) for n in sorted(ls))


def tree_discovery(ws, ti, log):
    if set(ti) - {"pattern", "path"}:
        raise Native("UNSUPPORTED_ARG")
    m = GLOB_RE.match(ti["pattern"])
    if not m:
        raise Native("UNSUPPORTED_GLOB")
    rel = scope(ws, ti.get("path"))
    if not os.path.isdir(os.path.join(ws, rel)):
        raise Native("NOT_A_DIRECTORY")
    ext = m.group(2) or ""  # Claude Glob (rg --glob): a slash-free pattern matches at any depth, so `*.rs` == `**/*.rs`
    universe = rg_files(ws, rel, log, "Glob")
    inv = bp_inventory(ws, universe, log)
    if universe != inv:
        raise Native("UNIVERSE_MISMATCH")
    hits = sorted(p for p in inv if p.endswith(ext))
    if len(hits) > 100:  # Glob truncates at 100 by mtime; not reproduced
        raise Native("NATIVE_TRUNCATION_SEMANTICS")
    return f"Brainprint current complete file list of {rel or '.'} matching {ti['pattern']} ({len(hits)}):\n" + "\n".join(hits)


ACTIONS = {"Read": ("SOURCE_READ", source_read), "Grep": ("TEXT_SEARCH", text_search), "Glob": ("PROJECT_TREE_DISCOVERY", tree_discovery)}


def main():
    t0 = time.time()
    ev = json.load(sys.stdin)
    tool = ev.get("tool_name")
    log = {"t": t0, "tool_use_id": ev.get("tool_use_id"), "agent_id": ev.get("agent_id"), "tool": tool,
           "input": ev.get("tool_input"), "bp_queries": 0, "bp_bytes": 0, "bp_ms": 0}
    out = None
    if ev.get("hook_event_name") != "PreToolUse" or tool not in ACTIONS:
        log.update(action="UNRELATED", decision="NATIVE", reason="UNRELATED")
    else:
        action, fn = ACTIONS[tool]
        log["action"] = action
        try:
            global CWD
            CWD = os.path.realpath(ev["cwd"])
            sub = fn(os.path.realpath(os.environ.get("S_WS") or ev["cwd"]), ev["tool_input"], log)
            log.update(decision="SUBSTITUTE", reason=None, sub_bytes=len(sub.encode()))
            out = {"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny",
                                          "permissionDecisionReason": sub}}
        except Native as n:
            log.update(decision="NATIVE", reason=str(n))
    log["hook_ms"] = round((time.time() - t0) * 1000)
    if os.environ.get("S_LOG"):
        with open(os.environ["S_LOG"], "a") as f:
            f.write(json.dumps(log) + "\n")
    if out:
        print(json.dumps(out))


if __name__ == "__main__":
    main()
