"""I5 Task 14 cost breakdown (measurement only).

Rebuilds every API request of a Claude Code stream-json run, attributes the
tokens that a tool result added to the *next* request's cache-write (a measured
number, not a bytes/4 guess), and prices each request with per-token prices
fitted from the runs' own total cost.

usage: breakdown.py <out-dir> [<out-dir> ...]  ->  prints tables, writes breakdown.json
"""

import json
import os
import sys
from collections import defaultdict

TOK = ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens")


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(c.get("text", "") for c in content if isinstance(c, dict))
    return ""


def load(path):
    """-> dict(requests=[...], calls={id: {...}}, result={...})"""
    requests, order, calls, results, res = [], {}, {}, {}, {}
    for line in open(path):
        try:
            d = json.loads(line)
        except ValueError:
            continue
        t = d.get("type")
        if t == "assistant":
            m = d["message"]
            mid = m.get("id")
            if mid not in order:
                order[mid] = len(requests)
                requests.append({"id": mid, "usage": m.get("usage", {}), "blocks": [], "calls": []})
            r = requests[order[mid]]
            for c in m["content"]:
                if c["type"] == "tool_use":
                    calls[c["id"]] = {"name": c["name"], "input": c["input"], "req": order[mid]}
                    r["calls"].append(c["id"])
                r["blocks"].append(c["type"])
        elif t == "user":
            for c in d.get("message", {}).get("content") or []:
                if isinstance(c, dict) and c.get("type") == "tool_result":
                    results[c["tool_use_id"]] = c
        elif t == "result":
            res = d
    for cid, c in calls.items():
        r = results.get(cid, {})
        c["text"] = text_of(r.get("content"))
        c["bytes"] = len(c["text"].encode())
        c["is_error"] = bool(r.get("is_error"))
    return {"requests": requests, "calls": calls, "result": res}


def fit_prices(runs):
    """cost = p_in*in + p_cw*cw + p_cr*cr + p_out*out, least squares over runs (numpy-free)."""
    rows = [([r["result"]["usage"].get(k, 0) for k in TOK], r["result"]["total_cost_usd"]) for r in runs]
    n = 4
    ata = [[sum(x[i] * x[j] for x, _ in rows) for j in range(n)] for i in range(n)]
    atb = [sum(x[i] * y for x, y in rows) for i in range(n)]
    for i in range(n):  # gaussian elimination
        p = max(range(i, n), key=lambda k: abs(ata[k][i]))
        ata[i], ata[p], atb[i], atb[p] = ata[p], ata[i], atb[p], atb[i]
        for k in range(i + 1, n):
            f = ata[k][i] / ata[i][i]
            for j in range(i, n):
                ata[k][j] -= f * ata[i][j]
            atb[k] -= f * atb[i]
    x = [0.0] * n
    for i in reversed(range(n)):
        x[i] = (atb[i] - sum(ata[i][j] * x[j] for j in range(i + 1, n))) / ata[i][i]
    return dict(zip(TOK, x))


def kind(name, inp):
    if name.startswith("mcp__brainprint__brainprint_"):
        tool = name.rsplit("_", 1)[-1]
        mode = inp.get("mode")
        return "BP:" + tool + (":" + mode if mode else "")
    if name == "Bash":
        return "Bash"
    return name


def breakdown(run, price):
    """One row per tool call. The tokens a call's result added to the context are the
    next request's cache-write (shared between calls of one request by result bytes);
    its cost is that write plus one cache-read for every later request."""
    reqs = run["requests"]
    out = []
    for k, r in enumerate(reqs):
        nxt = reqs[k + 1]["usage"] if k + 1 < len(reqs) else None
        block = nxt.get("cache_creation_input_tokens", 0) if nxt else 0
        weight = {cid: run["calls"][cid]["bytes"] + 60 for cid in r["calls"]}  # 60 ~ the tool_use itself
        total = sum(weight.values()) or 1
        later = max(len(reqs) - (k + 1) - 1, 0)
        for cid in r["calls"]:
            c = run["calls"][cid]
            added = block * weight[cid] / total
            write = added * price["cache_creation_input_tokens"]
            carry = added * price["cache_read_input_tokens"] * later
            out.append({"req": k, "kind": kind(c["name"], c["input"]), "input": c["input"], "bytes": c["bytes"],
                        "added_tokens": round(added), "later_requests": later,
                        "write_usd": write, "carry_usd": carry, "marginal_usd": write + carry,
                        "is_error": c["is_error"]})
    return out


def categories(run, price, bd):
    """Where one run's dollars went: BASE (first-request prefix, present in A and B),
    SETUP (B only: what request 0 carries beyond A, and the ToolSearch that loads the tool
    schemas), BP_RESULT, NATIVE_RESULT, OTHER (assistant turns), OUTPUT."""
    reqs = run["requests"]
    cw, cr, pin = price["cache_creation_input_tokens"], price["cache_read_input_tokens"], price["input_tokens"]
    total_in = sum(r["usage"].get("input_tokens", 0) for r in reqs) * pin
    cat = defaultdict(float)
    cat["OUTPUT"] = run["result"]["usage"].get("output_tokens", 0) * price["output_tokens"]
    cat["BASE"] += total_in + reqs[0]["usage"].get("cache_creation_input_tokens", 0) * cw
    # static prefix that every request re-reads
    static = reqs[0]["usage"].get("cache_read_input_tokens", 0)
    cat["BASE"] += static * cr * len(reqs)
    call_cost = defaultdict(float)
    for c in bd:
        k = c["kind"]
        if k == "ToolSearch":
            key = "SETUP"
        elif k.startswith("BP:"):
            key = "BP_RESULT"
        else:
            key = "NATIVE_RESULT"
        cat[key] += c["marginal_usd"]
        call_cost[key] += c["marginal_usd"]
    accounted = sum(cat.values())
    cat["OTHER"] = run["result"]["total_cost_usd"] - accounted
    return dict(cat)


def main():
    dirs = sys.argv[1:]
    runs = {}
    for d in dirs:
        for f in sorted(os.listdir(d)):
            if f.endswith(".stream.jsonl") and not f.startswith("smoke"):
                runs[(os.path.basename(d.rstrip("/")), f.split(".")[0])] = load(os.path.join(d, f))
    price = fit_prices(list(runs.values()))
    print("fitted USD per 1M tokens:", {k: round(v * 1e6, 2) for k, v in price.items()})
    report = {"price_per_token": price, "runs": {}}
    for (d, name), run in runs.items():
        reqs = run["requests"]
        bd = breakdown(run, price)
        per_req = [{"req": k, **{t: r["usage"].get(t, 0) for t in TOK[:3]},
                    "usd": sum(r["usage"].get(t, 0) * price[t] for t in TOK[:3]), "blocks": r["blocks"],
                    "calls": [run["calls"][c]["name"].replace("mcp__brainprint__brainprint_", "BP.") for c in r["calls"]]}
                   for k, r in enumerate(reqs)]
        out_tok = run["result"]["usage"].get("output_tokens", 0)
        report["runs"][f"{d}/{name}"] = {"categories": categories(run, price, bd), "total_usd": run["result"]["total_cost_usd"], "requests": len(reqs),
                                         "output_tokens": out_tok, "output_usd": out_tok * price["output_tokens"],
                                         "fit_usd": sum(x["usd"] for x in per_req) + out_tok * price["output_tokens"],
                                         "per_request": per_req, "calls": bd}
    json.dump(report, open("breakdown.json", "w"), indent=1, default=str)
    return report


if __name__ == "__main__":
    main()
