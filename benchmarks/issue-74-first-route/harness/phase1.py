#!/usr/bin/env python3
"""#74 Phase 1: deterministic decision matrix for s_hook.py (no model).

usage: phase1.py <fixture-workspace> <iso-wrapper> <out.json>
Feeds synthetic Claude Code PreToolUse events to s_hook.py and records decision, closed reason, Brainprint query
count and substitute. Mutation cases (revision / universe changes, stale daemon) mutate the fixture right before the
event and record the disk content at hook time; static substitutes are checked against Claude's own native tool
output by phase1_native.py.
"""

import json
import os
import subprocess
import sys
import time

H = os.path.dirname(os.path.abspath(__file__))
W, ISO, OUT = sys.argv[1], sys.argv[2], sys.argv[3]
W = os.path.realpath(W)
ENV = dict(os.environ, S_BP=json.dumps([ISO, "brainprint"]), S_RG="/Users/pixel/.local/bin/claude", S_LOG=OUT + ".hook.jsonl", S_WS=W)


def hook(tool, ti, cwd=None):
    ev = {"session_id": "p1", "cwd": cwd or W, "hook_event_name": "PreToolUse", "tool_name": tool, "tool_input": ti, "tool_use_id": "p1"}
    out = subprocess.run([sys.executable, H + "/s_hook.py"], input=json.dumps(ev), capture_output=True, text=True, env=ENV)
    with open(OUT + ".hook.jsonl") as f:
        log = json.loads(f.readlines()[-1])
    sub = json.loads(out.stdout)["hookSpecificOutput"]["permissionDecisionReason"] if out.stdout.strip() else None
    return log, sub


def p(rel):
    return os.path.join(W, rel)


def edit(rel, old, new):
    s = open(p(rel)).read()
    open(p(rel), "w").write(s.replace(old, new))


# (id, action, expectation, tool, input, setup, teardown); expectation: "SUBSTITUTE" or the closed NATIVE reason
CASES = [
    ("R1 exact bounded range", "SOURCE_READ", "SUBSTITUTE", "Read", {"file_path": p("src/lib.rs"), "offset": 6, "limit": 4}),
    ("R2 larger range", "SOURCE_READ", "RANGE_NOT_COVERED", "Read", {"file_path": p("src/lib.rs"), "offset": 5, "limit": 20}),
    ("R3 different range (imports)", "SOURCE_READ", "RANGE_NOT_COVERED", "Read", {"file_path": p("src/lib.rs"), "offset": 1, "limit": 3}),
    ("R4 range incl. declaration end line", "SOURCE_READ", "RANGE_NOT_COVERED", "Read", {"file_path": p("src/lib.rs"), "offset": 6, "limit": 6}),
    ("R5 unbounded whole file", "SOURCE_READ", "UNBOUNDED_RANGE", "Read", {"file_path": p("src/lib.rs")}),
    ("R6 missing", "SOURCE_READ", "MISSING", "Read", {"file_path": p("src/nope.rs"), "offset": 1, "limit": 1}),
    ("R7 outside Workspace", "SOURCE_READ", "OUTSIDE_WORKSPACE", "Read", {"file_path": "/etc/hosts", "offset": 1, "limit": 1}),
    ("R8 no source spans (partial: .txt)", "SOURCE_READ", "RANGE_NOT_COVERED", "Read", {"file_path": p("docs/notes.txt"), "offset": 1, "limit": 1}),
    ("R9 unsupported arg (pages)", "SOURCE_READ", "UNSUPPORTED_ARG", "Read", {"file_path": p("src/lib.rs"), "offset": 6, "limit": 4, "pages": "1"}),
    ("R10 method inside impl (span starts mid-line)", "SOURCE_READ", "SUBSTITUTE", "Read", {"file_path": p("src/lib.rs"), "offset": 21, "limit": 2}),
    ("G1 literal", "TEXT_SEARCH", "SUBSTITUTE", "Grep", {"pattern": "alpha_helper", "path": "src"}),
    ("G2 regex content", "TEXT_SEARCH", "SUBSTITUTE", "Grep", {"pattern": "fn [a-z_]+\\(", "path": "src", "output_mode": "content"}),
    ("G3 case-sensitive", "TEXT_SEARCH", "SUBSTITUTE", "Grep", {"pattern": "ALPHA", "path": "src", "output_mode": "content", "-n": True}),
    ("G4 case-insensitive", "TEXT_SEARCH", "SUBSTITUTE", "Grep", {"pattern": "alpha", "path": "src", "output_mode": "content", "-i": True}),
    ("G5 count", "TEXT_SEARCH", "SUBSTITUTE", "Grep", {"pattern": "alpha", "path": "src", "output_mode": "count", "-i": True}),
    ("G6 single file scope", "TEXT_SEARCH", "SUBSTITUTE", "Grep", {"pattern": "LIMIT", "path": "src/lib.rs", "output_mode": "content"}),
    ("G7 no match", "TEXT_SEARCH", "SUBSTITUTE", "Grep", {"pattern": "zzz_nothing", "path": "src"}),
    ("G8 unsupported flag -C", "TEXT_SEARCH", "UNSUPPORTED_ARG", "Grep", {"pattern": "alpha", "path": "src", "output_mode": "content", "-C": 2}),
    ("G9 unsupported glob filter", "TEXT_SEARCH", "UNSUPPORTED_ARG", "Grep", {"pattern": "alpha", "path": "src", "glob": "*.rs"}),
    ("G10 unsupported type filter", "TEXT_SEARCH", "UNSUPPORTED_ARG", "Grep", {"pattern": "alpha", "type": "rust"}),
    ("G11 anchored pattern", "TEXT_SEARCH", "UNSUPPORTED_PATTERN", "Grep", {"pattern": "^pub", "path": "src"}),
    ("G12 match would span lines", "TEXT_SEARCH", "MULTILINE_MATCH", "Grep", {"pattern": "counts\\s+\\}", "path": "src"}),
    ("G13 root scope (hidden/gitignored universe differs)", "TEXT_SEARCH", "UNIVERSE_MISMATCH", "Grep", {"pattern": "alpha", "-i": True}),
    ("G14 outside Workspace", "TEXT_SEARCH", "OUTSIDE_WORKSPACE", "Grep", {"pattern": "alpha", "path": "/etc"}),
    ("G15 Agent shell cwd drifted to src, relative scope", "TEXT_SEARCH", "SUBSTITUTE", "Grep", {"pattern": "alpha_helper", "path": "sub"}, "src"),
    ("G16 Agent shell cwd drifted to src, scope = Workspace root", "TEXT_SEARCH", "UNIVERSE_MISMATCH", "Grep", {"pattern": "alpha", "path": W}, "src"),
    ("G17 hidden directory scope", "TEXT_SEARCH", "SUBSTITUTE", "Grep", {"pattern": "alpha", "path": ".hidden", "output_mode": "content"}),
    ("G18 gitignored directory scope (native skips nothing explicit)", "TEXT_SEARCH", "SUBSTITUTE", "Grep", {"pattern": "alpha", "path": "ignored"}),
    ("T1 recursive ext", "PROJECT_TREE_DISCOVERY", "SUBSTITUTE", "Glob", {"pattern": "**/*.rs", "path": p("src")}),
    ("T2 recursive all", "PROJECT_TREE_DISCOVERY", "SUBSTITUTE", "Glob", {"pattern": "**/*", "path": p("src")}),
    ("T3 slash-free pattern (matches at any depth)", "PROJECT_TREE_DISCOVERY", "SUBSTITUTE", "Glob", {"pattern": "*.rs", "path": p("src")}),
    ("T4 changed scope (src/sub)", "PROJECT_TREE_DISCOVERY", "SUBSTITUTE", "Glob", {"pattern": "**/*", "path": p("src/sub")}),
    ("T5 root scope (hidden/gitignored universe differs)", "PROJECT_TREE_DISCOVERY", "UNIVERSE_MISMATCH", "Glob", {"pattern": "**/*.rs"}),
    ("T8 hidden directory scope", "PROJECT_TREE_DISCOVERY", "SUBSTITUTE", "Glob", {"pattern": "**/*", "path": p(".hidden")}),
    ("T9 gitignored directory scope", "PROJECT_TREE_DISCOVERY", "SUBSTITUTE", "Glob", {"pattern": "**/*.rs", "path": p("ignored")}),
    ("T6 filtered glob (path inside pattern)", "PROJECT_TREE_DISCOVERY", "UNSUPPORTED_GLOB", "Glob", {"pattern": "src/**/*.rs"}),
    ("T7 filtered glob (alternation)", "PROJECT_TREE_DISCOVERY", "UNSUPPORTED_GLOB", "Glob", {"pattern": "**/{lib,util}.rs", "path": p("src")}),
    ("U1 unrelated Bash", "UNRELATED", "UNRELATED", "Bash", {"command": "ls src"}),
    ("U2 unrelated ToolSearch", "UNRELATED", "UNRELATED", "ToolSearch", {"query": "select:Read"}),
    ("U3 unrelated Edit", "UNRELATED", "UNRELATED", "Edit", {"file_path": p("src/lib.rs"), "old_string": "a", "new_string": "b"}),
]

# Mutation cases: (id, action, tool, input, mutate, restore). Pass = NATIVE, or SUBSTITUTE equal to disk at hook time.
MUT = [
    ("M1 revision changed just before read", "SOURCE_READ", "Read", {"file_path": p("src/lib.rs"), "offset": 6, "limit": 4},
     lambda: edit("src/lib.rs", "let mut counts", "let mut tally"), lambda: edit("src/lib.rs", "let mut tally", "let mut counts")),
    ("M2 revision changed just before search", "TEXT_SEARCH", "Grep", {"pattern": "alpha_helper", "path": "src", "output_mode": "content"},
     lambda: edit("src/sub/util.rs", "pub fn alpha_helper", "pub fn beta_helper"), lambda: edit("src/sub/util.rs", "pub fn beta_helper", "pub fn alpha_helper")),
    ("M3 resource universe changed (file added)", "PROJECT_TREE_DISCOVERY", "Glob", {"pattern": "**/*.rs", "path": p("src")},
     lambda: open(p("src/sub/new.rs"), "w").write("pub fn added() {}\n"), lambda: os.remove(p("src/sub/new.rs"))),
    ("M4 resource universe changed (file removed)", "TEXT_SEARCH", "Grep", {"pattern": "alpha", "path": "src", "-i": True},
     lambda: os.rename(p("src/sub/util.rs"), p("util.rs.bak")), lambda: os.rename(p("util.rs.bak"), p("src/sub/util.rs"))),
    ("M5 unreadable resource (search)", "TEXT_SEARCH", "Grep", {"pattern": "alpha", "path": "src", "-i": True},
     lambda: os.chmod(p("src/sub/util.rs"), 0), lambda: os.chmod(p("src/sub/util.rs"), 0o644)),
    ("M6 unreadable directory (tree)", "PROJECT_TREE_DISCOVERY", "Glob", {"pattern": "**/*", "path": p("src")},
     lambda: os.chmod(p("src/sub"), 0), lambda: os.chmod(p("src/sub"), 0o755)),
    ("M7 unreadable resource (read)", "SOURCE_READ", "Read", {"file_path": p("src/sub/util.rs"), "offset": 2, "limit": 2},
     lambda: os.chmod(p("src/sub/util.rs"), 0), lambda: os.chmod(p("src/sub/util.rs"), 0o644)),
]


def disk_state(tool, ti):
    """What a native call would see right now (for mutation verdicts only)."""
    if tool == "Read":
        try:
            ls = open(ti["file_path"]).read().split("\n")
        except OSError as e:
            return "ERR " + e.__class__.__name__
        return "\n".join(ls[ti["offset"] - 1: ti["offset"] - 1 + ti["limit"]])
    out = subprocess.run(["rg", "--files", "src"], executable="/Users/pixel/.local/bin/claude", cwd=W, capture_output=True, text=True)
    return out.stdout + out.stderr


def main():
    open(OUT + ".hook.jsonl", "w").close()
    rows = []
    for cid, action, expect, tool, ti, *cwd in CASES:
        log, sub = hook(tool, ti, os.path.join(W, cwd[0]) if cwd else None)
        got = "SUBSTITUTE" if log["decision"] == "SUBSTITUTE" else log["reason"]
        rows.append({"id": cid, "action": action, "tool": tool, "input": ti, "expect": expect, "decision": log["decision"],
                     "reason": log["reason"], "match_expectation": got == expect, "bp_queries": log["bp_queries"],
                     "bp_bytes": log["bp_bytes"], "sub_bytes": log.get("sub_bytes"), "hook_ms": log["hook_ms"], "substitute": sub})
    for cid, action, tool, ti, mutate, restore in MUT:
        for when in ("immediate", "settled"):
            mutate()
            if when == "settled":
                time.sleep(3)
            log, sub = hook(tool, ti)
            disk = disk_state(tool, ti)
            restore()
            time.sleep(3)
            rows.append({"id": f"{cid} [{when}]", "action": action, "tool": tool, "input": ti, "expect": "NATIVE or current SUBSTITUTE",
                         "decision": log["decision"], "reason": log["reason"], "bp_queries": log["bp_queries"],
                         "bp_bytes": log["bp_bytes"], "sub_bytes": log.get("sub_bytes"), "hook_ms": log["hook_ms"],
                         "substitute": sub, "disk_at_hook": disk})
    json.dump(rows, open(OUT, "w"), indent=1)
    for r in rows:
        print(f"{r['id'][:52]:52} {r['decision']:10} {str(r['reason']):22} q={r['bp_queries']} {r.get('match_expectation', '')}")


if __name__ == "__main__":
    main()
