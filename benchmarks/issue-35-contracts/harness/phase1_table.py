"""Render phase1.json as the #35 Phase 1 tables (exact bytes; tokens NOT_MEASURED)."""
import json, sys
d = json.load(open(sys.argv[1]))["candidates"]
print("## contracts (tools/list as served)")
print("| cand | tools | schema B | description B | contract B (name+desc+schema) | largest tool |")
print("|---|---|---|---|---|---|")
for c, v in d.items():
    t = v["tools"]
    big = max(t, key=lambda x: x["contract_bytes"])
    print(f"| {c} | {len(t)} | {sum(x['schema_bytes'] for x in t):,} | {sum(x['description_bytes'] for x in t):,} | "
          f"{sum(x['contract_bytes'] for x in t):,} | {big['name']} {big['contract_bytes']:,} |")
print("\n## workloads: round trips / lookups / expands / invalid / args B / result B (model-visible tool I/O)")
names = list(d["A"]["workloads"])
print("| workload | " + " | ".join(d) + " |")
print("|---|" + "---|" * len(d))
for w in names:
    cells = []
    for c, v in d.items():
        rows = v["workloads"][w]
        rt = len(rows); lk = sum(r["kind"] == "lookup" for r in rows); ex = sum(r["kind"] == "expand" for r in rows)
        inv = sum(r["is_error"] for r in rows)
        io = sum(r["args_bytes"] + r["result_bytes"] for r in rows)
        cells.append(f"{rt}rt {lk}lk {ex}ex {inv}err {io:,}B")
    print(f"| {w} | " + " | ".join(cells) + " |")
print("\n## parity vs A (A-equivalent payloads equal / compared)")
for c, v in d.items():
    if "parity" in v:
        print(c, {w: f"{p['equal']}/{p['compared']}" for w, p in v["parity"].items()})
print("\n## E handle status on expand:",
      sorted({r.get("handle_status") for w in d["E"]["workloads"].values() for r in w if r["kind"] == "expand"}))
