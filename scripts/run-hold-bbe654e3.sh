#!/usr/bin/env bash
# Task 106, end to end and unattended: wait for both states, hold 24 hours, then
# verify the receipts.
#
# The 24 hours are a property of the task, not a schedule choice, so the only
# thing worth automating is everything around them. This waits for the subject
# and the witness to finish importing and reach the public tip, runs the
# committed harness for the interval, and finishes the receipts half after it.
#
#  subject  release-subject-bbe654e3   the packaged follower artifact, which is
#                                      what task 142's label and the harness both
#                                      name. Serves /health and /metrics on
#                                      loopback and nothing else.
#  witness  witness-bbe654e3           same revision, same compiler, same attested
#                                      bundle, executing independently. The
#                                      artifact omits the `event` role by policy,
#                                      so receipts can only come from here. A
#                                      *stale*-revision witness already failed a
#                                      hold over a fix it lacked, which is why
#                                      same-revision is the whole point.
#
# Both are at revision bbe654e37f75, compiler
# sha256:32862f69de96594600a24a889424dc372c1f8d7dd5918bebbf8a3b61436a87c6,
# profile 7a374d6deda2b4dc3b21228155acfaeaa98358c8dc12868dc255274aa9123351,
# imported from the bundle whose content root 3deca3ada868 both builder keys
# signed on 2026-08-26.
#
# Nothing here signals either node. `disk-guard.sh` is the only thing allowed to,
# and the harness treats a pid change during the interval as fatal to it — so no
# stall supervisor runs, deliberately.
set -euo pipefail

tree=/home/aldur/nano-stacks
subject=/home/aldur/release-subject-bbe654e3
receipts=/home/aldur/hold-receipts-bbe654e3
stamp=$(date -u +%Y%m%dT%H%M%SZ)
log=/home/aldur/hold-bbe654e3-$stamp.log
hold_output=/home/aldur/hold-bbe654e3-$stamp.jsonl
receipt_output=/home/aldur/hold-bbe654e3-$stamp-receipts.jsonl

subject_pattern='nano-stacks-follower-0.1.0-bbe654e37f75/bin/stacks-follower start'

oracle_a=http://172.96.141.17:20443
oracle_b=http://108.130.44.244:20443

exec > >(tee -a "$log") 2>&1
date -u

# Height the network is actually at, from a stock node rather than from either
# subject: "caught up" has to be judged against something neither of them is.
network_tip() {
    curl -s -m 20 "$oracle_a/v2/info" | jq -er .stacks_tip_height
}

# The artifact's own health surface, which is all it exposes.
subject_height() {
    curl -s -m 10 http://127.0.0.1:20478/health | jq -r '.stacks_height // empty'
}

witness_height() {
    curl -s -m 10 http://127.0.0.1:20494/nano/sync_status | jq -r '.executed_stacks_height // empty'
}

wait_for_tip() {
    local what=$1 reader=$2 last=0 still=0
    echo "== waiting for $what to import and reach the tip"
    while true; do
        local here network
        here=$($reader || true)
        network=$(network_tip || true)
        if [ -n "$here" ] && [ -n "$network" ]; then
            local behind=$((network - here))
            [ "$behind" -lt 0 ] && behind=0
            echo "$(date -u +%FT%TZ) $what at $here, network $network, behind $behind"
            # Within a handful of blocks is at tip: the network keeps moving and
            # ordinary propagation is a block or two.
            if [ "$behind" -le 3 ]; then
                echo "$(date -u +%FT%TZ) $what is at the tip"
                return 0
            fi
            if [ "$here" = "$last" ]; then
                still=$((still + 1))
                if [ "$still" -ge 60 ]; then
                    echo "$(date -u +%FT%TZ) $what has not moved for an hour, giving up"
                    return 1
                fi
            else
                still=0
            fi
            last=$here
        else
            echo "$(date -u +%FT%TZ) $what not answering yet"
        fi
        sleep 60
    done
}

wait_for_tip subject subject_height
wait_for_tip witness witness_height

subject_pid=$(pgrep -f "$subject_pattern" | head -1)
test -n "$subject_pid"
subject_exe=$(sha256sum "/proc/$subject_pid/exe" | awk '{print $1}')
echo "== subject pid $subject_pid, exe $subject_exe"

test -d "$receipts/new_block"
echo "== witness has emitted $(find "$receipts/new_block" -name '*.json' | wc -l) new_block payloads so far"

echo "== holding for 24 hours"
HOLD_OUTPUT="$hold_output" \
HOLD_HEALTH_URL=http://127.0.0.1:20478 \
HOLD_METRICS_URL=http://127.0.0.1:20479 \
HOLD_STATE_DIR="$subject/state" \
HOLD_CONFIG="$subject/config.toml" \
HOLD_PID="$subject_pid" \
HOLD_EXE_SHA256="$subject_exe" \
HOLD_ORACLE_A="$oracle_a" \
HOLD_ORACLE_B="$oracle_b" \
HOLD_BITCOIN_TIP_URL=https://mempool.space/api/blocks/tip/height \
HOLD_BLOCK_IDENTITY="$tree/target/release/block-identity" \
    "$tree/scripts/hold-follower-mainnet.sh"

echo "== interval complete, verifying the receipts half"
"$tree/scripts/verify-hold-receipts.sh" \
    "$hold_output" "$receipts" "https://api.hiro.so" "$receipt_output"

date -u
echo "hold complete: $hold_output and $receipt_output"
