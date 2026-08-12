#!/usr/bin/env bash
# shellcheck disable=SC2016
set -euo pipefail

if [ "$#" -lt 5 ]; then
    echo "usage: $0 OUTPUT NANO_RPC EVENT_DIR CYCLE STOCK_RPC [STOCK_RPC ...]" >&2
    exit 2
fi

readonly output="$1"
readonly nano_rpc="${2%/}"
readonly event_dir="$3"
readonly cycle="$4"
shift 4
readonly stock_rpcs=("$@")

[[ "$cycle" =~ ^[0-9]+$ ]] || { echo "cycle is not an integer: $cycle" >&2; exit 1; }
test ! -e "$output" || { echo "output already exists: $output" >&2; exit 1; }
test -d "$event_dir/new_block" || { echo "new_block events are absent: $event_dir" >&2; exit 1; }

parent="$(dirname "$output")"
mkdir -p "$parent"
temporary="$(mktemp -d "$parent/.signer-evidence.XXXXXX")"
trap 'rm -rf -- "$temporary"' EXIT
mkdir -p "$temporary/new_block" "$temporary/oracles" "$temporary/stacker_set"

curl -fsS --max-time 20 "$nano_rpc/v2/pox" -o "$temporary/pox.json"
curl -fsS --max-time 20 "$nano_rpc/v3/stacker_set/$cycle" \
    -o "$temporary/oracles/nano-cycle-$cycle.json"
for index in "${!stock_rpcs[@]}"; do
    curl -fsS --max-time 20 "${stock_rpcs[$index]%/}/v3/stacker_set/$cycle" \
        -o "$temporary/oracles/stock-$index-cycle-$cycle.json"
done

jq -Sc . "$temporary/oracles/nano-cycle-$cycle.json" > "$temporary/canonical-set.json"
for document in "$temporary"/oracles/stock-*.json; do
    jq -Sc . "$document" | cmp - "$temporary/canonical-set.json"
done
cp "$temporary/oracles/nano-cycle-$cycle.json" "$temporary/stacker_set/cycle-$cycle.json"

first_burn_height="$(jq -r '.first_burnchain_block_height' "$temporary/pox.json")"
cycle_length="$(jq -r '.reward_cycle_length' "$temporary/pox.json")"
cycle_start="$((first_burn_height + cycle * cycle_length))"
cycle_end="$((cycle_start + cycle_length - 1))"
find "$event_dir/new_block" -mindepth 1 -maxdepth 1 -type f -print0 |
    sort -z |
    xargs -0 -r -n 250 jq -rj \
        --argjson start "$cycle_start" \
        --argjson end "$cycle_end" \
        'select(.burn_block_height >= $start and .burn_block_height <= $end) | (input_filename, "\u0000")' \
        > "$temporary/events.list"
test -s "$temporary/events.list" || { echo "no accepted block event belongs to cycle $cycle" >&2; exit 1; }
xargs -0 cp -t "$temporary/new_block" < "$temporary/events.list"
rm "$temporary/events.list"

event_count="$(find "$temporary/new_block" -mindepth 1 -maxdepth 1 -type f -printf . | wc -c)"
test "$event_count" -gt 5 || { echo "only $event_count accepted blocks belong to cycle $cycle" >&2; exit 1; }
first_height="$(find "$temporary/new_block" -mindepth 1 -maxdepth 1 -type f -printf '%f\n' | sort | head -n 1)"
last_height="$(find "$temporary/new_block" -mindepth 1 -maxdepth 1 -type f -printf '%f\n' | sort | tail -n 1)"
first_height="$((10#${first_height%%-*}))"
last_height="$((10#${last_height%%-*}))"
set_sha256="$(sha256sum "$temporary/stacker_set/cycle-$cycle.json" | awk '{print $1}')"
(
    cd "$temporary"
    find new_block oracles stacker_set pox.json -type f -print0 |
        sort -z |
        xargs -0 sha256sum > SHA256SUMS
)
jq -n \
    --arg captured_at "$(date -u +%FT%TZ)" \
    --arg nano_rpc "$nano_rpc" \
    --argjson stock_rpcs "$(printf '%s\n' "${stock_rpcs[@]}" | jq -Rsc 'split("\n")[:-1]')" \
    --argjson cycle "$cycle" \
    --argjson event_count "$event_count" \
    --argjson first_burn_height "$cycle_start" \
    --argjson last_burn_height "$cycle_end" \
    --argjson first_height "$first_height" \
    --argjson last_height "$last_height" \
    --arg set_sha256 "$set_sha256" \
    '{
        captured_at: $captured_at,
        nano_rpc: $nano_rpc,
        stock_rpcs: $stock_rpcs,
        cycle: $cycle,
        accepted_blocks: $event_count,
        cycle_first_burn_height: $first_burn_height,
        cycle_last_burn_height: $last_burn_height,
        first_stacks_height: $first_height,
        last_stacks_height: $last_height,
        stacker_set_sha256: $set_sha256
    }' > "$temporary/manifest.json"

mv "$temporary" "$output"
trap - EXIT
echo "captured cycle $cycle: $event_count blocks, heights $first_height..$last_height, set $set_sha256"
