"""I5 Task 14 multi-task session experiment: per-session, per-turn and per-condition comparison.

usage: multi_compare.py <out-dir> [--skip smoke]   (session ids look like B1-O2-r1, files <id>.stream.jsonl ...)
Writes multi_compare.json and prints markdown-ready tables.

Everything is measured from the stream of ONE Claude Code process per session:
- turn tokens = that turn's `result` usage (per turn); turn cost = the difference of `total_cost_usd`, which is cumulative over the
  session (checked against the turn's tokens x prices: `price_residual`);
- per-turn cost split (incurred in that turn): every request's input / cache-write / cache-read tokens are
  attributed to BASE (static prefix + first request), SETUP (ToolSearch results = tool-schema load),
  BP_RESULT, NATIVE_RESULT or OTHER (user prompt, assistant text, thinking); a tool result is priced as the next
  request's cache-write and then as a cache-read in every later request of the SESSION (also in later turns);
  the split sums to the turn's total_cost_usd (checked); OUTPUT is the turn's output tokens;
- a session is COMPARABLE only when its first request reads just the shared static prefix and no later request
  reads cache it did not write itself (cache class STATIC), and a B session was not DEGRADED (T1 unconfirmed owners).
"""

import json
import os
import re
import statistics as st
import sys
from collections import defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import breakdown as B  # noqa: E402
import instr_compare as IC  # noqa: E402

PRICE = {"input_tokens": 4e-6, "cache_creation_input_tokens": 8e-6, "cache_read_input_tokens": 0.2e-6, "output_tokens": 20e-6}
CATS = ("BASE", "SETUP", "BP_RESULT", "NATIVE_RESULT", "OTHER", "OUTPUT")


def cites(text):
    """{basename.rs: {line numbers cited after it on the same text line, ranges expanded}}.
    A line without any `x.rs` mention attributes its `:NNN` numbers to the last file mentioned before it
    (answers often say "all in config.rs" once and then list bare `:368`, `:405`)."""
    out, last = {}, None
    rng = lambda lo, hi, f: out.setdefault(f, set()).update(range(lo, hi + 1)) if 0 <= hi - lo <= 20 else None
    for ln in text.splitlines():
        ms = list(re.finditer(r"([\w\-]+\.rs)", ln))
        if not ms:
            if last:
                for x in re.finditer(r"(?<![\w.])[:`]?(\d+)(?:\s*[-\u2013\u2014]\s*(\d+))?", ln):
                    rng(int(x.group(1)), int(x.group(2) or x.group(1)), last)
            continue
        for a, m in enumerate(ms):
            seg = ln[m.end(): ms[a + 1].start() if a + 1 < len(ms) else len(ln)]
            for x in re.finditer(r"(\d+)(?:\s*[-\u2013\u2014]\s*(\d+))?", seg):
                rng(int(x.group(1)), int(x.group(2) or x.group(1)), m.group(1))
        last = ms[-1].group(1)
    return out


def has(c, f, *lines, any_of=False):
    got = [n in c.get(f, ()) for n in lines]
    return any(got) if any_of else all(got)


def grade(task, final):
    if task in ("T1", "T2"):
        return IC.grade(task, final)
    c = cites(final)
    if task == "T3":
        g = {"signature": bool(re.search(r"fn now_unix_ms\(\)\s*->\s*u64", final)), "def": has(c, "util.rs", 60),
             "sites": has(c, "state.rs", 256) and has(c, "telemetry.rs", 57), "false_calls": []}
    elif task == "T4":
        false = [f"{f}:{n}" for f, ns in (("handlers.rs", (24,)), ("config.rs", (352, 387, 483))) for n in ns if has(c, f, n)]
        g = {"signature": "bootstrap_workspace_config(paths: &WorkspacePaths) -> Result<WorkspaceConfig, ConfigError>" in final,
             "def": has(c, "config.rs", 203),
             "sites": has(c, "init.rs", 404, 464) and has(c, "config.rs", 368, 405, 431, 519), "false_calls": false}
    elif task == "T5":
        variants = all(re.search(r"\b" + v + r"\b", final) for v in ("MissingParent", "Io", "Decode", "Encode", "UnsupportedFormat"))
        fns = all(re.search(r"\b" + v + r"\b", final) for v in ("bootstrap_global_config", "bootstrap_workspace_config",
                                                                  "load_global_config", "load_workspace_config"))
        g = {"signature": bool(re.search(r"CONFIG_FORMAT_VERSION[^\n]{0,60}\b1\b", final)), "def": has(c, "config.rs", 19),
             "sites": has(c, "config.rs", *range(298, 313), any_of=True) and "validate_format" in final and "UnsupportedFormat" in final
             and variants and fns, "false_calls": []}
    elif task == "T6":
        false = [f"gateway.rs:{n}" for n in (431, 486, 558, 599) if has(c, "gateway.rs", n)]
        g = {"signature": "impl IntoIterator<Item = &'a str>" in final and "-> String" in final
             and bool(re.search(r"(?i)fnv", final)) and "32" in final and bool(re.search(r"(?i)hex", final)),
             "def": has(c, "util.rs", 15),
             "sites": has(c, "delivery.rs", 204) and has(c, "probe.rs", 71) and has(c, "state.rs", 222) and has(c, "telemetry.rs", 60)
             and has(c, "util.rs", 80, 81, 82, any_of=True) and has(c, "i5_task13_adoption_acceptance.rs", 692), "false_calls": false}
    else:
        raise ValueError(task)
    # `false_calls` lists sites of look-alike functions that the answer cites anywhere (also when it explains why
    # they do not count); it is informational and reviewed by hand, `pass` is recall of every required fact.
    g["pass_recall"] = g["signature"] and g["def"] and g["sites"]
    g["pass"] = g["pass_recall"]
    return g


def load_session(prefix):
    times = [json.loads(l)["t"] for l in open(prefix + ".times.jsonl")]
    reqs, order, calls, results, turn_res, req_turn, use_t, res_t = [], {}, {}, {}, [], [], {}, {}
    for n, line in enumerate(open(prefix + ".stream.jsonl")):
        try:
            d = json.loads(line)
        except ValueError:
            continue
        t, ty = times[n] if n < len(times) else None, d.get("type")
        if ty == "assistant":
            m = d["message"]
            if m.get("id") not in order:
                order[m["id"]] = len(reqs)
                reqs.append({"id": m["id"], "usage": m.get("usage", {}), "blocks": [], "calls": []})
                req_turn.append(len(turn_res))
            r = reqs[order[m["id"]]]
            for c in m["content"]:
                if c["type"] == "tool_use":
                    calls[c["id"]] = {"name": c["name"], "input": c["input"], "req": order[m["id"]]}
                    use_t[c["id"]] = t
                    r["calls"].append(c["id"])
                r["blocks"].append(c["type"])
        elif ty == "user":
            for c in d.get("message", {}).get("content") or []:
                if isinstance(c, dict) and c.get("type") == "tool_result":
                    results[c["tool_use_id"]] = c
                    res_t[c["tool_use_id"]] = t
        elif ty == "result":
            turn_res.append(d)
    for cid, c in calls.items():
        r = results.get(cid, {})
        c["text"] = B.text_of(r.get("content"))
        c["bytes"] = len(c["text"].encode())
        c["is_error"] = bool(r.get("is_error"))
        c["latency_s"] = (res_t[cid] - use_t[cid]) if cid in res_t and use_t.get(cid) and res_t[cid] else None
    return {"requests": reqs, "calls": calls, "turn_results": turn_res, "req_turn": req_turn,
            "turns": json.load(open(prefix + ".turns.json")), "rss": json.load(open(prefix + ".rss.json"))}


def split_costs(s, bd):
    """per-turn incurred cost by category (see module docstring)"""
    reqs, cw, cr, pin, po = s["requests"], PRICE["cache_creation_input_tokens"], PRICE["cache_read_input_tokens"], PRICE["input_tokens"], PRICE["output_tokens"]
    by_req = defaultdict(list)
    for c in bd:
        by_req[c["req"]].append(c)
    cat_of = lambda k: "SETUP" if k == "ToolSearch" else "BP_RESULT" if k.startswith("BP:") else "NATIVE_RESULT"
    u0 = reqs[0]["usage"]
    static, base_cw = u0.get("cache_read_input_tokens", 0), u0.get("cache_creation_input_tokens", 0)
    out = [defaultdict(float) for _ in s["turn_results"]]
    for j, r in enumerate(reqs):
        t = s["req_turn"][j]
        if t >= len(out):
            continue
        u = r["usage"]
        out[t]["BASE"] += u.get("input_tokens", 0) * pin
        w = u.get("cache_creation_input_tokens", 0)
        if j == 0:
            out[t]["BASE"] += w * cw
        else:
            prev = by_req.get(j - 1, [])
            used = 0.0
            for c in prev:
                out[t][cat_of(c["kind"])] += c["added_tokens"] * cw
                used += c["added_tokens"]
            out[t]["OTHER"] += max(w - used, 0) * cw
        rd = u.get("cache_read_input_tokens", 0)
        left = rd
        st_tok = min(static, left)
        out[t]["BASE"] += st_tok * cr
        left -= st_tok
        b_tok = min(base_cw, left) if j > 0 else 0
        out[t]["BASE"] += b_tok * cr
        left -= b_tok
        for m in range(j - 1):  # segments written by requests <= j-1 come from calls of requests <= j-2
            for c in by_req.get(m, []):
                tok = min(c["added_tokens"], left)
                out[t][cat_of(c["kind"])] += tok * cr
                left -= tok
        out[t]["OTHER"] += left * cr
    for t, res in enumerate(s["turn_results"]):
        out[t]["OUTPUT"] += res["usage"].get("output_tokens", 0) * po
    return out


def t1_sites(calls_of_turn):
    uniq, unconfirmed, seen = set(), 0, False
    for c in calls_of_turn:
        if c["name"].endswith("context") and c["input"].get("mode") == "change" and not c["is_error"]:
            try:
                pg = json.loads(c["text"])["payload"]["Context"]["page"]
            except (ValueError, KeyError, TypeError):
                continue
            seen = True
            for e in pg["evidence"]:
                if "Full" in e and "CurrentSource" in e["Full"] and e["Full"]["CurrentSource"]["role"] == "EvidenceSpan":
                    cs = e["Full"]["CurrentSource"]
                    uniq.add((cs["path_rel"], cs["span"]["start"]["line"] + 1))
            unconfirmed += sum(g["UnconfirmedCallerOwners"] for g in pg.get("gaps", []) if isinstance(g, dict) and "UnconfirmedCallerOwners" in g)
    return (len(uniq), unconfirmed) if seen else (None, None)


def analyze(name, prefix):
    s = load_session(prefix)
    reqs = s["requests"]
    task_ids = [t["task"] for t in s["turns"]]
    fake = {"requests": reqs, "calls": s["calls"], "result": {}}
    bd = B.breakdown(fake, PRICE)
    for c in bd:
        c["turn"] = s["req_turn"][c["req"]]
    split = split_costs(s, bd)
    ext = 0
    for k in range(1, len(reqs)):
        p, c = reqs[k - 1]["usage"], reqs[k]["usage"]
        if c.get("cache_read_input_tokens", 0) > p.get("cache_read_input_tokens", 0) + p.get("cache_creation_input_tokens", 0) + 100:
            ext += 1
    u0 = reqs[0]["usage"]
    cond = name.split("-")[0]
    bp_seen = ""
    read_paths = defaultdict(int)
    turns = []
    for t, res in enumerate(s["turn_results"]):
        task = task_ids[t] if t < len(task_ids) else "?"
        rids = [j for j in range(len(reqs)) if s["req_turn"][j] == t]
        cids = [cid for j in rids for cid in reqs[j]["calls"]]
        cl = [s["calls"][cid] for cid in cids]
        kinds = [B.kind(c["name"], c["input"]) for c in cl]
        u = res.get("usage", {})
        tot = res["total_cost_usd"] - (s["turn_results"][t - 1]["total_cost_usd"] if t else 0.0)  # total_cost_usd is cumulative, usage is per turn
        resid = abs(sum(u.get(k, 0) * PRICE[k] for k in PRICE) - tot) / tot
        cost_split = {k: round(split[t].get(k, 0.0), 5) for k in CATS}
        native = [(c, k) for c, k in zip(cl, kinds) if not k.startswith("BP:") and k != "ToolSearch"]
        bp = [(c, k) for c, k in zip(cl, kinds) if k.startswith("BP:")]
        ts = [c for c in bd if c["turn"] == t and c["kind"] == "ToolSearch"]
        read_after_bp = 0
        for c, k in zip(cl, kinds):
            if k.startswith("BP:"):
                bp_seen += c["text"]
            elif k == "Read":
                p = c["input"].get("file_path", "")
                rel = p.split("/wtm/")[-1].split("/", 1)[-1]
                read_paths[rel] += 1
                if rel and rel in bp_seen:
                    read_after_bp += 1
        lat = sorted(c["latency_s"] for c, _ in bp if c["latency_s"] is not None)
        final = res.get("result", "")
        sites = unconf = None
        if task == "T1" and cond != "A":
            sites, unconf = t1_sites(cl)
        turns.append({
            "task": task, "pos": t + 1, "cost_usd": tot, "cost_split_usd": cost_split, "split_check_usd": round(sum(cost_split.values()) - tot, 6),
            "price_residual": round(resid, 6),
            "tokens": {"input": u.get("input_tokens", 0), "output": u.get("output_tokens", 0),
                       "cache_write": u.get("cache_creation_input_tokens", 0), "cache_read": u.get("cache_read_input_tokens", 0)},
            "usd_by_token_type": {"input": round(u.get("input_tokens", 0) * PRICE["input_tokens"], 5),
                                  "output": round(u.get("output_tokens", 0) * PRICE["output_tokens"], 5),
                                  "cache_write": round(u.get("cache_creation_input_tokens", 0) * PRICE["cache_creation_input_tokens"], 5),
                                  "cache_read": round(u.get("cache_read_input_tokens", 0) * PRICE["cache_read_input_tokens"], 5)},
            "api_requests": len(rids), "req0_cache_write": reqs[rids[0]]["usage"].get("cache_creation_input_tokens", 0),
            "req0_cache_read": reqs[rids[0]]["usage"].get("cache_read_input_tokens", 0),
            "toolsearch_calls": len(ts), "toolsearch_tokens": sum(c["added_tokens"] for c in ts),
            "toolsearch_queries": [c["input"].get("query", "") for c in cl if c["name"] == "ToolSearch"],
            "bp_calls": len(bp), "bp_kinds": [k for _, k in bp], "bp_bytes": sum(c["bytes"] for c, _ in bp),
            "bp_latency_s": {"n": len(lat), "p50": round(lat[len(lat) // 2], 2) if lat else None, "max": round(lat[-1], 2) if lat else None},
            "native_calls": len(native), "native_bytes": sum(c["bytes"] for c, _ in native),
            "native_by_tool": {k: sum(1 for _, kk in native if kk == k) for k in {kk for _, kk in native}},
            "native_read_calls": sum(1 for _, k in native if k == "Read"), "native_read_after_bp": read_after_bp,
            "wall_s": s["turns"][t]["wall_s"] if t < len(s["turns"]) else None, "num_turns": res.get("num_turns"),
            "t1_sites_from_change_pages": sites, "t1_unconfirmed_caller_owners": unconf,
            "grade": grade(task, final) if task != "?" else None,
        })
    dup_reads = sum(n - 1 for n in read_paths.values() if n > 1)
    tel = []
    tp = prefix + ".telemetry.jsonl"
    if os.path.exists(tp):
        tel = [json.loads(l) for l in open(tp) if l.strip()]
    degraded = any((t["t1_unconfirmed_caller_owners"] or 0) > 0 for t in turns)
    cls = "STATIC" if (9000 <= u0.get("cache_read_input_tokens", 0) <= 13000 and ext == 0) else "OTHER"
    peak = s["rss"]["peak_rss_kib"]
    return {"session": name, "cond": cond, "order": name.split("-")[1], "round": name.split("-")[2], "cache_class": cls,
            "req0_cache_write": u0.get("cache_creation_input_tokens", 0), "req0_cache_read": u0.get("cache_read_input_tokens", 0),
            "extended_cache_hits": ext, "degraded": degraded, "complete": len(turns) == len(task_ids) and len(turns) > 0,
            "total_usd": round(sum(t["cost_usd"] for t in turns), 5), "turns": turns,
            "toolsearch_calls": sum(t["toolsearch_calls"] for t in turns), "toolsearch_tokens": sum(t["toolsearch_tokens"] for t in turns),
            "schema_load_turns": [t["pos"] for t in turns if t["toolsearch_calls"]],
            "duplicate_file_reads": dup_reads, "peak_rss_kib": peak, "end_of_turn_rss_kib": s["rss"]["end_of_turn_rss_kib"],
            "hook_events": len(tel), "hook_latency_us_p50": sorted(e["latency_us"] for e in tel)[len(tel) // 2] if tel else None,
            "hook_latency_us_max": max((e["latency_us"] for e in tel), default=None),
            "wall_s_total": round(sum(t["wall_s"] or 0 for t in turns), 1)}


def mean(v):
    return st.mean(v) if v else None


def main():
    d = sys.argv[1]
    skip = sys.argv[sys.argv.index("--skip") + 1] if "--skip" in sys.argv else None
    names = sorted(f.split(".")[0] for f in os.listdir(d) if f.endswith(".stream.jsonl") and os.path.exists(os.path.join(d, f.split(".")[0] + ".rss.json")) and not (skip and f.startswith(skip)))
    ses = [analyze(n, os.path.join(d, n)) for n in names]
    pool = [x for x in ses if x["cache_class"] == "STATIC" and not x["degraded"] and x["complete"]]
    out = {"price_per_million_usd": {k: round(v * 1e6, 2) for k, v in PRICE.items()}, "sessions": ses,
           "pooled_sessions": [x["session"] for x in pool], "not_pooled": [x["session"] for x in ses if x not in pool]}
    print("sessions:", len(ses), " pooled:", len(pool), " not pooled:", out["not_pooled"])
    print(f"{'session':10} {'cls':6} {'total':>7} | " + " ".join(f"c{k}" .rjust(6) for k in range(1, 7)) + " | grades  TS  peakRSS(MiB: total/daemon)")
    for x in ses:
        cs = " ".join(f"{t['cost_usd']:.3f}".rjust(6) for t in x["turns"])
        gr = "".join("P" if (t["grade"] and t["grade"]["pass"]) else "F" for t in x["turns"])
        p = x["peak_rss_kib"]
        print(f"{x['session']:10} {x['cache_class']:6} {x['total_usd']:7.3f} | {cs} | {gr:6} {x['toolsearch_calls']:>2} {p.get('brainprint_total', 0) // 1024}/{p.get('brainprintd', 0) // 1024} {'DEGRADED' if x['degraded'] else ''}")
    # position-wise pooled cost, cumulative and per-task average
    agg = {}
    for cond in ("A", "B1"):
        ss = [x for x in pool if x["cond"] == cond]
        if not ss:
            continue
        n = min(len(x["turns"]) for x in ss)
        per = [[x["turns"][k]["cost_usd"] for x in ss] for k in range(n)]
        cum = [[sum(x["turns"][i]["cost_usd"] for i in range(k + 1)) for x in ss] for k in range(n)]
        agg[cond] = {"sessions": [x["session"] for x in ss], "n": len(ss),
                     "turn_cost_mean": [round(mean(v), 4) for v in per], "turn_cost_sd": [round(st.stdev(v), 4) if len(v) > 1 else 0 for v in per],
                     "cum_mean": [round(mean(v), 4) for v in cum], "cum_min": [round(min(v), 4) for v in cum], "cum_max": [round(max(v), 4) for v in cum],
                     "avg_per_task_mean": [round(mean(v) / (k + 1), 4) for k, v in enumerate(cum)],
                     "first_turn_mean": round(mean(per[0]), 4), "followup_turn_mean": round(mean([c for v in per[1:] for c in v]), 4) if n > 1 else None}
        comp = defaultdict(lambda: defaultdict(list))
        for x in ss:
            for t in x["turns"]:
                pos = "first" if t["pos"] == 1 else "followup"
                for k in CATS:
                    comp[pos][k].append(t["cost_split_usd"][k])
                for k, v in t["usd_by_token_type"].items():
                    comp[pos]["tok_" + k].append(v)
        agg[cond]["split_mean_per_turn"] = {pos: {k: round(mean(v), 4) for k, v in dd.items()} for pos, dd in comp.items()}
    out["pooled"] = agg
    # paired B1 - A: same order, same round index -> same position in the same task order
    pairs = []
    for x in pool:
        if x["cond"] == "B1":
            a = next((y for y in pool if y["cond"] == "A" and y["order"] == x["order"] and y["round"] == x["round"]), None)
            if a:
                pairs.append((a, x))
    out["paired_b1_minus_a_cum"] = [{"order": b["order"], "round": b["round"],
                                     "cum_diff": [round(sum(t["cost_usd"] for t in b["turns"][:k + 1]) - sum(t["cost_usd"] for t in a["turns"][:k + 1]), 4)
                                                  for k in range(min(len(a["turns"]), len(b["turns"])))]} for a, b in pairs]
    if "A" in agg and "B1" in agg:
        n = min(len(agg["A"]["cum_mean"]), len(agg["B1"]["cum_mean"]))
        diff = [round(agg["B1"]["cum_mean"][k] - agg["A"]["cum_mean"][k], 4) for k in range(n)]
        out["cum_mean_diff_b1_minus_a"] = diff
        out["first_k_with_b1_cum_below_a"] = next((k + 1 for k, v in enumerate(diff) if v < 0), None)
        out["first_k_with_all_pairs_b1_below_a"] = next((k + 1 for k in range(n) if pairs and all(p["cum_diff"][k] < 0 for p in out["paired_b1_minus_a_cum"])), None)
        out["first_k_with_b1_cum_range_below_a_range"] = next((k + 1 for k in range(n) if agg["B1"]["cum_max"][k] < agg["A"]["cum_min"][k]), None)
        print("\nposition:            " + " ".join(f"{k + 1:>7}" for k in range(n)))
        for c in ("A", "B1"):
            print(f"{c:3} turn cost mean  " + " ".join(f"{v:7.3f}" for v in agg[c]["turn_cost_mean"][:n]))
            print(f"{c:3} cum mean        " + " ".join(f"{v:7.3f}" for v in agg[c]["cum_mean"][:n]))
            print(f"{c:3} cum min..max    " + " ".join(f"{lo:.2f}-{hi:.2f}"[:7].rjust(7) for lo, hi in zip(agg[c]["cum_min"][:n], agg[c]["cum_max"][:n])))
            print(f"{c:3} avg per task    " + " ".join(f"{v:7.3f}" for v in agg[c]["avg_per_task_mean"][:n]))
        print("B1-A cum mean       " + " ".join(f"{v:+7.3f}" for v in diff))
        for p in out["paired_b1_minus_a_cum"]:
            print(f"pair {p['order']}-{p['round']} B1-A  " + " ".join(f"{v:+7.3f}" for v in p["cum_diff"]))
        print("first k with mean cum B1 < A:", out["first_k_with_b1_cum_below_a"], "| all pairs:", out["first_k_with_all_pairs_b1_below_a"],
              "| ranges separated:", out["first_k_with_b1_cum_range_below_a_range"])
    json.dump(out, open("multi_compare.json", "w"), indent=1, ensure_ascii=False, default=lambda o: sorted(o) if isinstance(o, set) else str(o))


if __name__ == "__main__":
    main()
