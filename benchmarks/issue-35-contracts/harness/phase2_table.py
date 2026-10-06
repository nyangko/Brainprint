"""#35 Phase 2 summary: per arm (N = native, A = shipped, B-E = candidates) from the per-arm analyzer outputs and
the proxy call logs. USD and tokens are the client's own (`total_cost_usd`, usage); nothing estimated."""
import glob, json, os, sys
from collections import Counter
P2, OUT_C = sys.argv[1], sys.argv[2]
arms = {}
for X in "ABCDE":
    d = json.load(open(f"{P2}/N_vs_{X}.json"))
    arms.setdefault("N", d["pooled"]["A"])
    arms[X] = d["pooled"]["B"]
    arms[X]["pairs"] = d["pairs"]
rows = []
print("| arm | session USD mean [per session] | B−N | BASE | SETUP(ToolSearch) | BP_RESULT | NATIVE_RESULT | OUTPUT | cache-write / cache-read tok | BP calls / KB | native calls / KB | ToolSearch tok | grades |")
print("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
n = arms["N"]["session_usd_mean"]
for k in "NABCDE":
    a = arms[k]; s = a["split_usd_session_mean"]; t = a["tokens_session_mean"]
    print(f"| {k} | {a['session_usd_mean']:.3f} {a['session_usd']} | {a['session_usd_mean']-n:+.3f} | {s['BASE']:.3f} | {s['SETUP']:.3f} | "
          f"{s['BP_RESULT']:.3f} | {s['NATIVE_RESULT']:.3f} | {s['OUTPUT']:.3f} | {t['cache_write']:.0f} / {t['cache_read']:.0f} | "
          f"{a['bp_calls_session_mean']} / {a['bp_bytes_session_mean']/1000:.1f} | {a['native_calls_session_mean']} / "
          f"{a['native_bytes_session_mean']/1000:.1f} | {a['toolsearch_tokens_session_mean']:.0f} | {a['grades']} |")
print("\nper-task USD mean:")
for k in "NABCDE":
    print(" ", k, arms[k]["per_task_usd_mean"])
print("\nproxy call log (B-E): calls / invalid / lookups / expands / result KB per session")
for k in "BCDE":
    rows = [json.loads(l) for f in glob.glob(f"{OUT_C}/{k}-*.proxy.jsonl") for l in open(f)]
    n_s = len(glob.glob(f"{OUT_C}/{k}-*.proxy.jsonl"))
    c = Counter(r["tool"] for r in rows)
    print(f"  {k}: calls {len(rows)/n_s:.1f}  invalid {sum(r['invalid'] for r in rows)}  lookups {sum(r['lookup'] for r in rows)}  "
          f"expands {sum(r['expand'] for r in rows)}  errors {sum(bool(r['is_error']) for r in rows)}  result_KB {sum(r['result_bytes'] for r in rows)/n_s/1000:.1f}  tools {dict(c)}")
print("\nauto_class (native after BP):")
for k in "ABCDE":
    print(" ", k, arms[k]["auto_class_total"], "native_before_bp", arms[k]["native_before_bp_total"])
