#!/usr/bin/env python3
"""Measure what the semantic fleet actually costs this machine (#19 task 15).

`brainprint-engine` cannot read a child process's resident size: the
Workspace denies `unsafe_code`, so `getrusage` and the platform process
APIs are out of reach, and `ResourceUsage` says `None` rather than
inventing a zero. That is the right answer for product code and a
useless one for a benchmark, so the measurement lives out here instead.

What it does: start `i4_final_benchmark --fleet-hold-ms`, which brings
up one backend per installed family in one supervisor and holds them,
then sample the *benchmark process's own descendant tree* repeatedly and
report each process's peak.

Scope, stated rather than assumed: every figure is the whole
fixture-owned process tree under the harness -- a Node child and its
workers, the Roslyn server and whatever `dotnet` puts under it,
rust-analyzer -- and nothing else on the machine. Processes are matched
by descent from the harness pid, never by name, so an editor's own
language server running in the background cannot wander into the total.

Usage:
    python3 scripts/i4_final_acceptance/measure_rss.py --hold-ms 25000
"""

from __future__ import annotations

import argparse
import collections
import json
import os
import re
import shutil
import subprocess
import sys
import time

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def process_table() -> dict[int, tuple[int, int, str]]:
    """Every process as pid -> (ppid, rss_bytes, command).

    `ps` is a portable-enough reader for pid/ppid/rss on macOS and Linux
    both. RSS is reported in kibibytes by both, which is the one unit
    conversion here.
    """
    output = subprocess.run(
        ["ps", "-Ao", "pid=,ppid=,rss=,comm="],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    table: dict[int, tuple[int, int, str]] = {}
    for line in output.splitlines():
        parts = line.split(None, 3)
        if len(parts) < 4:
            continue
        try:
            pid, ppid, rss_kib = int(parts[0]), int(parts[1]), int(parts[2])
        except ValueError:
            continue
        table[pid] = (ppid, rss_kib * 1024, parts[3].strip())
    return table


def descendants(table: dict[int, tuple[int, int, str]], root: int) -> set[int]:
    """Every pid descended from `root`, root excluded.

    Descent, not name matching: a `node` the user happens to be running
    for something else is not part of this measurement, and a backend
    that renames itself still is.
    """
    children: dict[int, list[int]] = collections.defaultdict(list)
    for pid, (ppid, _rss, _comm) in table.items():
        children[ppid].append(pid)
    found: set[int] = set()
    queue = list(children.get(root, []))
    while queue:
        pid = queue.pop()
        if pid in found:
            continue
        found.add(pid)
        queue.extend(children.get(pid, []))
    return found


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--hold-ms", type=int, default=25000)
    parser.add_argument("--interval-ms", type=int, default=250)
    parser.add_argument("--output", default=None, help="write the observations as JSON")
    arguments = parser.parse_args()

    binary = os.path.join(REPO_ROOT, "target", "release", "examples", "i4_final_benchmark")
    if not os.path.exists(binary):
        print(
            "build it first: cargo build --release -p brainprint-engine "
            "--example i4_final_benchmark",
            file=sys.stderr,
        )
        return 2

    harness = subprocess.Popen(
        [binary, "--fleet-hold-ms", str(arguments.hold_ms)],
        stdout=subprocess.PIPE,
        text=True,
        cwd=REPO_ROOT,
    )

    # The harness prints one line once every family is up. Waiting for
    # it rather than for a clock is the difference between measuring the
    # fleet and measuring a process that is still spawning.
    families = "unknown"
    assert harness.stdout is not None
    while True:
        line = harness.stdout.readline()
        if not line:
            print("the harness exited before it reported holding", file=sys.stderr)
            return 1
        print(line.rstrip())
        match = re.search(r"HOLDING pid=(\d+) families=(\S+) live=(\d+)", line)
        if match:
            families = match.group(2)
            live = int(match.group(3))
            break

    peak: dict[int, tuple[int, str]] = {}
    # The sum of per-process peaks is an upper bound nobody ever paid.
    # The largest *instantaneous* total is a figure the machine really
    # held, so both are reported and the difference is visible.
    max_simultaneous = 0
    max_simultaneous_processes = 0
    samples = 0
    deadline = time.monotonic() + (arguments.hold_ms / 1000.0) * 0.9
    while time.monotonic() < deadline and harness.poll() is None:
        table = process_table()
        instant = 0
        alive = 0
        for pid in descendants(table, harness.pid):
            _ppid, rss, comm = table[pid]
            instant += rss
            alive += 1
            previous = peak.get(pid, (0, comm))[0]
            if rss > previous:
                peak[pid] = (rss, comm)
        if instant > max_simultaneous:
            max_simultaneous = instant
            max_simultaneous_processes = alive
        samples += 1
        time.sleep(arguments.interval_ms / 1000.0)

    harness.wait()

    observations = [
        {"pid": pid, "command": comm, "peak_rss_bytes": rss}
        for pid, (rss, comm) in sorted(peak.items(), key=lambda item: -item[1][0])
    ]
    report = {
        "scope": "whole fixture-owned process tree under the harness pid, by descent",
        "families": families,
        "live_runtimes": live,
        "samples": samples,
        "interval_ms": arguments.interval_ms,
        "harness_pid": harness.pid,
        "uname": " ".join(os.uname()),
        "processes": observations,
        "sum_of_peaks_rss_bytes": sum(item["peak_rss_bytes"] for item in observations),
        "max_simultaneous_rss_bytes": max_simultaneous,
        "max_simultaneous_processes": max_simultaneous_processes,
        "note": (
            "per-process figures are that process's peak across the window. "
            "sum_of_peaks is an upper bound nobody paid at once; "
            "max_simultaneous is the largest total actually observed in one "
            "sample and is the figure to quote. Harness RSS is excluded: only "
            "descendants are counted."
        ),
    }
    text = json.dumps(report, indent=2)
    if arguments.output:
        with open(arguments.output, "w", encoding="utf-8") as handle:
            handle.write(text + "\n")
    print(text)
    return 0


if __name__ == "__main__":
    if shutil.which("ps") is None:
        print("no `ps` on this machine: RSS is not measured", file=sys.stderr)
        sys.exit(2)
    sys.exit(main())
