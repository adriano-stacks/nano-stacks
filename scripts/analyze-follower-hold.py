#!/usr/bin/env python3
"""Summarize a hold-follower-mainnet.sh evidence file.

Reads the JSONL run and answers the hold's acceptance questions with numbers:
whether the interval completed, how far the follower lagged the oracles, how
many blocks were verified against which layers, and whether any resource
measurement trends upward without bound. The slope is a least-squares fit per
hour over the whole interval; interpretation stays with the reader.
"""

import json
import sys
from pathlib import Path


def slope_per_hour(points: list[tuple[float, float]]) -> float:
    if len(points) < 2:
        return 0.0
    n = len(points)
    mean_x = sum(x for x, _ in points) / n
    mean_y = sum(y for _, y in points) / n
    numerator = sum((x - mean_x) * (y - mean_y) for x, y in points)
    denominator = sum((x - mean_x) ** 2 for x, _ in points)
    if denominator == 0:
        return 0.0
    return numerator / denominator * 3600


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: analyze-follower-hold.py <hold.jsonl>", file=sys.stderr)
        return 2
    samples, blocks, oracle_checks = [], 0, 0
    start = complete = failure = None
    retractions = []
    for line in Path(sys.argv[1]).read_text().splitlines():
        record = json.loads(line)
        kind = record["type"]
        if kind == "sample":
            samples.append(record)
        elif kind == "block":
            blocks += 1
        elif kind == "oracle":
            oracle_checks += 1
        elif kind == "start":
            start = record
        elif kind == "complete":
            complete = record
        elif kind == "failure":
            failure = record
        elif kind == "retraction":
            retractions.append(record)

    if start is None:
        print("no start record: not a hold run", file=sys.stderr)
        return 1
    print(f"artifact          {start['exe_sha256']}")
    print(f"config            {start['config_sha256']}")
    print(f"started           {start['timestamp']} at height {start['initial_height']}")
    if failure:
        print(f"FAILED            {failure['timestamp']}: {failure['reason']}")
    if complete:
        print(
            f"completed         {complete['timestamp']} after "
            f"{complete['elapsed_seconds']}s at height {complete['verified_height']}"
        )
    print(f"samples           {len(samples)}")
    print(f"blocks verified   {blocks} against both stock oracles and the witness digest")
    print(f"receipt oracle    {oracle_checks} blocks compared field by field")
    print(f"retractions       {len(retractions)}")

    lag_max = 0
    unobserved = 0
    for sample in samples:
        health = sample.get("health")
        if not isinstance(health, dict):
            unobserved += 1
            continue
        tips = sample["oracle_tips"]
        heights = [
            tips[key]["stacks"]
            for key in ("a", "b")
            if isinstance(tips.get(key), dict)
        ]
        if heights:
            lag_max = max(lag_max, max(heights) - health["stacks_height"])
    print(f"worst oracle lag  {lag_max} blocks")
    print(f"unobserved        {unobserved} samples with no health answer")

    samples = [s for s in samples if isinstance(s.get("health"), dict)]
    series = {
        "rss_kb": lambda s: s["resources"]["rss_kb"],
        "open_files": lambda s: s["resources"]["open_files"],
        "disk_available": lambda s: s["resources"]["disk_available"],
        "staging_bytes": lambda s: s["resources"]["database_bytes"]["staging"],
        "marf_wal_bytes": lambda s: s["resources"]["database_bytes"]["marf_wal"],
    }
    for name, extract in series.items():
        points = [(float(s["elapsed_seconds"]), float(extract(s))) for s in samples]
        if not points:
            continue
        print(
            f"{name:<17} first {points[0][1]:.0f}  last {points[-1][1]:.0f}  "
            f"slope {slope_per_hour(points):+.1f}/h"
        )
    return 0 if complete and not failure else 1


if __name__ == "__main__":
    raise SystemExit(main())
