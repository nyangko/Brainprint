#!/usr/bin/env python3
"""I5 Task 14 multi-task session driver: ONE Claude Code process, N user turns in a row.

usage: session.py <out-prefix> <prompts.json> <task,task,...> <daemon-pid|0> -- <claude command ...>

The command must contain `-p --input-format stream-json --output-format stream-json --verbose`.
Each task prompt is sent only after the previous turn's `result` event arrived, so the whole
sequence is one session (one prompt-cache lineage, one context). Writes
  <prefix>.stream.jsonl  raw stdout lines
  <prefix>.times.jsonl   {"i": line number, "t": epoch seconds when the line was read}
  <prefix>.turns.json    per turn: task, t_send, t_result, wall_s, exit state
  <prefix>.rss.json      peak RSS (KiB) of the Brainprint processes and of claude, sampled every 0.5 s,
                         plus a snapshot at the end of every turn
"""

import json
import os
import queue
import subprocess
import sys
import threading
import time

TURN_TIMEOUT_S = 900
WATCH = ("brainprintd", "brainprint-mcp", "brainprint-agent", "rust-analyzer", "rust-analyzer-proc-macro-srv", "claude")


def snapshot(claude_pid, daemon_pid):
    """{name: rss_kib} summed over the Brainprint-related descendants of the daemon and of claude."""
    out = subprocess.run(["ps", "-axo", "pid=,ppid=,rss=,comm="], capture_output=True, text=True).stdout
    rows = {}
    for line in out.splitlines():
        parts = line.split(None, 3)
        if len(parts) == 4:
            rows[int(parts[0])] = (int(parts[1]), int(parts[2]), os.path.basename(parts[3].strip()))
    def under(root):
        seen = {root} if root in rows else set()
        grew = True
        while grew:
            grew = False
            for pid, (ppid, _, _) in rows.items():
                if ppid in seen and pid not in seen:
                    seen.add(pid)
                    grew = True
        return seen
    mine = under(claude_pid) | (under(daemon_pid) if daemon_pid else set())
    snap = {}
    for pid in mine:
        _, rss, name = rows[pid]
        if name in WATCH:
            snap[name] = snap.get(name, 0) + rss
    snap["brainprint_total"] = sum(v for k, v in snap.items() if k != "claude")
    return snap


def main():
    prefix, prompts_path, tasks, daemon_pid = sys.argv[1], sys.argv[2], sys.argv[3].split(","), int(sys.argv[4])
    cmd = sys.argv[sys.argv.index("--") + 1:]
    prompts = json.load(open(prompts_path))
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(prefix + ".stderr", "w"),
                            text=True, bufsize=1)
    lines = queue.Queue()

    def reader():
        for i, line in enumerate(proc.stdout):
            lines.put((i, time.time(), line))
        lines.put(None)

    peaks, ends, stop = {}, [], threading.Event()

    def sampler():
        while not stop.is_set():
            try:
                for k, v in snapshot(proc.pid, daemon_pid).items():
                    peaks[k] = max(peaks.get(k, 0), v)
            except Exception:  # a sample must never end the run
                pass
            stop.wait(0.5)

    threading.Thread(target=reader, daemon=True).start()
    threading.Thread(target=sampler, daemon=True).start()
    stream, times = open(prefix + ".stream.jsonl", "w"), open(prefix + ".times.jsonl", "w")
    turns = []
    alive = True
    for task in tasks:
        msg = {"type": "user", "message": {"role": "user", "content": prompts[task]}}
        t_send = time.time()
        proc.stdin.write(json.dumps(msg) + "\n")
        proc.stdin.flush()
        state = "timeout"
        while time.time() - t_send < TURN_TIMEOUT_S:
            try:
                item = lines.get(timeout=5)
            except queue.Empty:
                continue
            if item is None:
                state, alive = "process_exited", False
                break
            i, t, line = item
            stream.write(line)
            times.write(json.dumps({"i": i, "t": t}) + "\n")
            try:
                if json.loads(line).get("type") == "result":
                    state = "ok"
                    break
            except ValueError:
                pass
        t_result = time.time()
        try:
            ends.append({"task": task, "rss_kib": snapshot(proc.pid, daemon_pid)})
        except Exception:
            ends.append({"task": task, "rss_kib": {}})
        turns.append({"task": task, "t_send": t_send, "t_result": t_result, "wall_s": round(t_result - t_send, 2), "state": state})
        if state != "ok":
            break
    stream.flush()
    times.flush()
    try:
        proc.stdin.close()
    except OSError:
        pass
    try:
        proc.wait(timeout=60)
    except subprocess.TimeoutExpired:
        proc.kill()
    while True:  # anything after the last result (there should be nothing)
        try:
            item = lines.get(timeout=1)
        except queue.Empty:
            break
        if item is None:
            break
        stream.write(item[2])
        times.write(json.dumps({"i": item[0], "t": item[1]}) + "\n")
    stop.set()
    stream.close()
    times.close()
    json.dump(turns, open(prefix + ".turns.json", "w"), indent=1)
    json.dump({"peak_rss_kib": peaks, "end_of_turn_rss_kib": ends, "sample_interval_s": 0.5}, open(prefix + ".rss.json", "w"), indent=1)
    sys.exit(0 if all(t["state"] == "ok" for t in turns) and len(turns) == len(tasks) else 1)


if __name__ == "__main__":
    main()
