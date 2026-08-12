#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 7 ]; then
    echo "usage: $0 OUTPUT RPC_URL METRICS_URL BITCOIN_TIP_URL SYSTEMD_UNIT STATE_DIR CONFIG" >&2
    exit 2
fi

readonly output="$1"
readonly rpc_url="${2%/}"
readonly metrics_url="${3%/}"
readonly bitcoin_tip_url="$4"
readonly unit="$5"
readonly state_dir="$6"
readonly config="$7"
readonly interval_seconds=60
readonly duration_seconds=86400

fail() {
    local reason="$1"
    jq -cn \
        --arg timestamp "$(date -u +%FT%TZ)" \
        --arg reason "$reason" \
        '{type: "failure", timestamp: $timestamp, reason: $reason}' >> "$output"
    echo "$reason" >&2
    exit 1
}

read_bitcoin_tip() {
    local height
    height="$(curl -fsS --max-time 10 "$bitcoin_tip_url")"
    [[ "$height" =~ ^[0-9]+$ ]] || return 1
    printf '%s\n' "$height"
}

test -d "$state_dir" || { echo "state directory is absent: $state_dir" >&2; exit 1; }
test -r "$config" || { echo "config is unreadable: $config" >&2; exit 1; }
test ! -s "$output" || { echo "output already contains a run: $output" >&2; exit 1; }
mkdir -p "$(dirname "$output")"
: > "$output"

if rg -qi 'api\.(mainnet\.)?hiro\.so|api\.hiro\.so' "$config"; then
    fail "the release config names a hosted Stacks API"
fi

initial_pid="$(systemctl --user show "$unit" --property MainPID --value)"
readonly initial_pid
test "$initial_pid" -gt 0 || fail "the release unit has no main process"
initial_start_ticks="$(awk '{print $22}' "/proc/$initial_pid/stat")"
readonly initial_start_ticks
executable="$(readlink -f "/proc/$initial_pid/exe")"
readonly executable
executable_sha256="$(sha256sum "$executable" | awk '{print $1}')"
readonly executable_sha256
config_sha256="$(sha256sum "$config" | awk '{print $1}')"
readonly config_sha256

initial_sync="$(curl -fsS --max-time 10 "$rpc_url/nano/sync_status")" || \
    fail "the release RPC did not answer before the soak"
initial_bitcoin_tip="$(read_bitcoin_tip)" || fail "the Bitcoin tip source did not answer"
jq -e '
    .blocks_behind == 0 and
    .p2p_sessions > 0 and
    (.event_observers | length > 0) and
    (.event_observers | all(.reachable and .undelivered == 0))
' <<< "$initial_sync" >/dev/null || fail "the release node is not caught up and observable"

jq -cn \
    --arg timestamp "$(date -u +%FT%TZ)" \
    --arg unit "$unit" \
    --argjson pid "$initial_pid" \
    --arg executable "$executable" \
    --arg executable_sha256 "$executable_sha256" \
    --arg config "$config" \
    --arg config_sha256 "$config_sha256" \
    --arg state_dir "$state_dir" \
    --arg bitcoin_tip_url "$bitcoin_tip_url" \
    --argjson bitcoin_tip "$initial_bitcoin_tip" \
    --argjson duration_seconds "$duration_seconds" \
    '{
        type: "start",
        timestamp: $timestamp,
        unit: $unit,
        pid: $pid,
        executable: $executable,
        executable_sha256: $executable_sha256,
        config: $config,
        config_sha256: $config_sha256,
        state_dir: $state_dir,
        bitcoin_tip_url: $bitcoin_tip_url,
        bitcoin_tip: $bitcoin_tip,
        duration_seconds: $duration_seconds
    }' >> "$output"

SECONDS=0
while [ "$SECONDS" -lt "$duration_seconds" ]; do
    current_pid="$(systemctl --user show "$unit" --property MainPID --value)"
    [ "$current_pid" = "$initial_pid" ] || fail "the release process changed"
    current_start_ticks="$(awk '{print $22}' "/proc/$current_pid/stat" 2>/dev/null || true)"
    [ "$current_start_ticks" = "$initial_start_ticks" ] || fail "the release process restarted"

    info="$(curl -fsS --max-time 10 "$rpc_url/v2/info")" || fail "the release info RPC failed"
    sync="$(curl -fsS --max-time 10 "$rpc_url/nano/sync_status")" || \
        fail "the release sync RPC failed"
    metrics="$(curl -fsS --max-time 10 "$metrics_url/metrics")" || \
        fail "the release metrics RPC failed"
    bitcoin_tip="$(read_bitcoin_tip)" || fail "the Bitcoin tip source failed"
    memory_current="$(systemctl --user show "$unit" --property MemoryCurrent --value)"
    memory_peak="$(systemctl --user show "$unit" --property MemoryPeak --value)"
    open_files="$(find "/proc/$current_pid/fd" -mindepth 1 -maxdepth 1 -printf . | wc -c)"
    disk_available="$(df -B1 --output=avail "$state_dir" | tail -n 1 | tr -d ' ')"
    db_sizes="$(jq -cn \
        --argjson marf "$(stat -c %s "$state_dir/chainstate/marf.sqlite")" \
        --argjson clarity "$(stat -c %s "$state_dir/chainstate/clarity.sqlite")" \
        --argjson staging "$(stat -c %s "$state_dir/chainstate/staging.sqlite")" \
        --argjson archive "$(stat -c %s "$state_dir/chainstate/archive.sqlite")" \
        '{marf: $marf, clarity: $clarity, staging: $staging, archive: $archive}')"
    selected_metrics="$(printf '%s\n' "$metrics" | awk '
        /^nano_(block_refusals_total|peer_failovers_total|sync_rounds_unanswered_total|stackerdb_rounds_unanswered_total|pushed_blocks_(accepted|refused)_total|followed_stacks_height|selected_stacks_height|burn_height|last_sealed_timestamp_seconds|serving_peers|staged_blocks|relay_(offered|dropped)|queued_(blocks|proposals|stackerdb_chunks|transactions))([ {]|$)/ { print }
    ')"

    jq -cn \
        --arg timestamp "$(date -u +%FT%TZ)" \
        --argjson elapsed_seconds "$SECONDS" \
        --argjson pid "$current_pid" \
        --argjson info "$info" \
        --argjson sync "$sync" \
        --argjson bitcoin_tip "$bitcoin_tip" \
        --argjson memory_current "$memory_current" \
        --argjson memory_peak "$memory_peak" \
        --argjson open_files "$open_files" \
        --argjson disk_available "$disk_available" \
        --argjson db_sizes "$db_sizes" \
        --arg metrics "$selected_metrics" \
        '{
            type: "sample",
            timestamp: $timestamp,
            elapsed_seconds: $elapsed_seconds,
            pid: $pid,
            info: $info,
            sync: $sync,
            bitcoin_tip: $bitcoin_tip,
            resources: {
                memory_current: $memory_current,
                memory_peak: $memory_peak,
                open_files: $open_files,
                disk_available: $disk_available,
                database_bytes: $db_sizes
            },
            metrics: ($metrics | split("\n") | map(select(length > 0)))
        }' >> "$output"

    sleep "$interval_seconds"
done

jq -cn \
    --arg timestamp "$(date -u +%FT%TZ)" \
    --argjson elapsed_seconds "$SECONDS" \
    '{type: "complete", timestamp: $timestamp, elapsed_seconds: $elapsed_seconds}' >> "$output"
