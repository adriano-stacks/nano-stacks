#!/usr/bin/env bash
# Verify a hold's recorded receipt commitments after the interval.
#
# The hold records, for every block the follower executed, its archived
# receipt commitment beside the state root two independent stock oracles
# already byte-confirmed. This finishes the receipts half: for each recorded
# block, an independently executing same-revision witness node's new_block
# payload must digest to the identical commitment, and the payload must match
# the receipt oracle field by field through verify-mainnet-observer.py. A
# missing payload, a differing digest or an oracle mismatch fails the whole
# verification: the hold is not release evidence without this pass.
#
# usage: verify-hold-receipts.sh <hold.jsonl> <witness-events-dir> <receipt-oracle-url> <output.jsonl>
set -euo pipefail

hold=$1
witness=$2
receipt_oracle=${3%/}
output=$4
receipt_digest_bin="$(dirname "$0")/../target/release/receipt-digest"
verifier="$(dirname "$0")/verify-mainnet-observer.py"

test -s "$hold"
test -d "$witness/new_block"
test -x "$receipt_digest_bin"
test -x "$verifier"
test ! -s "$output"
: > "$output"

blocks=0
while IFS= read -r record; do
    [ "$(jq -r .type <<< "$record")" = "block" ] || continue
    blocks=$((blocks + 1))
    height=$(jq -er .height <<< "$record")
    block_hash=$(jq -er .receipts.block <<< "$record")
    root=$(jq -er .state_index_root <<< "$record")
    payload=$(printf '%s/new_block/%08d-%s.json' "$witness" "$height" "$block_hash")
    if [ ! -f "$payload" ]; then
        echo "the witness produced no payload for block $height ($block_hash)" >&2
        exit 1
    fi
    witness_digest=$("$receipt_digest_bin" "$payload")
    recorded=$(jq -cS .receipts <<< "$record")
    if [ "$(jq -cS . <<< "$witness_digest")" != "$recorded" ]; then
        echo "block $height receipts differ: hold $recorded, witness $witness_digest" >&2
        exit 1
    fi
    # The receipt oracle is retried on unavailability (exit 75), never failed
    # over it: it is evidence, not liveness.
    until "$verifier" "$payload" "$receipt_oracle" "$root" >> "$output"; do
        status=$?
        if [ "$status" -ne 75 ]; then
            echo "block $height failed receipt-oracle verification" >&2
            exit 1
        fi
        sleep 15
    done
done < "$hold"

jq -cn --argjson blocks "$blocks" \
    --arg witness "$witness" --arg oracle "$receipt_oracle" \
    '{type: "receipts-complete", blocks: $blocks, witness: $witness, oracle: $oracle}' \
    >> "$output"
echo "verified $blocks blocks against the witness and the receipt oracle"
