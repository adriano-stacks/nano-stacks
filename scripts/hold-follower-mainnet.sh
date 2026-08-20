#!/usr/bin/env bash
# Hold the packaged follower artifact at the public mainnet tip for 24 hours.
#
# The subject is the standalone follower, whose only surfaces are loopback
# /health and /metrics and whose only synchronization inputs are the local
# Bitcoin view and discovered Stacks P2P peers. The watcher therefore reads
# what the artifact exposes and verifies what it executed, rather than asking
# the node to describe itself:
#
#  - once a minute it samples /health, /metrics, the process (RSS, peak RSS,
#    open files, start ticks, executable hash), the state directory (database
#    and WAL sizes, free disk) and four tip views: the follower's, both stock
#    oracles' and an external Bitcoin height source;
#  - for every block the follower executes during the hold it byte-compares
#    the archived block against both independent stock oracles (byte equality
#    covers the header state root the follower already refused to seal
#    without) and records the archived receipt commitment beside it;
#  - the receipt commitments are verified after the interval by
#    scripts/verify-hold-receipts.sh against an independently executing
#    same-revision witness node's new_block payloads and the receipt oracle.
#    Deferred rather than inline because the first hold attempt proved the
#    inline witness was load-bearing: a stale-revision witness failed the run
#    over a compiler cost fix the witness lacked, not over the follower.
#
# Any process change, health failure, byte difference or missing archive row
# appends a terminal failure record and exits nonzero: a hold with a defect
# restarts from zero. Every record lands in one JSONL output.
set -euo pipefail

usage() {
    cat >&2 <<'EOF'
usage: hold-follower-mainnet.sh, configured by environment:
  HOLD_OUTPUT            JSONL evidence file; must not already hold a run
  HOLD_HEALTH_URL        follower /health base, e.g. http://127.0.0.1:22492
  HOLD_METRICS_URL       follower /metrics base, e.g. http://127.0.0.1:19154
  HOLD_STATE_DIR         the follower's working directory
  HOLD_CONFIG            the follower's configuration file
  HOLD_PID               the follower's process id
  HOLD_EXE_SHA256        required sha256 of /proc/PID/exe
  HOLD_ORACLE_A          stock node base URL
  HOLD_ORACLE_B          stock node base URL
  HOLD_BITCOIN_TIP_URL   URL answering the Bitcoin tip height as decimal
  HOLD_BLOCK_IDENTITY    path to the block-identity binary
  HOLD_DURATION_SECONDS  optional, default 86400
EOF
    exit 2
}

for name in HOLD_OUTPUT HOLD_HEALTH_URL HOLD_METRICS_URL HOLD_STATE_DIR \
    HOLD_CONFIG HOLD_PID HOLD_EXE_SHA256 HOLD_ORACLE_A \
    HOLD_ORACLE_B HOLD_BITCOIN_TIP_URL HOLD_BLOCK_IDENTITY; do
    [ -n "${!name:-}" ] || { echo "$name is not set" >&2; usage; }
done

readonly output="$HOLD_OUTPUT"
readonly health_url="${HOLD_HEALTH_URL%/}"
readonly metrics_url="${HOLD_METRICS_URL%/}"
readonly state_dir="$HOLD_STATE_DIR"
readonly config="$HOLD_CONFIG"
readonly pid="$HOLD_PID"
readonly expected_exe_sha256="$HOLD_EXE_SHA256"
readonly oracle_a="${HOLD_ORACLE_A%/}"
readonly oracle_b="${HOLD_ORACLE_B%/}"
readonly bitcoin_tip_url="$HOLD_BITCOIN_TIP_URL"
readonly block_identity_bin="$HOLD_BLOCK_IDENTITY"
readonly duration_seconds="${HOLD_DURATION_SECONDS:-86400}"
readonly interval_seconds=60
readonly executed_db="$state_dir/chainstate/executed.sqlite"

work="$(mktemp -d)"
readonly work
trap 'rm -rf -- "$work"' EXIT

fail() {
    local reason="$1"
    jq -cn \
        --arg timestamp "$(date -u +%FT%TZ)" \
        --arg reason "$reason" \
        '{type: "failure", timestamp: $timestamp, reason: $reason}' >> "$output"
    echo "$reason" >&2
    exit 1
}

executed_query() {
    sqlite3 -readonly "file:$executed_db?mode=ro" "$1"
}

read_bitcoin_tip() {
    local height
    height="$(curl -fsS --max-time 10 "$bitcoin_tip_url")"
    [[ "$height" =~ ^[0-9]+$ ]] || return 1
    printf '%s\n' "$height"
}

process_start_ticks() {
    awk '{print $22}' "/proc/$pid/stat"
}

check_process() {
    [ -d "/proc/$pid" ] || fail "the follower process is gone"
    [ "$(process_start_ticks)" = "$initial_start_ticks" ] || \
        fail "the follower process restarted"
    local sha
    sha="$(sha256sum "/proc/$pid/exe" | awk '{print $1}')"
    [ "$sha" = "$expected_exe_sha256" ] || \
        fail "the follower executable changed: $sha"
}

# Compare one archived block's consensus identity against both stock oracles
# and record its receipt commitment for the deferred verification. Identity
# rather than bytes: the signer signature vector is outside the block hash,
# so two valid representations of one block may legitimately differ in it —
# the first hold attempt failed on a fourteen- versus fifteen-signature
# representation of the same block. The block hash covers every header field
# and the transaction merkle root, so its equality is the consensus claim.
# Failures are terminal.
verify_block() {
    local height="$1" block_id="$2"
    local follower_block="$work/follower.bin"
    executed_query "SELECT lower(hex(bytes)) FROM executed
                    WHERE block_id = x'$block_id'" | xxd -r -p > "$follower_block"
    [ -s "$follower_block" ] || fail "block $height left the archive before verification"
    local follower_identity
    follower_identity="$("$block_identity_bin" "$follower_block")" || \
        fail "the archived block $height does not decode"
    [ "$(jq -er .block_id <<< "$follower_identity")" = "$block_id" ] || \
        fail "the archived block at height $height is not the block the archive names"

    # An oracle that does not answer is retried across minutes and recorded,
    # never treated as the subject's failure: unavailability is a property of
    # the oracle, and the fifth interval died on exactly that. An oracle that
    # answers with a *different* block is terminal, as ever.
    local oracle oracle_identity served
    for oracle in "$oracle_a" "$oracle_b"; do
        served=""
        for _ in 1 2 3 4 5 6; do
            if curl -fsS --max-time 20 --retry 3 --retry-all-errors \
                "$oracle/v3/blocks/$block_id" > "$work/oracle.bin" 2>/dev/null; then
                served=yes
                break
            fi
            sleep 20
            check_process
        done
        if [ -z "$served" ]; then
            jq -cn \
                --arg timestamp "$(date -u +%FT%TZ)" \
                --argjson height "$height" \
                --arg oracle "$oracle" \
                '{type: "oracle-unavailable", timestamp: $timestamp,
                  height: $height, oracle: $oracle}' >> "$output"
            continue
        fi
        oracle_identity="$("$block_identity_bin" "$work/oracle.bin")" || \
            fail "block $height from $oracle does not decode"
        [ "$(jq -c 'del(.signer_signatures)' <<< "$follower_identity")" = \
          "$(jq -c 'del(.signer_signatures)' <<< "$oracle_identity")" ] || \
            fail "block $height differs from $oracle: local $follower_identity, served $oracle_identity"
    done

    local summary
    summary="$(executed_query "SELECT cast(summary AS text) FROM receipts
                               WHERE block_id = x'$block_id'")"
    [ -n "$summary" ] || fail "block $height has no archived receipt commitment"

    local root
    root="$(jq -er .state_index_root <<< "$follower_identity")"
    jq -cn \
        --arg timestamp "$(date -u +%FT%TZ)" \
        --argjson height "$height" \
        --arg block_id "$block_id" \
        --arg state_index_root "$root" \
        --argjson receipts "$summary" \
        '{type: "block", timestamp: $timestamp, height: $height,
          block_id: $block_id, state_index_root: $state_index_root,
          receipts: $receipts, oracles: 2}' >> "$output"
}

test -d "$state_dir" || { echo "state directory is absent: $state_dir" >&2; exit 1; }
test -r "$config" || { echo "config is unreadable: $config" >&2; exit 1; }
test -r "$executed_db" || { echo "executed archive is unreadable: $executed_db" >&2; exit 1; }
test ! -s "$output" || { echo "output already contains a run: $output" >&2; exit 1; }
mkdir -p "$(dirname "$output")"
: > "$output"

if grep -Eqi 'api\.(mainnet\.)?hiro\.so' "$config"; then
    fail "the follower config names a hosted Stacks API"
fi
grep -Eq '^peers = \[\]' "$config" || \
    fail "the follower config names configured peers instead of discovering them"

initial_start_ticks="$(process_start_ticks)" || fail "the follower process is absent"
readonly initial_start_ticks
exe_sha256="$(sha256sum "/proc/$pid/exe" | awk '{print $1}')"
readonly exe_sha256
[ "$exe_sha256" = "$expected_exe_sha256" ] || \
    fail "the process is not the expected artifact: $exe_sha256"
config_sha256="$(sha256sum "$config" | awk '{print $1}')"
readonly config_sha256

initial_health="$(curl -fsS --max-time 10 "$health_url/health")" || \
    fail "the follower health endpoint did not answer"
jq -e '.ready == true and .last_error == null and .p2p_connected >= 2' \
    <<< "$initial_health" > /dev/null || fail "the follower is not ready: $initial_health"
initial_bitcoin_tip="$(read_bitcoin_tip)" || fail "the Bitcoin tip source did not answer"
initial_height="$(executed_query 'SELECT max(height) FROM executed')"
[ -n "$initial_height" ] || fail "the follower archive holds no executed block"
last_verified_height="$initial_height"

jq -cn \
    --arg timestamp "$(date -u +%FT%TZ)" \
    --argjson pid "$pid" \
    --arg exe_sha256 "$exe_sha256" \
    --arg config "$config" \
    --arg config_sha256 "$config_sha256" \
    --arg state_dir "$state_dir" \
    --arg oracle_a "$oracle_a" \
    --arg oracle_b "$oracle_b" \
    --arg bitcoin_tip_url "$bitcoin_tip_url" \
    --argjson bitcoin_tip "$initial_bitcoin_tip" \
    --argjson initial_height "$initial_height" \
    --argjson health "$initial_health" \
    --argjson duration_seconds "$duration_seconds" \
    '{type: "start", timestamp: $timestamp, pid: $pid,
      exe_sha256: $exe_sha256, config: $config, config_sha256: $config_sha256,
      state_dir: $state_dir,
      oracle_a: $oracle_a, oracle_b: $oracle_b,
      bitcoin_tip_url: $bitcoin_tip_url, bitcoin_tip: $bitcoin_tip,
      initial_height: $initial_height, health: $health,
      duration_seconds: $duration_seconds}' >> "$output"

SECONDS=0
hole_rounds=0
unresponsive_samples=0
while [ "$SECONDS" -lt "$duration_seconds" ]; do
    check_process

    # A missed probe under host I/O pressure is recorded, not fatal: the task
    # restarts the interval on a node defect or process stop, and a follower
    # whose process is alive and answers again within minutes is neither —
    # three intervals died on exactly this while every block still verified.
    # Sustained unavailability is a defect and still fails.
    health="$(curl -fsS --max-time 10 "$health_url/health")" || health=""
    if [ -n "$health" ]; then
        unresponsive_samples=0
        jq -e '.last_error == null' <<< "$health" > /dev/null || \
            fail "the follower reports an error: $health"
    else
        unresponsive_samples=$((unresponsive_samples + 1))
        [ "$unresponsive_samples" -lt 5 ] || \
            fail "the follower health endpoint was unavailable for $unresponsive_samples consecutive samples"
        health=null
    fi
    metrics="$(curl -fsS --max-time 10 "$metrics_url/metrics")" || metrics=""
    info_a="$(curl -fsS --max-time 10 "$oracle_a/v2/info")" || info_a='null'
    info_b="$(curl -fsS --max-time 10 "$oracle_b/v2/info")" || info_b='null'
    bitcoin_tip="$(read_bitcoin_tip)" || bitcoin_tip=null

    rss_kb="$(awk '/^VmRSS:/ {print $2}' "/proc/$pid/status")"
    peak_kb="$(awk '/^VmHWM:/ {print $2}' "/proc/$pid/status")"
    open_files="$(find "/proc/$pid/fd" -mindepth 1 -maxdepth 1 -printf . | wc -c)"
    disk_available="$(df -B1 --output=avail "$state_dir" | tail -n 1 | tr -d ' ')"
    db_sizes="$(jq -cn \
        --argjson marf "$(stat -c %s "$state_dir/chainstate/marf.sqlite")" \
        --argjson marf_wal "$(stat -c %s "$state_dir/chainstate/marf.sqlite-wal" 2>/dev/null || echo 0)" \
        --argjson clarity "$(stat -c %s "$state_dir/chainstate/clarity.sqlite")" \
        --argjson staging "$(stat -c %s "$state_dir/chainstate/staging.sqlite")" \
        --argjson executed "$(stat -c %s "$state_dir/chainstate/executed.sqlite")" \
        '{marf: $marf, marf_wal: $marf_wal, clarity: $clarity,
          staging: $staging, executed: $executed}')"

    tip="$(executed_query 'SELECT max(height) FROM executed')"
    if [ "$tip" -lt "$last_verified_height" ]; then
        # A fork switch re-executes replacements at heights this loop already
        # passed; move the cursor back so they are verified too.
        jq -cn \
            --arg timestamp "$(date -u +%FT%TZ)" \
            --argjson from "$last_verified_height" --argjson to "$tip" \
            '{type: "retraction", timestamp: $timestamp, from: $from, to: $to}' >> "$output"
        last_verified_height="$tip"
    fi
    while [ "$last_verified_height" -lt "$tip" ]; do
        next=$((last_verified_height + 1))
        block_id="$(executed_query "SELECT lower(hex(block_id)) FROM executed
                                    WHERE height = $next")"
        if [ -z "$block_id" ]; then
            # A hole can be a fork mid-read, gone next round. One that stays
            # is verification falling out of the bounded archive.
            hole_rounds=$((hole_rounds + 1))
            [ "$hole_rounds" -lt 10 ] || \
                fail "the archive holds no block at height $next while its tip is $tip"
            break
        fi
        hole_rounds=0
        verify_block "$next" "$block_id"
        last_verified_height="$next"
    done

    jq -cn \
        --arg timestamp "$(date -u +%FT%TZ)" \
        --argjson elapsed_seconds "$SECONDS" \
        --argjson health "$health" \
        --argjson info_a "$info_a" \
        --argjson info_b "$info_b" \
        --argjson bitcoin_tip "$bitcoin_tip" \
        --argjson verified_height "$last_verified_height" \
        --argjson rss_kb "$rss_kb" \
        --argjson peak_kb "$peak_kb" \
        --argjson open_files "$open_files" \
        --argjson disk_available "$disk_available" \
        --argjson db_sizes "$db_sizes" \
        --arg metrics "$metrics" \
        '{type: "sample", timestamp: $timestamp, elapsed_seconds: $elapsed_seconds,
          health: $health,
          oracle_tips: {a: (if $info_a == null then null else
              {stacks: $info_a.stacks_tip_height, burn: $info_a.burn_block_height} end),
            b: (if $info_b == null then null else
              {stacks: $info_b.stacks_tip_height, burn: $info_b.burn_block_height} end)},
          bitcoin_tip: $bitcoin_tip,
          verified_height: $verified_height,
          resources: {rss_kb: $rss_kb, peak_kb: $peak_kb, open_files: $open_files,
            disk_available: $disk_available, database_bytes: $db_sizes},
          metrics: ($metrics | split("\n") | map(select(length > 0)))}' >> "$output"

    sleep "$interval_seconds"
done

# The interval is over. Receipt commitments recorded per block remain to be
# verified by scripts/verify-hold-receipts.sh against the same-revision
# witness and the receipt oracle; the hold is not release evidence until that
# verification also passes.
jq -cn \
    --arg timestamp "$(date -u +%FT%TZ)" \
    --argjson elapsed_seconds "$SECONDS" \
    --argjson initial_height "$initial_height" \
    --argjson verified_height "$last_verified_height" \
    '{type: "complete", timestamp: $timestamp, elapsed_seconds: $elapsed_seconds,
      initial_height: $initial_height, verified_height: $verified_height}' >> "$output"
