#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -lt 4 ]; then
    echo "usage: $0 OUTPUT LOCAL_URL ORACLE_URL ORACLE_URL..." >&2
    exit 2
fi

readonly output="$1"
readonly local_url="${2%/}"
shift 2
readonly -a oracle_urls=("$@")

exec 9>"${output}.lock"
if ! flock -n 9; then
    echo "another mainnet-cycle watcher owns ${output}" >&2
    exit 1
fi

derive_unbroken_pox_id() {
    python3 -c '
import hashlib
import json
import sys

snapshot = json.load(sys.stdin)
header = bytes.fromhex(snapshot["burn_block_hash"].removeprefix("0x"))
identifier = bytes.fromhex(snapshot["sortition_id"].removeprefix("0x"))
for bits in range(1, 257):
    value = "1" * bits
    if hashlib.new("sha512_256", header + value.encode()).digest() == identifier:
        print(json.dumps({"bits": bits, "consensus_bytes": value}))
        break
else:
    raise SystemExit("sortition identifier has no unbroken PoX history")
'
}

last_height=0
if [ -f "$output" ]; then
    if ! last_height=$(jq -se '
        map(.local.burn_block_height) as $heights |
        if ($heights | length) == 0 then
            0
        elif
            ($heights | unique | length) == ($heights | length) and
            (($heights | max) - ($heights | min) + 1) == ($heights | length)
        then
            $heights | max
        else
            error("heights are duplicated or discontinuous")
        end
    ' "$output"); then
        echo "existing cycle evidence is malformed or discontinuous: ${output}" >&2
        exit 1
    fi
fi

while true; do
    # `/v3/sortitions` includes the current locally derived burn view even when
    # that Bitcoin block elected no miner. `latest_and_last` deliberately skips
    # those views, which would leave holes in a whole-cycle comparison.
    local_response=$(curl -fsS --max-time 10 "$local_url/v3/sortitions" || true)
    current_snapshot=$(jq -ce '.[0]' <<<"$local_response" 2>/dev/null || true)
    current_height=$(jq -r '.burn_block_height // 0' <<<"$current_snapshot" 2>/dev/null || true)
    current_height=${current_height:-0}

    if [ "$current_height" -gt "$last_height" ]; then
        start_height=$current_height
        if [ "$last_height" -ne 0 ]; then
            start_height=$((last_height + 1))
        fi

        for ((height = start_height; height <= current_height; height++)); do
            if [ "$height" -eq "$current_height" ]; then
                local_snapshot=$current_snapshot
            else
                local_snapshot=$(curl -fsS --max-time 10 \
                    "$local_url/v3/sortitions/burn_height/$height" 2>/dev/null |
                    jq -ce '.[0]' || true)
                # Older deployed nano nodes retain the same recent local views
                # under their consensus-hash route but do not expose the
                # stacks-core-compatible burn-height route yet. Use an oracle's
                # hash only as the lookup key, then require nano's own answer to
                # name the requested height before comparing either oracle.
                if [ -z "$local_snapshot" ]; then
                    lookup_consensus=$(curl -fsS --max-time 10 \
                        "${oracle_urls[0]%/}/v3/sortitions/burn_height/$height" \
                        2>/dev/null | jq -er '.[0].consensus_hash' || true)
                    local_snapshot=$(curl -fsS --max-time 10 \
                        "$local_url/v3/sortitions/consensus/${lookup_consensus#0x}" \
                        2>/dev/null | jq -ce '.[0]' || true)
                fi
            fi
            local_snapshot_height=$(jq -r '.burn_block_height // 0' \
                <<<"${local_snapshot:-null}" 2>/dev/null || true)
            if [ "$local_snapshot_height" -ne "$height" ]; then
                echo "local node has no retained sortition at burn height ${height}" >&2
                exit 1
            fi

            local_info=$(curl -fsS --max-time 10 "$local_url/v2/info" || echo null)
            local_pox_id=$(derive_unbroken_pox_id <<<"$local_snapshot" || echo null)
            oracles='[]'
            oracles_ready=true
            for oracle_url in "${oracle_urls[@]}"; do
                oracle_url=${oracle_url%/}
                snapshot=$(curl -fsS --max-time 10 \
                    "$oracle_url/v3/sortitions/burn_height/$height" 2>/dev/null |
                    jq -ce '.[0]' || true)
                if [ -z "$snapshot" ]; then
                    oracles_ready=false
                    break
                fi
                info=$(curl -fsS --max-time 10 "$oracle_url/v2/info" 2>/dev/null || echo null)
                pox_id=$(derive_unbroken_pox_id <<<"$snapshot" || echo null)
                oracles=$(jq -cn \
                    --argjson entries "$oracles" \
                    --arg url "$oracle_url" \
                    --argjson snapshot "${snapshot:-null}" \
                    --argjson pox_id "$pox_id" \
                    --argjson info "$info" \
                    '$entries + [{url: $url, snapshot: $snapshot, pox_id: $pox_id, info: $info}]')
            done
            if [ "$oracles_ready" = false ]; then
                echo "waiting for oracle evidence at burn height ${height}" >&2
                break
            fi

            jq -cn \
                --arg timestamp "$(date -u +%FT%TZ)" \
                --argjson local "$local_snapshot" \
                --argjson local_pox_id "$local_pox_id" \
                --argjson local_info "$local_info" \
                --argjson oracles "$oracles" '
                    def consensus_fields: {
                        burn_block_hash,
                        burn_block_height,
                        sortition_id,
                        parent_sortition_id,
                        consensus_hash,
                        was_sortition,
                        miner_pk_hash160,
                        stacks_parent_ch,
                        last_sortition_ch,
                        committed_block_hash,
                        vrf_seed
                    };
                    ($local | consensus_fields) as $expected |
                    {
                        timestamp: $timestamp,
                        local: $local,
                        local_pox_id: $local_pox_id,
                        local_info: $local_info,
                        oracles: [
                            $oracles[] |
                            . + {
                                matches_local: (
                                    .snapshot != null and
                                    (.snapshot | consensus_fields) == $expected
                                )
                            }
                        ]
                    }
                ' >> "$output"
            last_height=$height
        done
    fi

    sleep 60
done
