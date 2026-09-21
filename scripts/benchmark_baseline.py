#!/usr/bin/env python3
"""Run a deterministic no-Brainprint filesystem/text-search baseline."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import sys
import time
from typing import Any


def line_count(data: bytes) -> int:
    if not data:
        return 0
    return data.count(b"\n") + (0 if data.endswith(b"\n") else 1)


def peak_rss_bytes() -> int | None:
    try:
        import resource
    except ImportError:
        return None

    value = int(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss)
    if sys.platform == "darwin":
        return value
    return value * 1024


def load_scenario(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def run_baseline(repo_root: Path, scenario: dict[str, Any]) -> dict[str, Any]:
    workspace = repo_root / scenario["workspace"]
    query = scenario["query"].encode("utf-8")
    extensions = set(scenario["include_extensions"])
    expected = set(scenario["expected_matches"])

    started_at = int(time.time() * 1000)
    started = time.perf_counter_ns()
    cpu_started = time.process_time_ns()

    tool_calls = 0
    source_read_bytes = 0
    source_read_lines = 0
    duplicate_read_bytes = 0

    # Tool 1: list candidate source/config files.
    tool_calls += 1
    candidates = sorted(
        path
        for path in workspace.rglob("*")
        if path.is_file() and path.suffix in extensions
    )

    # Tool 2: broad text search across the candidate set.
    tool_calls += 1
    matched: list[Path] = []
    first_read_sizes: dict[Path, int] = {}
    for path in candidates:
        data = path.read_bytes()
        first_read_sizes[path] = len(data)
        source_read_bytes += len(data)
        source_read_lines += line_count(data)
        if query in data:
            matched.append(path)

    # Tool N: read every search hit again, matching a common agent exploration flow.
    for path in matched:
        tool_calls += 1
        data = path.read_bytes()
        source_read_bytes += len(data)
        source_read_lines += line_count(data)
        duplicate_read_bytes += min(first_read_sizes.get(path, 0), len(data))

    relative_matches = {
        path.relative_to(workspace).as_posix()
        for path in matched
    }
    missing = sorted(expected - relative_matches)
    success = not missing

    ended_at = int(time.time() * 1000)
    elapsed_ms = max(0, (time.perf_counter_ns() - started) // 1_000_000)
    cpu_ms = max(0, (time.process_time_ns() - cpu_started) // 1_000_000)

    return {
        "schema_version": 1,
        "run_id": f"{scenario['id']}-{started_at}-{os.getpid()}",
        "scenario_id": scenario["id"],
        "variant": scenario["variant"],
        "build": None,
        "started_at_unix_ms": started_at,
        "ended_at_unix_ms": ended_at,
        "elapsed_ms": elapsed_ms,
        "metrics": {
            "tool_calls": tool_calls,
            "source_read_bytes": source_read_bytes,
            "source_read_lines": source_read_lines,
            "duplicate_read_bytes": duplicate_read_bytes,
            "process_cpu_ms": cpu_ms,
            "peak_rss_bytes": peak_rss_bytes(),
        },
        "success": success,
        "failure_kind": None if success else "acceptance_mismatch",
        "notes": (
            f"matches={','.join(sorted(relative_matches))}"
            if success
            else f"missing={','.join(missing)}"
        ),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--scenario", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parents[1]
    scenario = load_scenario(repo_root / args.scenario)
    result = run_baseline(repo_root, scenario)

    output = repo_root / args.output
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(result, separators=(",", ":"), sort_keys=True))
        handle.write("\n")

    print(
        json.dumps(
            {
                "scenario_id": result["scenario_id"],
                "success": result["success"],
                "tool_calls": result["metrics"]["tool_calls"],
                "source_read_bytes": result["metrics"]["source_read_bytes"],
                "duplicate_read_bytes": result["metrics"]["duplicate_read_bytes"],
                "output": output.relative_to(repo_root).as_posix(),
            },
            sort_keys=True,
        )
    )
    return 0 if result["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
