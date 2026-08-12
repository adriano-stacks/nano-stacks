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

last_height=$(tail -n 1 "$output" 2>/dev/null | jq -r '.local.burn_block_height // 0' || true)
last_height=${last_height:-0}

while true; do
    local_response=$(curl -fsS --max-time 10 "$local_url/v3/sortitions/latest_and_last" || true)
    local_snapshot=$(jq -ce '.[0]' <<<"$local_response" 2>/dev/null || true)
    height=$(jq -r '.burn_block_height // 0' <<<"$local_snapshot" 2>/dev/null || true)
    height=${height:-0}

    if [ "$height" -gt "$last_height" ]; then
        local_info=$(curl -fsS --max-time 10 "$local_url/v2/info" || echo null)
        oracles='[]'
        for oracle_url in "${oracle_urls[@]}"; do
            oracle_url=${oracle_url%/}
            snapshot=$(curl -fsS --max-time 10 \
                "$oracle_url/v3/sortitions/latest_and_last" 2>/dev/null |
                jq -ce --argjson height "$height" '.[] | select(.burn_block_height == $height)' || true)
            info=$(curl -fsS --max-time 10 "$oracle_url/v2/info" 2>/dev/null || echo null)
            oracles=$(jq -cn \
                --argjson entries "$oracles" \
                --arg url "$oracle_url" \
                --argjson snapshot "${snapshot:-null}" \
                --argjson info "$info" \
                '$entries + [{url: $url, snapshot: $snapshot, info: $info}]')
        done

        jq -cn \
            --arg timestamp "$(date -u +%FT%TZ)" \
            --argjson local "$local_snapshot" \
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
    fi

    sleep 60
done
