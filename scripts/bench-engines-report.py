#!/usr/bin/env python3
"""Summarize a NANO_BENCH_ENGINES samples file.

Input: the TSV `bench_engines_before_sealing` appends — one row per contract
call: height, txid, contract, function, charged runtime cost, compiled wall
nanoseconds (comma-joined repeats), interpreted wall nanoseconds, agreement.

Per call the representative time is the median repeat, so one preempted run
does not speak for an engine. Ratios above 1 mean clarity-wasm is faster.
"""

import statistics
import sys
from collections import defaultdict


def load(path):
    rows = []
    with open(path) as samples:
        for line in samples:
            height, txid, contract, function, runtime, wasm, interp, agree = (
                line.rstrip("\n").split("\t")
            )
            rows.append(
                {
                    "height": int(height),
                    "txid": txid,
                    "call": f"{contract.split('.', 1)[1]}::{function}",
                    "contract": contract,
                    "runtime": int(runtime),
                    "wasm": statistics.median(int(t) for t in wasm.split(",")),
                    "interp": statistics.median(int(t) for t in interp.split(",")),
                    "agree": agree == "true",
                }
            )
    return rows


def milliseconds(nanos):
    return nanos / 1e6


def main():
    if len(sys.argv) != 2:
        sys.exit(f"usage: {sys.argv[0]} <samples.tsv>")
    rows = load(sys.argv[1])
    if not rows:
        sys.exit("the samples file is empty")

    disagreements = [row for row in rows if not row["agree"]]
    wasm_total = sum(row["wasm"] for row in rows)
    interp_total = sum(row["interp"] for row in rows)
    print(f"calls: {len(rows)}   disagreements: {len(disagreements)}")
    print(
        f"total (median per call): wasm {milliseconds(wasm_total):.0f} ms, "
        f"interpreter {milliseconds(interp_total):.0f} ms, "
        f"speedup {interp_total / wasm_total:.2f}x"
    )
    ratios = sorted(row["interp"] / row["wasm"] for row in rows)
    print(
        f"per-call speedup: p10 {ratios[len(ratios) // 10]:.2f}x, "
        f"median {ratios[len(ratios) // 2]:.2f}x, "
        f"p90 {ratios[9 * len(ratios) // 10]:.2f}x"
    )
    # The charged runtime dimension was calibrated to interpreter nanoseconds,
    # so cost-per-nanosecond above ~1 says the block limit overcharges the
    # engine that actually runs.
    charged = [row for row in rows if row["runtime"] > 0]
    if charged:
        wasm_headroom = statistics.median(row["runtime"] / row["wasm"] for row in charged)
        interp_headroom = statistics.median(
            row["runtime"] / row["interp"] for row in charged
        )
        print(
            f"charged runtime units per wall ns (median): "
            f"wasm {wasm_headroom:.2f}, interpreter {interp_headroom:.2f}"
        )

    by_contract = defaultdict(list)
    for row in rows:
        by_contract[row["contract"]].append(row)
    print(f"\n{'contract':64} {'calls':>5} {'wasm ms':>9} {'interp ms':>9} {'speedup':>7}")
    ranked = sorted(
        by_contract.items(), key=lambda item: -sum(row["interp"] for row in item[1])
    )
    for contract, calls in ranked[:20]:
        wasm = sum(row["wasm"] for row in calls)
        interp = sum(row["interp"] for row in calls)
        print(
            f"{contract:64} {len(calls):>5} {milliseconds(wasm):>9.1f} "
            f"{milliseconds(interp):>9.1f} {interp / wasm:>6.2f}x"
        )

    print(f"\nslowest wasm calls:")
    for row in sorted(rows, key=lambda row: -row["wasm"])[:10]:
        print(
            f"  {milliseconds(row['wasm']):>8.2f} ms wasm, "
            f"{milliseconds(row['interp']):>8.2f} ms interp  "
            f"{row['call']}  @{row['height']}"
        )
    for row in disagreements[:10]:
        print(f"DISAGREED: {row['call']} in {row['txid']} @{row['height']}")


if __name__ == "__main__":
    main()
