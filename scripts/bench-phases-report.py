#!/usr/bin/env python3
"""Summarize a NANO_WASM_PHASES attribution file (`<samples>.phases`).

One row per benched call: height, txid, contract, function, repeats, then a
`nanos:count` pair per phase in the order clar2wasm's `phases::labels()` names
them. The phases nest — `wasm_invoke` contains the `host_*` and `value_*`
buckets and the setup phases of nested contract-calls — so the report groups
rather than sums them. Counts above the repeat count expose nesting: a
`linker_setup` count of 12 over 3 repeats is 4 setups per top-level call.
"""

import statistics
import sys
from collections import defaultdict

LABELS = [
    "contract_load",
    "linker_setup",
    "instantiate",
    "call_setup",
    "wasm_invoke",
    "return_read",
    "host_var",
    "host_map",
    "host_event",
    "host_stx",
    "host_shape",
    "shape_save",
    "shape_eq",
    "shape_admit",
    "value_write",
    "value_read",
    "ident_read",
    "context_setup",
    "module_probe",
    "commit",
]
TOP_LEVEL = ["context_setup", "module_probe", "contract_load", "linker_setup", "instantiate", "call_setup", "wasm_invoke", "return_read", "commit"]
# Disjoint host buckets inside wasm_invoke; the marshalling rows below overlap
# them (a shape measurement contains its value reads), so they inform but do
# not sum.
NESTED = ["host_var", "host_map", "host_event", "host_stx", "host_shape", "shape_save", "shape_eq", "shape_admit"]
OVERLAPPING = ["value_write", "value_read", "ident_read"]


def load(path):
    rows = []
    with open(path) as samples:
        for line in samples:
            fields = line.rstrip("\n").split("\t")
            height, txid, contract, function, repeats = fields[:5]
            repeats = int(repeats)
            phases = {}
            for label, pair in zip(LABELS, fields[5:]):
                nanos, count = pair.split(":")
                phases[label] = (int(nanos) / repeats, int(count) / repeats)
            rows.append(
                {
                    "contract": contract,
                    "call": f"{contract.split('.', 1)[1]}::{function}",
                    "height": int(height),
                    "phases": phases,
                }
            )
    return rows


def report(rows, title):
    total = {label: statistics.mean(row["phases"][label][0] for row in rows) for label in LABELS}
    counts = {label: statistics.mean(row["phases"][label][1] for row in rows) for label in LABELS}
    top = sum(total[label] for label in TOP_LEVEL)
    print(f"\n{title} — {len(rows)} calls, mean {top / 1e6:.2f} ms/call")
    print(f"  {'phase':15} {'ms/call':>9} {'% of call':>9} {'ops/call':>9}")
    for label in TOP_LEVEL:
        print(
            f"  {label:15} {total[label] / 1e6:>9.3f} {100 * total[label] / top:>8.1f}% "
            f"{counts[label]:>9.1f}"
        )
    print("  inside wasm_invoke (disjoint):")
    accounted = 0
    for label in NESTED:
        accounted += total[label]
        print(
            f"    {label:13} {total[label] / 1e6:>9.3f} "
            f"{100 * total[label] / max(total['wasm_invoke'], 1):>8.1f}% {counts[label]:>9.1f}"
        )
    residue = total["wasm_invoke"] - accounted
    print(
        f"    {'(unattributed)':13} {residue / 1e6:>9.3f} "
        f"{100 * residue / max(total['wasm_invoke'], 1):>8.1f}%  wasm code + glue + nested setups"
    )
    print("  overlapping marshalling (inside the buckets above):")
    for label in OVERLAPPING:
        print(
            f"    {label:13} {total[label] / 1e6:>9.3f} "
            f"{100 * total[label] / max(total['wasm_invoke'], 1):>8.1f}% {counts[label]:>9.1f}"
        )


def main():
    if len(sys.argv) < 2:
        sys.exit(f"usage: {sys.argv[0]} <samples.tsv.phases> [contract-filter ...]")
    rows = load(sys.argv[1])
    if not rows:
        sys.exit("the phases file is empty")
    report(rows, "all calls")
    filters = sys.argv[2:] or sorted(
        {row["contract"] for row in rows},
        key=lambda contract: -sum(
            sum(row["phases"][label][0] for label in TOP_LEVEL)
            for row in rows
            if row["contract"] == contract
        ),
    )[:6]
    for wanted in filters:
        matching = [row for row in rows if wanted in row["contract"]]
        if matching:
            report(matching, wanted)


if __name__ == "__main__":
    main()
