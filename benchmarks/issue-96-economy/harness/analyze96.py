"""#96: N / H / M sessions through the #32 analyzer (multi_compare.analyze, final_analyze grades and
CACHE_MISS handling unchanged). One addition: a Bash call that runs the `brainprint` CLI is a Brainprint
call (`BP:cli:<subcommand>`), so arm H's CLI answers are booked as BP_RESULT, not as native results.

usage: analyze96.py <out-dir> <out.json>
"""

import json
import os
import re
import sys
from collections import defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import breakdown as B  # noqa: E402
import final_analyze  # noqa: E402,F401  (patches MC.grade and B.breakdown)
import multi_compare as MC  # noqa: E402

CLI = re.compile(r"(?:^|[;&|(]\s*)brainprint\s+([a-z-]+)(?:\s+([a-z-]+))?")
_kind = B.kind


def kind(name, inp):
    if name == "Bash":
        m = CLI.search(inp.get("command", ""))
        if m:
            sub = m.group(1) + (":" + m.group(2) if m.group(2) and not m.group(2).startswith("-") else "")
            return "BP:cli:" + sub
    return _kind(name, inp)


B.kind = kind


def native_after_bp(prefix):
    """per turn: native exploration calls after the turn's first Brainprint call"""
    s = MC.load_session(prefix)
    out = []
    for t in range(len(s["turn_results"])):
        seen, n = False, 0
        for j, r in enumerate(s["requests"]):
            if s["req_turn"][j] != t:
                continue
            for cid in r["calls"]:
                k = kind(s["calls"][cid]["name"], s["calls"][cid]["input"])
                if k.startswith("BP:"):
                    seen = True
                elif k != "ToolSearch" and seen:
                    n += 1
        out.append(n)
    return out


def main():
    d, dest = sys.argv[1], sys.argv[2]
    names = sorted(f[: -len(".turns.json")] for f in os.listdir(d) if f.endswith(".turns.json"))
    sessions = []
    for name in names:
        row = MC.analyze(name, os.path.join(d, name))
        for t, n in zip(row["turns"], native_after_bp(os.path.join(d, name))):
            t["native_after_bp"] = n
        sessions.append(row)
    arms = defaultdict(list)
    for s in sessions:
        arms[s["cond"]].append(s)
    agg = {}
    for cond, rows in sorted(arms.items()):
        n = len(rows)
        per_task = defaultdict(list)
        split = defaultdict(float)
        for s in rows:
            for t in s["turns"]:
                per_task[t["task"]].append(t["cost_usd"])
                for k, v in t["cost_split_usd"].items():
                    split[k] += v / n
        tok = lambda key: sum(t["tokens"][key] for s in rows for t in s["turns"]) / n
        agg[cond] = {
            "sessions": n,
            "usd_mean": round(sum(s["total_usd"] for s in rows) / n, 4),
            "usd_each": {s["session"]: s["total_usd"] for s in rows},
            "usd_per_task_mean": {k: round(sum(v) / len(v), 4) for k, v in sorted(per_task.items())},
            "split_usd_session_mean": {k: round(v, 4) for k, v in split.items()},
            "tokens_session_mean": {k: round(tok(k)) for k in ("input", "cache_write", "cache_read", "output")},
            "correct": f"{sum(t['grade']['pass'] for s in rows for t in s['turns'])}/{sum(len(s['turns']) for s in rows)}",
            "failed": [f"{s['session']}:{t['task']}" for s in rows for t in s["turns"] if not t["grade"]["pass"]],
            "native_calls_session_mean": sum(t["native_calls"] for s in rows for t in s["turns"]) / n,
            "native_after_bp_total": sum(t["native_after_bp"] for s in rows for t in s["turns"]),
            "native_bytes_session_mean": round(sum(t["native_bytes"] for s in rows for t in s["turns"]) / n),
            "bp_calls_session_mean": sum(t["bp_calls"] for s in rows for t in s["turns"]) / n,
            "bp_bytes_session_mean": round(sum(t["bp_bytes"] for s in rows for t in s["turns"]) / n),
            "toolsearch_calls_total": sum(s["toolsearch_calls"] for s in rows),
            "cache_classes": sorted({s["cache_class"] for s in rows}),
            "wall_s_session_mean": round(sum(s["wall_s_total"] for s in rows) / n, 1),
        }
    json.dump({"sessions": sessions, "arms": agg}, open(dest, "w"), indent=1)
    print(json.dumps(agg, indent=1))


if __name__ == "__main__":
    main()
