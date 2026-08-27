#!/usr/bin/env python3
"""Compare this node's locally derived consensus hashes with stock nodes'.

Task 082 asks for one thing that no unit test can give: that a sortition chain
*this* node derived from Bitcoin, without asking any peer which fork to be on,
agrees with stock nodes across a reward cycle boundary and through the cycle after
it. A consensus hash mixes the `PoxId`, so a node that guesses the anchor bit at a
boundary derives a wrong hash for every block after it and says nothing about it —
which is exactly why the comparison has to be against somebody else's answer.

The node's own persisted history is the input: `state/consensus-hashes.json`, a
dense array oldest first whose index 0 is mainnet's first PoX height. Nothing in
it came from a peer. The oracles are stock `/v3/sortitions/burn_height/<h>`
answers, and every height is asked of *each* oracle given, so an oracle that has
pruned or lags cannot quietly become the only opinion.

usage:
  compare-derived-consensus.py <consensus-hashes.json> <from-burn> <to-burn> \
      <oracle-url> [oracle-url ...] [--first-height N] [--out FILE] [--pace SECONDS]

Exit status is 0 only if every height was compared and every comparison agreed.
"""

import argparse
import json
import sys
import time
import urllib.error
import urllib.request

# Mainnet's first PoX-relevant burn height, which is index 0 of the history.
DEFAULT_FIRST_HEIGHT = 666_050


def load_history(path):
    with open(path) as handle:
        return json.load(handle)["hashes"]


def ask(oracle, height, attempts=4, pace=0.0):
    """One oracle's consensus hash for one burn height, or None if it cannot say."""
    url = f"{oracle.rstrip('/')}/v3/sortitions/burn_height/{height}"
    for attempt in range(attempts):
        try:
            with urllib.request.urlopen(url, timeout=20) as response:
                body = json.load(response)
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError):
            time.sleep(1 + attempt * 2)
            continue
        entries = body if isinstance(body, list) else [body]
        for entry in entries:
            if entry.get("burn_block_height") == height:
                answer = entry.get("consensus_hash")
                if isinstance(answer, str):
                    if pace:
                        time.sleep(pace)
                    return answer.removeprefix("0x").lower()
        return None
    return None


def main():
    parser = argparse.ArgumentParser(add_help=True)
    parser.add_argument("history")
    parser.add_argument("start", type=int)
    parser.add_argument("end", type=int)
    parser.add_argument("oracles", nargs="+")
    parser.add_argument("--first-height", type=int, default=DEFAULT_FIRST_HEIGHT)
    parser.add_argument("--out")
    parser.add_argument("--pace", type=float, default=0.0)
    arguments = parser.parse_args()

    hashes = load_history(arguments.history)
    last = arguments.first_height + len(hashes) - 1
    if arguments.start < arguments.first_height or arguments.end > last:
        print(
            f"the history covers burn {arguments.first_height}..{last}, "
            f"which does not contain {arguments.start}..{arguments.end}",
            file=sys.stderr,
        )
        return 2

    out = open(arguments.out, "w") if arguments.out else None
    compared = 0
    agreed = 0
    unanswered = 0
    disagreements = []
    for height in range(arguments.start, arguments.end + 1):
        local = hashes[height - arguments.first_height].lower()
        answers = {oracle: ask(oracle, height, pace=arguments.pace) for oracle in arguments.oracles}
        said = {oracle: answer for oracle, answer in answers.items() if answer}
        record = {"burn_block_height": height, "local": local, "oracles": answers}
        if not said:
            unanswered += 1
            record["verdict"] = "no oracle answered"
        else:
            compared += 1
            if all(answer == local for answer in said.values()):
                agreed += 1
                record["verdict"] = "agrees"
            else:
                record["verdict"] = "DISAGREES"
                disagreements.append(record)
        if out:
            out.write(json.dumps(record) + "\n")
            out.flush()
        if height % 100 == 0 or record["verdict"] == "DISAGREES":
            print(f"{height} {record['verdict']} local={local[:12]}", flush=True)
    if out:
        out.close()

    total = arguments.end - arguments.start + 1
    print(
        f"\n{agreed}/{compared} compared heights agree over burn "
        f"{arguments.start}..{arguments.end} ({total} heights, {unanswered} unanswered)"
    )
    for record in disagreements[:10]:
        print(f"  burn {record['burn_block_height']}: local {record['local']} vs {record['oracles']}")
    if disagreements:
        return 1
    if unanswered or compared != total:
        print("not every height was compared, so this proves nothing about the gap")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
