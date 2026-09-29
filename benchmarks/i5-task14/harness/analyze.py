import json, re, sys, os, collections

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "out")
NATIVE_BASH = re.compile(r"^\s*(cd [^;&]+[;&]+\s*)?(ls|find|grep|rg|cat|sed|head|tail|awk|wc|tree|git (grep|ls-files|show|log|blame)|less|nl|stat|file)\b")
BP_BASH = re.compile(r"(^|[\s;&|/])brainprint(-agent)?\s")


def classify(tool, inp):
    if tool.startswith("mcp__brainprint__"):
        return "brainprint"
    if tool == "Bash":
        cmd = inp.get("command", "")
        if BP_BASH.search(cmd):
            return "brainprint"
        if NATIVE_BASH.search(cmd):
            return "native"
        return "other_bash"
    if tool in ("Read", "Grep", "Glob"):
        return "native"
    return "meta"  # ToolSearch, Skill, TodoWrite...


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(c.get("text", "") for c in content if isinstance(c, dict))
    return ""


def key_of(tool, inp):
    if tool == "Read":
        return ("Read", inp.get("file_path", "").split("/wt/")[-1].split("/", 1)[-1], inp.get("offset"), inp.get("limit"))
    if tool == "Grep":
        return ("Grep", inp.get("pattern"), inp.get("path"), inp.get("glob"))
    if tool == "Glob":
        return ("Glob", inp.get("pattern"), inp.get("path"))
    if tool == "Bash":
        return ("Bash", inp.get("command"))
    return (tool, json.dumps(inp, sort_keys=True))


def analyze(name):
    calls, results, final, res = [], {}, "", {}
    init = {}
    for line in open(os.path.join(OUT, name + ".stream.jsonl")):
        try:
            d = json.loads(line)
        except ValueError:
            continue
        t = d.get("type")
        if t == "system" and d.get("subtype") == "init":
            init = d
        elif t == "assistant":
            for c in d["message"]["content"]:
                if c["type"] == "tool_use":
                    calls.append(c)
        elif t == "user":
            for c in (d.get("message", {}).get("content") or []):
                if isinstance(c, dict) and c.get("type") == "tool_result":
                    results[c["tool_use_id"]] = c
        elif t == "result":
            res = d
            final = d.get("result", "")
    rows = []
    for c in calls:
        r = results.get(c["id"], {})
        rows.append({
            "tool": c["name"], "cls": classify(c["name"], c["input"]), "key": key_of(c["name"], c["input"]),
            "input": c["input"], "out_bytes": len(text_of(r.get("content"))), "is_error": bool(r.get("is_error")),
            "out": text_of(r.get("content")),
        })
    first_bp = next((i for i, x in enumerate(rows) if x["cls"] == "brainprint"), None)
    discovery = [x for x in rows if x["cls"] in ("brainprint", "native")]
    seen, dups = collections.Counter(), 0
    for x in rows:
        if x["cls"] == "native":
            seen[x["key"]] += 1
    dups = sum(n - 1 for n in seen.values() if n > 1)
    read_paths = collections.Counter(x["key"][1] for x in rows if x["tool"] == "Read")
    dup_file_reads = sum(n - 1 for n in read_paths.values() if n > 1)
    # native reads/searches after Brainprint already returned that file path
    bp_text = ""
    redundant_after_bp = 0
    for x in rows:
        if x["cls"] == "brainprint":
            bp_text += x["out"]
        elif x["cls"] == "native" and x["tool"] == "Read" and bp_text:
            if x["key"][1] and x["key"][1] in bp_text:
                redundant_after_bp += 1
    blocked = [x for x in rows if x["is_error"] and re.search(r"(?i)hook|brainprint", x["out"])]
    tel = []
    tp = os.path.join(OUT, name + ".telemetry.jsonl")
    if os.path.exists(tp):
        tel = [json.loads(l) for l in open(tp) if l.strip()]
    u = res.get("usage", {}) or {}
    return {
        "name": name,
        "model": init.get("model"), "cc_version": init.get("claude_code_version"),
        "bp_tools_available": len([t for t in init.get("tools", []) if "brainprint" in t]),
        "tool_calls": len(rows),
        "first_route": (rows[0]["tool"] + (":" + rows[0]["input"].get("command", "")[:40] if rows[0]["tool"] == "Bash" else "")) if rows else None,
        "first_discovery_route": (discovery[0]["cls"] + "/" + discovery[0]["tool"]) if discovery else None,
        "brainprint_calls": sum(x["cls"] == "brainprint" for x in rows),
        "native_discovery": sum(x["cls"] == "native" for x in rows),
        "native_by_tool": dict(collections.Counter(x["tool"] for x in rows if x["cls"] == "native")),
        "native_before_first_bp": (sum(x["cls"] == "native" for x in rows[:first_bp]) if first_bp is not None else None),
        "other_bash": sum(x["cls"] == "other_bash" for x in rows),
        "meta": dict(collections.Counter(x["tool"] for x in rows if x["cls"] == "meta")),
        "duplicate_native_calls": dups, "duplicate_file_reads": dup_file_reads,
        "native_read_after_bp_same_file": redundant_after_bp,
        "native_result_bytes": sum(x["out_bytes"] for x in rows if x["cls"] == "native"),
        "read_bytes": sum(x["out_bytes"] for x in rows if x["tool"] == "Read"),
        "brainprint_delivered_bytes": sum(x["out_bytes"] for x in rows if x["cls"] == "brainprint"),
        "hook_blocked_calls": len(blocked),
        "telemetry": {
            "events": len(tel),
            "decisions": dict(collections.Counter(e["decision"] for e in tel)),
            "fallbacks": dict(collections.Counter(e["fallback_reason"] for e in tel if e["fallback_reason"])),
            "native_attempts": dict(collections.Counter(e["native_attempt"] for e in tel if e["native_attempt"])),
            "exact_substitute_proven": sum(1 for e in tel if e["exact_substitute_proven"]),
            "hook_latency_us_p50": sorted(e["latency_us"] for e in tel)[len(tel) // 2] if tel else None,
            "hook_latency_us_max": max((e["latency_us"] for e in tel), default=None),
            "injected_bytes": sum(e["bootstrap_bytes"] + e["message_bytes"] for e in tel),
        },
        "duration_ms": res.get("duration_ms"), "num_turns": res.get("num_turns"), "cost_usd": res.get("total_cost_usd"),
        "tokens": {k: u.get(k) for k in ("input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens")},
        "is_error": res.get("is_error"),
        "sequence": [x["cls"][0].upper() + ":" + x["tool"] + ((" " + x["input"].get("command", "")[:60]) if x["tool"] == "Bash" else (" " + str(x["key"][1])[:60] if x["tool"] in ("Read", "Grep", "Glob") else (" " + json.dumps({k: v for k, v in x["input"].items() if k in ("mode", "symbol_name", "direction", "pattern", "path_prefix", "resource_path", "query")})[:80]))) for x in rows],
        "final": final,
    }


def grade(task, final):
    f = final
    if task == "T1":
        return {
            "signature": "load_workspace_config(paths: &WorkspacePaths) -> Result<WorkspaceConfig, ConfigError>" in f,
            "decl_line_213": bool(re.search(r"config\.rs:213\b", f)),
            "prod_lifecycle_load": ("lifecycle.rs" in f) and bool(re.search(r"WorkspaceLifecycle::load|\bload\b", f)),
            "prod_query_surface_open": ("query_surface.rs" in f) and ("open" in f),
            "test_sites_cited": len(set(re.findall(r"\b(446|462|472|525|538|550)\b", f))),
        }
    return {
        "env_var": "BRAINPRINT_ADOPTION_TELEMETRY_PATH" in f,
        "append_fn": "append" in f,
        "jsonl": bool(re.search(r"(?i)jsonl|json line|one json", f)),
        "fields_found": sum(1 for k in "timestamp_unix_ms client client_version session reset_source mode event native_attempt route exact_substitute_proven decision fallback_reason suggested latency_us daemon_probe_count daemon_probe_us state_bytes bootstrap_bytes message_bytes".split() if re.search(r"\b" + k + r"\b", f)),
        "emitters_event_bridge": ("bridge" in f) and bool(re.search(r"\bevent\b", f)),
    }


if __name__ == "__main__":
    names = sys.argv[1:] or sorted({f.split(".")[0] for f in os.listdir(OUT) if f.endswith(".stream.jsonl") and not f.startswith("smoke")})
    allr = []
    for n in names:
        a = analyze(n)
        a["grade"] = grade(n[:2], a["final"])
        allr.append(a)
    json.dump(allr, open(os.path.join(OUT, "summary.json"), "w"), indent=1, ensure_ascii=False)
    for a in allr:
        print(json.dumps({k: v for k, v in a.items() if k not in ("sequence", "final")}, ensure_ascii=False))
