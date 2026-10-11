"""I5 Task 14 instruction-only experiment: per-run and per-condition comparison.

usage: instr_compare.py <out-dir> [<out-dir> ...]  (run ids look like T1-B1-r3)
Reads the stream-json runs with breakdown.py, prices them with the prices fitted from the
runs' own totals, and writes instr_compare.json + a markdown summary on stdout.

Per run it records what a fair comparison needs: the prompt-cache state of the first request
(cache-write / cache-read), whether any later request read cache it did not write itself in this
run ("extended hit"), the ToolSearch load in tokens, and the cost split
(BASE / SETUP = tool-schema load / BP_RESULT / NATIVE_RESULT / OTHER / OUTPUT).
A run is COMPARABLE only when its cache class is STATIC (first request reads just the shared
static prefix, no extended hit); other runs are listed but never pooled with STATIC ones.
"""

import json
import os
import re
import statistics as st
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import breakdown as B  # noqa: E402

EXPECTED_T1 = {446, 462, 472, 525, 538, 550, 543, 115}
NON_CALL_REFS = {24, 29, 213, 214, 215, 315, 314, 1365, 1396, 1444, 23, 108}
T2_FIELDS = ("timestamp_unix_ms client client_version session reset_source mode event native_attempt route "
             "exact_substitute_proven decision fallback_reason suggested latency_us daemon_probe_count "
             "daemon_probe_us state_bytes bootstrap_bytes message_bytes").split()


def grade(task, final):
    if task == "T1":
        section = final
        m = re.search(r"(?is)direct call sites(.*?)(\*\*3|\n#+ *3|tests that would)", final)
        if m:
            section = m.group(1)
        nums = {int(x) for x in re.findall(r"(?<![\d.])(\d{2,4})(?![\d.])", section)}
        found = EXPECTED_T1 & nums
        extra = sorted(n for n in nums - EXPECTED_T1 - NON_CALL_REFS if 100 <= n <= 700)
        return {"sites_found": len(found), "sites_expected": 8, "missing": sorted(EXPECTED_T1 - found),
                "unexplained_line_numbers_in_call_site_section": extra,
                "signature": "load_workspace_config(paths: &WorkspacePaths) -> Result<WorkspaceConfig, ConfigError>" in final,
                "pass": len(found) == 8}
    fields = sum(1 for k in T2_FIELDS if re.search(r"\b" + k + r"\b", final))
    g = {"env_var": "BRAINPRINT_ADOPTION_TELEMETRY_PATH" in final, "append_fn": "append" in final,
         "jsonl": bool(re.search(r"(?i)jsonl|json line|one json", final)), "fields_found": fields,
         "emitters_event_bridge": ("bridge" in final) and bool(re.search(r"\bevent\b", final))}
    g["pass"] = g["env_var"] and g["append_fn"] and g["jsonl"] and fields == 19 and g["emitters_event_bridge"]
    return g


def one(name, run, price):
    reqs = run["requests"]
    bd = B.breakdown(run, price)
    cat = B.categories(run, price, bd)
    u0 = reqs[0]["usage"]
    ext = 0
    for k in range(1, len(reqs)):
        p, c = reqs[k - 1]["usage"], reqs[k]["usage"]
        if c.get("cache_read_input_tokens", 0) > p.get("cache_read_input_tokens", 0) + p.get("cache_creation_input_tokens", 0) + 100:
            ext += 1
    ts = [c for c in bd if c["kind"] == "ToolSearch"]
    cls = "STATIC" if (9000 <= u0.get("cache_read_input_tokens", 0) <= 13000 and ext == 0) else "OTHER"
    native = [c for c in bd if not c["kind"].startswith("BP:") and c["kind"] != "ToolSearch"]
    bp = [c for c in bd if c["kind"].startswith("BP:")]
    task, cond = name.split("-")[0], name.split("-")[1]
    # T1 only: how many call sites the `context change` pages really returned, and whether it said
    # (UnconfirmedCallerOwners) that it could not confirm the callers. A B run whose Brainprint had no
    # working semantic backend is DEGRADED and is never pooled with a working one.
    sites, unconfirmed = None, None
    if task == "T1" and cond != "A":
        sites, unconfirmed = 0, 0
        seen_change = False
        uniq = set()
        for cid, c in run["calls"].items():  # every page of the change context (continuation pages included)
            if c["name"].endswith("context") and c["input"].get("mode") == "change" and not c["is_error"]:
                try:
                    pg = json.loads(c["text"])["payload"]["Context"]["page"]
                except (ValueError, KeyError, TypeError):
                    continue
                seen_change = True
                for e in pg["evidence"]:
                    if "Full" in e and "CurrentSource" in e["Full"] and e["Full"]["CurrentSource"]["role"] == "EvidenceSpan":
                        cs = e["Full"]["CurrentSource"]
                        uniq.add((cs["path_rel"], cs["span"]["start"]["line"] + 1))
                unconfirmed += sum(g["UnconfirmedCallerOwners"] for g in pg.get("gaps", [])
                                   if isinstance(g, dict) and "UnconfirmedCallerOwners" in g)
        sites = len(uniq)  # distinct (path, line) call-site spans delivered by Brainprint over all pages
        if not seen_change:
            sites = unconfirmed = None
    sel = [run["calls"][cid]["input"].get("query", "") for r in reqs for cid in r["calls"]
           if run["calls"][cid]["name"] == "ToolSearch"]
    return {
        "run": name, "task": task, "cond": cond, "total_usd": run["result"]["total_cost_usd"],
        "categories_usd": {k: round(v, 4) for k, v in cat.items()},
        "req0_cache_write": u0.get("cache_creation_input_tokens", 0), "req0_cache_read": u0.get("cache_read_input_tokens", 0),
        "extended_cache_hits": ext, "cache_class": cls,
        "toolsearch_calls": len(ts), "toolsearch_tokens": sum(c["added_tokens"] for c in ts), "toolsearch_queries": sel,
        "context_change_sites_all_pages": sites, "unconfirmed_caller_owners": unconfirmed,
        "degraded": bool(task == "T1" and cond != "A" and bool(unconfirmed)),
        "cache_write_total": run["result"]["usage"].get("cache_creation_input_tokens", 0),
        "cache_read_total": run["result"]["usage"].get("cache_read_input_tokens", 0),
        "output_tokens": run["result"]["usage"].get("output_tokens", 0),
        "api_requests": len(reqs), "bp_calls": len(bp), "bp_bytes": sum(c["bytes"] for c in bp),
        "bp_kinds": [c["kind"] for c in bp],
        "native_calls": len(native), "native_bytes": sum(c["bytes"] for c in native),
        "wall_s": round(run["result"].get("duration_ms", 0) / 1000, 1),
        "grade": grade(task, run["result"].get("result", "")),
    }


def stats(vals):
    return {"n": len(vals), "mean": round(st.mean(vals), 4), "min": round(min(vals), 4), "max": round(max(vals), 4),
            "sd": round(st.stdev(vals), 4) if len(vals) > 1 else 0.0}


def main():
    runs = {}
    for d in sys.argv[1:]:
        for f in sorted(os.listdir(d)):
            if f.endswith(".stream.jsonl"):
                runs[f.split(".")[0]] = B.load(os.path.join(d, f))
    # prices fitted earlier from 20 runs (exact, 0.00% residual): a fit over a handful of runs is ill-conditioned
    price = {"input_tokens": 4e-6, "cache_creation_input_tokens": 8e-6, "cache_read_input_tokens": 0.2e-6, "output_tokens": 20e-6}
    resid = max(abs(sum(r["result"]["usage"].get(k, 0) * price[k] for k in price) - r["result"]["total_cost_usd"])
                / r["result"]["total_cost_usd"] for r in runs.values())
    print("max price residual over these runs: %.2f%%" % (resid * 100))
    rows = [one(n, r, price) for n, r in runs.items()]
    out = {"price_per_million_usd": {k: round(v * 1e6, 2) for k, v in price.items()}, "runs": rows, "conditions": {}}
    for task in ("T1", "T2"):
        for cond in ("A", "B0", "B1"):
            sel = [r for r in rows if r["task"] == task and r["cond"] == cond]
            if not sel:
                continue
            comp = [r for r in sel if r["cache_class"] == "STATIC" and not r["degraded"]]
            d = {"runs": len(sel), "comparable_runs": len(comp), "total_all": stats([r["total_usd"] for r in sel]),
                 "grade_pass": sum(r["grade"]["pass"] for r in sel)}
            if comp:
                d["total_comparable"] = stats([r["total_usd"] for r in comp])
                for k in ("BASE", "SETUP", "BP_RESULT", "NATIVE_RESULT", "OTHER", "OUTPUT"):
                    d[k] = round(st.mean(r["categories_usd"].get(k, 0) for r in comp), 4)
                d["toolsearch_tokens"] = round(st.mean(r["toolsearch_tokens"] for r in comp))
                d["cache_write_mean"] = round(st.mean(r["cache_write_total"] for r in comp))
                d["cache_read_mean"] = round(st.mean(r["cache_read_total"] for r in comp))
                d["bp_bytes_mean"] = round(st.mean(r["bp_bytes"] for r in comp))
                d["native_bytes_mean"] = round(st.mean(r["native_bytes"] for r in comp))
                d["wall_s_mean"] = round(st.mean(r["wall_s"] for r in comp), 1)
            out["conditions"][f"{task}-{cond}"] = d
    json.dump(out, open("instr_compare.json", "w"), indent=1, ensure_ascii=False)
    print("prices/1M:", out["price_per_million_usd"])
    print(f"{'run':10} {'cls':6} {'total':>6} req0(cw/cr) {'TSrch tok':>9} | BASE  SETUP BP    NAT   OUT   | bp# nat# | grade")
    for r in rows:
        c = r["categories_usd"]
        print(f"{r['run']:10} {r['cache_class']:6} {r['total_usd']:.3f}  {r['req0_cache_write']:>5}/{r['req0_cache_read']:<6} {r['toolsearch_tokens']:>9} | "
              f"{c['BASE']:.3f} {c.get('SETUP',0):.3f} {c.get('BP_RESULT',0):.3f} {c.get('NATIVE_RESULT',0):.3f} {c['OUTPUT']:.3f} | {r['bp_calls']:>3} {r['native_calls']:>4} | {'PASS' if r['grade']['pass'] else 'FAIL'} {'DEGRADED' if r['degraded'] else ''} sites={r['context_change_sites_all_pages']} {r['grade'].get('unexplained_line_numbers_in_call_site_section','')}")
    print()
    for k, d in out["conditions"].items():
        print(k, json.dumps(d))


if __name__ == "__main__":
    main()
