"""I5 Task 14 final rerun (base 6775dac): grading, call-level telemetry, per-turn cost split, A/B aggregate.

usage: final_analyze.py <out-dir> <out-prefix>
Writes <out-prefix>_calls.jsonl (one row per tool call) and <out-prefix>.json (sessions, turns, aggregates).
Session ids: <A|B>-<O1|O2>-<r1|r2>; files from run_final.sh / session.py.

Cost split and token accounting are multi_compare.analyze (unchanged). On top of it, per tool call:
- Brainprint calls: mode, safe arguments, wire bytes, currentness/status, more_available, continuation used,
  economy block as the daemon reported it (raw_available/prepared/delivered), gaps/unconfirmed caller owners;
- native calls: tool, target path/pattern/command, result bytes, and against every earlier Brainprint answer of
  the same session (one revision: the runs are read-only): `content_overlap` = share of the native result's
  non-trivial lines (>= 12 chars after strip) that already appear verbatim in a Brainprint answer;
- phase per turn: native-before-BP (no Brainprint call yet in this turn) / native-after-BP;
- `auto_class` for native-after-BP calls (reviewed by hand afterwards, see the summary):
  REDUNDANT_CANDIDATE  overlap >= 0.8 and the delivering answer was Current, not more_available, without gaps
  FALLBACK_BOUNDED     the latest earlier Brainprint answer had more_available and its continuation was not requested
  FALLBACK_PARTIAL     the latest earlier Brainprint answer reported gaps / unconfirmed caller owners
  FALLBACK_ERROR       the latest earlier Brainprint answer was an error / not found
  NOT_DELIVERED        otherwise (Brainprint did not deliver that content in this session)
- `post_process`: a Bash call whose pipeline sorts/dedupes/counts/filters (sort, uniq, wc, awk, cut, head/tail).
"""

import json
import os
import re
import statistics as st
import sys
from collections import defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import breakdown as B  # noqa: E402
import multi_compare as MC  # noqa: E402

CATS = MC.CATS


PREFIX = re.compile(r"^\s*(?:[\w./\-]+[:\-])?\d+(?:[:\t→\-]|\s{2,})")  # grep `path:12:`, Read `12\t` / `12→`, `cat -n`


def lines_set(text):
    out = set()
    for l in text.splitlines():
        l = PREFIX.sub("", l, count=1).strip()
        if len(l) >= 12:
            out.add(l)
    return out


def strings(o, acc):
    if isinstance(o, str):
        acc.append(o)
    elif isinstance(o, dict):
        for v in o.values():
            strings(v, acc)
    elif isinstance(o, list):
        for v in o:
            strings(v, acc)
    return acc


def grade(task, final):
    c = MC.cites(final)
    has = MC.has
    if task == "T1":
        g = {"signature": "load_workspace_config(paths: &WorkspacePaths) -> Result<WorkspaceConfig, ConfigError>" in final,
             "def": has(c, "config.rs", 233),
             "sites": has(c, "lifecycle.rs", 215) and has(c, "maintenance.rs", 430, 714) and has(c, "query_surface.rs", 551)
             and has(c, "config.rs", 482, 498, 508, 561, 574, 586),
             "false_calls": [f"{f}:{n}" for f, ns in (("lifecycle.rs", (26,)), ("maintenance.rs", (38,)), ("query_surface.rs", (29,))) for n in ns if has(c, f, n)]}
    elif task == "T5":
        variants = all(re.search(r"\b" + v + r"\b", final) for v in ("MissingParent", "Io", "Decode", "Encode", "UnsupportedFormat"))
        fns = all(re.search(r"\b" + v + r"\b", final) for v in ("bootstrap_global_config", "bootstrap_workspace_config", "load_global_config", "load_workspace_config"))
        g = {"signature": bool(re.search(r"CONFIG_FORMAT_VERSION[^\n]{0,60}\b1\b", final)), "def": has(c, "config.rs", 19),
             "sites": has(c, "config.rs", *range(318, 332), any_of=True) and "validate_format" in final and "UnsupportedFormat" in final and variants and fns,
             "false_calls": []}
    elif task == "T6":
        return _mc_grade("T6", final)
    elif task == "T7":
        g = {"signature": bool(re.search(r"fn now_unix_ms\(\)\s*->\s*u64", final)) and "u128" in final and "now_unix_nanos" in final,
             "def": has(c, "util.rs", 60),
             "sites": has(c, "state.rs", 256) and has(c, "telemetry.rs", 57), "false_calls": []}
    else:
        raise ValueError(task)
    g["pass"] = g["pass_recall"] = g["signature"] and g["def"] and g["sites"]
    return g


_mc_grade = MC.grade
MC.grade = grade  # multi_compare.analyze grades through this name


def misses(reqs):
    """{request index: prefix tokens the request re-wrote because its cache read fell short of the previous request's
    read + write}. A prompt-cache miss is not caused by the tool result before it; it gets its own category."""
    out = {}
    for k in range(1, len(reqs)):
        p, c = reqs[k - 1]["usage"], reqs[k]["usage"]
        lost = p.get("cache_read_input_tokens", 0) + p.get("cache_creation_input_tokens", 0) - c.get("cache_read_input_tokens", 0)
        if lost > 100:
            out[k] = min(lost, c.get("cache_creation_input_tokens", 0))
    return out


_breakdown = B.breakdown


def breakdown_without_misses(run, price):
    m = misses(run["requests"])
    if not m:
        return _breakdown(run, price)
    reqs = [dict(r, usage=dict(r["usage"], cache_creation_input_tokens=r["usage"].get("cache_creation_input_tokens", 0) - m.get(k, 0)))
            for k, r in enumerate(run["requests"])]
    return _breakdown(dict(run, requests=reqs), price)


B.breakdown = breakdown_without_misses  # MC.split_costs then books the re-write as OTHER; main() moves it to CACHE_MISS
CATS = CATS + ("CACHE_MISS",)


def rel(p, ws):
    return p.replace(ws + "/", "").replace(ws, ".") if isinstance(p, str) else p


def safe_args(name, inp, ws):
    a = {k: v for k, v in inp.items() if k not in ("continuation",)}
    if "continuation" in inp and inp["continuation"]:
        a["continuation"] = "<echoed>"
    return json.loads(rel(json.dumps(a), ws))


def bp_meta(text):
    try:
        j = json.loads(text)
    except ValueError:
        return {"parse": False}
    m = {"mode": j.get("mode"), "outcome": j.get("outcome"), "protocol": j.get("protocol_version")}
    pl = j.get("payload") or {}
    inner = next(iter(pl.values()), {}) if isinstance(pl, dict) and pl else {}
    if isinstance(inner, dict) and len(inner) == 1 and isinstance(next(iter(inner.values())), dict):
        inner = next(iter(inner.values()))
    s = json.dumps(pl)
    m["currentness"] = inner.get("currentness") or inner.get("structural_currentness") or ("Current" if '"Current"' in s else None)
    m["more_available"] = inner.get("more_available")
    m["economy"] = inner.get("economy")
    page = inner.get("page") or {}
    m["page_used_items"], m["page_used_bytes"] = page.get("used_items"), page.get("used_bytes")
    m["gaps"] = len(page.get("gaps") or []) if page else None
    m["unconfirmed_owners"] = sum(g.get("UnconfirmedCallerOwners", 0) for g in (page.get("gaps") or []) if isinstance(g, dict))
    m["error"] = j.get("outcome") not in ("ok", None) or '"NotFound"' in s[:400]
    m["matches"] = len(inner.get("matches") or []) if "matches" in inner else None
    m["text_lines"] = strings(pl, [])
    return m


def calls_of(name, prefix, ws, session_row):
    s = MC.load_session(prefix)
    reqs, task_ids = s["requests"], [t["task"] for t in s["turns"]]
    rows, bp_lines, bp_hist = [], set(), []  # bp_hist: (seq, meta) of every Brainprint answer so far in the session
    seq = 0
    for t in range(len(s["turn_results"])):
        task = task_ids[t] if t < len(task_ids) else "?"
        seen_bp_in_turn = False
        for j in [j for j in range(len(reqs)) if s["req_turn"][j] == t]:
            for cid in reqs[j]["calls"]:
                c = s["calls"][cid]
                k = B.kind(c["name"], c["input"])
                seq += 1
                row = {"session": name, "turn": t + 1, "task": task, "seq": seq, "req": j, "tool": c["name"], "kind": k,
                       "args": safe_args(c["name"], c["input"], ws), "result_bytes": c["bytes"], "is_error": c["is_error"],
                       "latency_s": round(c["latency_s"], 3) if c["latency_s"] else None}
                if k.startswith("BP:"):
                    m = bp_meta(c["text"])
                    used_cont = bool(c["input"].get("continuation"))
                    for x in m.pop("text_lines", []):
                        bp_lines |= lines_set(x)
                    row.update({"class": "BP", "bp": m, "continuation_used": used_cont})
                    bp_hist.append(m)
                    seen_bp_in_turn = True
                elif k == "ToolSearch":
                    row.update({"class": "SETUP"})
                elif k == "Skill":
                    row.update({"class": "SKILL"})
                else:
                    nl = lines_set(c["text"])
                    ov = round(len(nl & bp_lines) / len(nl), 3) if nl else None
                    cmd = c["input"].get("command", "")
                    row.update({"class": "NATIVE", "phase": ("after-BP-in-turn" if seen_bp_in_turn else ("after-BP-in-session" if bp_hist else "no-BP-yet")),
                                "content_overlap": ov, "result_lines": len(nl),
                                "post_process": bool(k.startswith("Bash") and re.search(r"\|\s*(sort|uniq|wc|awk|cut|head|tail)\b", cmd))})
                    if bp_hist:
                        last = bp_hist[-1]
                        if ov is not None and ov >= 0.8 and last.get("currentness") == "Current" and not last.get("more_available") and not last.get("gaps"):
                            ac = "REDUNDANT_CANDIDATE"
                        elif any(h.get("more_available") for h in bp_hist[-3:]):
                            ac = "FALLBACK_BOUNDED"
                        elif any(h.get("unconfirmed_owners") or h.get("gaps") for h in bp_hist[-3:]):
                            ac = "FALLBACK_PARTIAL"
                        elif last.get("error"):
                            ac = "FALLBACK_ERROR"
                        else:
                            ac = "NOT_DELIVERED"
                        row["auto_class"] = ac
                rows.append(row)
    return rows


def mean(v):
    return round(st.mean(v), 4) if v else None


def main():
    d, outp = sys.argv[1], sys.argv[2]
    names = sorted(f[: -len(".stream.jsonl")] for f in os.listdir(d) if f.endswith(".stream.jsonl") and os.path.exists(os.path.join(d, f.split(".")[0] + ".turns.json")) and re.match(r"^[AB]-O[12]-r\d$", f.split(".")[0]))
    ws_root = os.path.join(os.path.dirname(os.path.abspath(d)), "wtf")
    ses, allrows = [], []
    for n in names:
        x = MC.analyze(n, os.path.join(d, n))
        rows = calls_of(n, os.path.join(d, n), os.path.join(ws_root, n), x)
        allrows += rows
        s = MC.load_session(os.path.join(d, n))
        miss = misses(s["requests"])
        x["cache_miss_requests"] = len(miss)
        for t in x["turns"]:
            mu = sum(v for k, v in miss.items() if s["req_turn"][k] == t["pos"] - 1) * MC.PRICE["cache_creation_input_tokens"]
            t["cost_split_usd"]["OTHER"] = round(t["cost_split_usd"]["OTHER"] - mu, 5)
            t["cost_split_usd"]["CACHE_MISS"] = round(mu, 5)
            t["cache_miss_requests"] = sum(1 for k in miss if s["req_turn"][k] == t["pos"] - 1)
            tr = [r for r in rows if r["turn"] == t["pos"]]
            nat = [r for r in tr if r["class"] == "NATIVE"]
            t["native_before_bp"] = sum(1 for r in nat if r["phase"] == "no-BP-yet")
            t["native_after_bp"] = sum(1 for r in nat if r["phase"] != "no-BP-yet")
            t["auto_class"] = {c: sum(1 for r in nat if r.get("auto_class") == c) for c in {r.get("auto_class") for r in nat if r.get("auto_class")}}
            t["post_process_bash"] = sum(1 for r in nat if r["post_process"])
            t["bp_economy"] = [r["bp"].get("economy") for r in tr if r["class"] == "BP"]
        x["degraded"] = False  # MC's T1 degraded detector reads the old change-page shape; degradation is read per call (unconfirmed_owners)
        x["unconfirmed_owner_answers"] = sum(1 for r in rows if r["class"] == "BP" and r["bp"].get("unconfirmed_owners"))
        ses.append(x)
    with open(outp + "_calls.jsonl", "w") as f:
        for r in allrows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    pool = [x for x in ses if x["complete"]]
    agg = {}
    for cond in ("A", "B"):
        ss = [x for x in pool if x["cond"] == cond]
        if not ss:
            continue
        T = [t for x in ss for t in x["turns"]]
        rows = [r for r in allrows if r["session"] in {x["session"] for x in ss}]
        nat = [r for r in rows if r["class"] == "NATIVE"]
        agg[cond] = {
            "sessions": [x["session"] for x in ss], "cache_classes": [x["cache_class"] for x in ss],
            "session_usd_mean": mean([x["total_usd"] for x in ss]), "session_usd_min": min(x["total_usd"] for x in ss), "session_usd_max": max(x["total_usd"] for x in ss),
            "session_usd": [x["total_usd"] for x in ss],
            "per_task_usd_mean": {tk: mean([t["cost_usd"] for t in T if t["task"] == tk]) for tk in ("T1", "T5", "T6", "T7")},
            "split_usd_session_mean": {k: mean([sum(t["cost_split_usd"][k] for t in x["turns"]) for x in ss]) for k in CATS},
            "tokens_session_mean": {k: mean([sum(t["tokens"][k] for t in x["turns"]) for x in ss]) for k in ("input", "output", "cache_write", "cache_read")},
            "api_requests_session_mean": mean([sum(t["api_requests"] for t in x["turns"]) for x in ss]),
            "grades": f"{sum(1 for t in T if t['grade'] and t['grade']['pass'])}/{len(T)}",
            "failed": [(x["session"], t["task"]) for x in ss for t in x["turns"] if not (t["grade"] and t["grade"]["pass"])],
            "bp_calls_session_mean": mean([sum(t["bp_calls"] for t in x["turns"]) for x in ss]),
            "bp_bytes_session_mean": mean([sum(t["bp_bytes"] for t in x["turns"]) for x in ss]),
            "native_calls_session_mean": mean([sum(t["native_calls"] for t in x["turns"]) for x in ss]),
            "native_bytes_session_mean": mean([sum(t["native_bytes"] for t in x["turns"]) for x in ss]),
            "native_by_tool_total": {k: sum(1 for r in nat if r["kind"] == k) for k in sorted({r["kind"] for r in nat})},
            "native_before_bp_total": sum(1 for r in nat if r["phase"] == "no-BP-yet"),
            "native_after_bp_total": sum(1 for r in nat if r["phase"] != "no-BP-yet"),
            "auto_class_total": {c: sum(1 for r in nat if r.get("auto_class") == c) for c in sorted({r.get("auto_class") for r in nat if r.get("auto_class")})},
            "post_process_bash_total": sum(1 for r in nat if r["post_process"]),
            "toolsearch_calls_total": sum(x["toolsearch_calls"] for x in ss), "toolsearch_tokens_session_mean": mean([x["toolsearch_tokens"] for x in ss]),
            "wall_s_session_mean": mean([x["wall_s_total"] for x in ss]),
            "peak_rss_mib": {k: [round(x["peak_rss_kib"].get(k, 0) / 1024) for x in ss] for k in ("brainprintd", "brainprint-mcp", "rust-analyzer", "brainprint_total", "claude")},
            "hook_events_total": sum(x["hook_events"] for x in ss), "hook_latency_us_p50": [x["hook_latency_us_p50"] for x in ss],
        }
    pairs = []
    for b in pool:
        if b["cond"] == "B":
            a = next((y for y in pool if y["cond"] == "A" and y["order"] == b["order"] and y["round"] == b["round"]), None)
            if a:
                pairs.append({"pair": f"{b['order']}-{b['round']}", "A": a["total_usd"], "B": b["total_usd"], "B_minus_A": round(b["total_usd"] - a["total_usd"], 4)})
    out = {"base": "6775dac", "client": "Claude Code 2.1.289", "model": "claude-opus-5-5", "effort": "medium",
           "price_per_million_usd": {k: round(v * 1e6, 2) for k, v in MC.PRICE.items()},
           "sessions": ses, "pooled": agg, "pairs": pairs}
    json.dump(out, open(outp + ".json", "w"), indent=1, ensure_ascii=False, default=lambda o: sorted(o) if isinstance(o, set) else str(o))
    for c, a in agg.items():
        print(c, json.dumps({k: v for k, v in a.items() if k not in ("peak_rss_mib",)}, ensure_ascii=False))
    print("pairs", json.dumps(pairs))


if __name__ == "__main__":
    main()
