#!/usr/bin/env python3
"""#75 analysis: per turn and per arm x class x condition, on top of the #32 analyzer (multi_compare.analyze, unchanged:
cost from the client's cumulative total_cost_usd, tokens from usage, tool calls from the stream).

usage: analyze.py <ab-root with out/ and the #32 analyzer modules> <manifest.json> <out-prefix>
Writes <out-prefix>_turns.jsonl (one row per turn) and <out-prefix>.json (aggregates).

Per turn: class (manifest), condition (S1 turn 1 = cold, S1 W* = long-session reuse, other S1 turns = warm,
S2 = second Agent, S3 = third Agent), manifest route (R; B = BRAINPRINT for every task; N = none), observed route:
  BRAINPRINT        Brainprint call(s) and no native exploration after them
  FALLBACK(NATIVE)  native exploration after the turn's first Brainprint call
  NATIVE            native exploration only
  REUSE             no exploration call at all
violation (R/B): NATIVE turn with a Brainprint call or ToolSearch; BRAINPRINT turn whose first exploration call is
native; REUSE turn with any exploration call (= REUSE -> FALLBACK). Grades: #32 graders for T*/W*, manifest graders for A*.
"""

import json
import os
import re
import statistics as st
import sys

AB, MAN, OUTP = sys.argv[1:4]
sys.path.insert(0, AB)
import final_analyze as FA  # noqa: E402  (installs the 6775dac T1/T5/T6/T7 graders into multi_compare)
import multi_compare as MC  # noqa: E402

M = json.load(open(MAN))
EXPLORE = ("Read", "Grep", "Glob", "Bash")


def grade_a(task, final):
    c = MC.cites(final)
    if task == "A1":
        ok = all(n in final for n in ("claude.rs", "codex.rs", "gemini.rs", "mod.rs"))
    elif task == "A2":
        ok = bool(re.search(r"fn now_unix_ms\(\)\s*->\s*u64", final)) and "now_unix_nanos() / 1_000_000" in final
    elif task == "A3":
        ok = MC.has(c, "config.rs", 19, 41, 134, 323, 327, 447)
    elif task == "A4":
        ok = MC.has(c, "telemetry.rs", 25)
    return {"pass": ok}


_g = MC.grade


def grade(task, final):
    if task.startswith("A"):
        return grade_a(task, final)
    return _g(M["tasks"][task].get("repeat_of", task), final)


MC.grade = grade


def condition(ses, pos, task):
    if ses == "S1":
        return "cold" if pos == 1 else ("long_reuse" if task.startswith("W") else "warm")
    return {"S2": "agent2", "S3": "agent3"}[ses]


def observe(kinds):
    ex = [k for k in kinds if k in EXPLORE or k.startswith("BP:")]
    bp = [i for i, k in enumerate(ex) if k.startswith("BP:")]
    if not ex:
        return "REUSE", ex
    if not bp:
        return "NATIVE", ex
    return ("FALLBACK(NATIVE)" if any(k in EXPLORE for k in ex[bp[0] + 1:]) else "BRAINPRINT"), ex


def main():
    out = os.path.join(AB, "out")
    rows = []
    for f in sorted(os.listdir(out)):
        if not f.endswith(".turns.json"):
            continue
        sid = f[: -len(".turns.json")]
        arm, rnd, ses = sid.split("-")
        x = MC.analyze(sid, os.path.join(out, sid))
        s = MC.load_session(os.path.join(out, sid))
        for t in x["turns"]:
            task = t["task"]
            cl = [s["calls"][cid] for j in range(len(s["requests"])) if s["req_turn"][j] == t["pos"] - 1 for cid in s["requests"][j]["calls"]]
            kinds = [MC.B.kind(c["name"], c["input"]) for c in cl]
            obs, ex = observe(kinds)
            route = None if arm == "N" else ("BRAINPRINT" if arm == "B" else M["arms"]["R"]["route"][task])
            viol = None
            if route == "NATIVE" and (any(k.startswith("BP:") for k in kinds) or "ToolSearch" in kinds):
                viol = "BRAINPRINT_OR_TOOLSEARCH_IN_NATIVE_TURN"
            elif route == "BRAINPRINT" and ex and not ex[0].startswith("BP:"):
                viol = "NATIVE_BEFORE_BRAINPRINT"
            elif route == "REUSE" and ex:
                viol = "REUSE_FALLBACK"
            rows.append({"session": sid, "arm": arm, "round": rnd, "ses": ses, "pos": t["pos"], "task": task,
                         "class": M["tasks"][task]["class"], "condition": condition(ses, t["pos"], task),
                         "route": route, "observed": obs, "violation": viol, "pass": bool(t["grade"] and t["grade"]["pass"]),
                         "usd": round(t["cost_usd"], 5), "tokens": t["tokens"], "split_usd": t["cost_split_usd"],
                         "bp_calls": t["bp_calls"], "bp_bytes": t["bp_bytes"], "native_calls": t["native_calls"], "native_bytes": t["native_bytes"],
                         "toolsearch_calls": t["toolsearch_calls"], "toolsearch_tokens": t["toolsearch_tokens"],
                         "native_after_bp": sum(1 for i, k in enumerate(ex) if k in EXPLORE and any(e.startswith("BP:") for e in ex[:i])),
                         "wall_s": t["wall_s"], "api_requests": t["api_requests"], "exploration_sequence": ex,
                         "reuse_miss_declared": "REUSE MISS:" in (s["turn_results"][t["pos"] - 1].get("result") or "")})
        rows[-1]["peak_rss_mib"] = {k: round(v / 1024) for k, v in x["peak_rss_kib"].items()}
    with open(OUTP + "_turns.jsonl", "w") as fh:
        for r in rows:
            fh.write(json.dumps(r, ensure_ascii=False) + "\n")
    agg = {}
    key = lambda r, by: tuple(r[b] for b in by)
    for by in (("arm",), ("arm", "class"), ("arm", "condition"), ("arm", "class", "condition"), ("arm", "task")):
        groups = {}
        for r in rows:
            groups.setdefault(key(r, by), []).append(r)
        agg["/".join(by)] = {"|".join(k): {
            "n_turns": len(v), "rounds": sorted({r["round"] for r in v}),
            "usd_sum_per_round": round(sum(r["usd"] for r in v) / len({r["round"] for r in v}), 4),
            "usd_per_turn_mean": round(st.mean(r["usd"] for r in v), 4),
            "correct": f"{sum(r['pass'] for r in v)}/{len(v)}",
            "tokens_sum_per_round": {t: round(sum(r["tokens"][t] for r in v) / len({r["round"] for r in v})) for t in ("input", "output", "cache_write", "cache_read")},
            "bp_calls": sum(r["bp_calls"] for r in v), "bp_bytes": sum(r["bp_bytes"] for r in v),
            "native_calls": sum(r["native_calls"] for r in v), "native_bytes": sum(r["native_bytes"] for r in v),
            "native_after_bp": sum(r["native_after_bp"] for r in v),
            "toolsearch_calls": sum(r["toolsearch_calls"] for r in v), "toolsearch_tokens": sum(r["toolsearch_tokens"] for r in v),
            "observed": {o: sum(1 for r in v if r["observed"] == o) for o in sorted({r["observed"] for r in v})},
            "violations": {o: sum(1 for r in v if r["violation"] == o) for o in sorted({r["violation"] for r in v if r["violation"]})},
            "wall_s_sum_per_round": round(sum(r["wall_s"] or 0 for r in v) / len({r["round"] for r in v}), 1),
        } for k, v in sorted(groups.items())}
    json.dump({"manifest": MAN, "aggregates": agg}, open(OUTP + ".json", "w"), indent=1, ensure_ascii=False)
    for k, v in agg["arm/class"].items():
        print(k, v["usd_sum_per_round"], v["correct"], "bp", v["bp_calls"], "nat", v["native_calls"], "ts", v["toolsearch_calls"], v["observed"], v["violations"])


if __name__ == "__main__":
    main()
