#!/usr/bin/env python3
"""Summarize a `NANO_VALUE_TRAFFIC` engine-bench attribution file."""

import statistics
import sys


LABELS = [
    "sqlite_string",
    "cache_clone",
    "backing_store_value",
    "value_decode",
    "value_clone",
    "wasm_write",
]


def load(path):
    rows = []
    with open(path, encoding="utf-8") as samples:
        for line in samples:
            fields = line.rstrip("\n").split("\t")
            height, txid, contract, function, repeats = fields[:5]
            repeats = int(repeats)
            traffic = {}
            for label, pair in zip(LABELS, fields[5:]):
                count, size = pair.split(":")
                traffic[label] = (int(count) / repeats, int(size) / repeats)
            rows.append(
                {
                    "height": int(height),
                    "txid": txid,
                    "contract": contract,
                    "call": f"{contract.split('.', 1)[1]}::{function}",
                    "traffic": traffic,
                }
            )
    return rows


def report(rows, title):
    print(f"\n{title} — {len(rows)} calls")
    print(f"  {'boundary':23} {'ops/call':>12} {'bytes/call':>14}")
    for label in LABELS:
        counts = [row["traffic"][label][0] for row in rows]
        sizes = [row["traffic"][label][1] for row in rows]
        print(
            f"  {label:23} {statistics.mean(counts):>12.1f} "
            f"{statistics.mean(sizes):>14.1f}"
        )


def main():
    if len(sys.argv) < 2:
        sys.exit(f"usage: {sys.argv[0]} <samples.tsv.traffic> [contract-filter ...]")
    rows = load(sys.argv[1])
    if not rows:
        sys.exit("the traffic file is empty")
    report(rows, "all calls")
    filters = sys.argv[2:] or ["pox-5", ".loto"]
    for wanted in filters:
        matching = [row for row in rows if wanted in row["contract"]]
        if matching:
            report(matching, wanted)


if __name__ == "__main__":
    main()
