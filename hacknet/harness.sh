#!/usr/bin/env bash
# Run a Hacknet epoch-4 network with one stock participant replaced by nano.
#
# A Hacknet participant is a stacks-node and the stacks-signer it feeds, so a
# replacement takes over both: the signer holds a stacked key the reward set
# needs, and the node commits on Bitcoin for tenures. Every step is a command
# here so a run can be repeated, inspected, and interrupted at any stage.
#
# Compose is driven directly rather than through Hacknet's Makefile, whose
# Linux path assumes rootful Docker: it removes chainstate with sudo and
# extracts archives with sudo tar. The commands below mirror `make build`,
# `make genesis`, `make down` and `make stop/start` one for one.
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

HACKNET_REMOTE=${HACKNET_REMOTE:-https://github.com/stacks-network/hacknet.git}
# Hacknet commit the compatibility patches in this directory apply to.
HACKNET_COMMIT=${HACKNET_COMMIT:-bf821e9d556eab8c7a30c6e86a7dc1f9b200f1a1}
HOME_DIR=${NANO_HACKNET_HOME:-$HOME/.cache/nano-stacks/hacknet}
SRC=$HOME_DIR/src
RUN=$HOME_DIR/run
PROJECT=${NANO_HACKNET_PROJECT:-hacknet}

# Host endpoints Hacknet publishes.
BITCOIN_RPC=${BITCOIN_RPC:-http://127.0.0.1:18443}
# Bitcoin height at which the bitcoin-miner stops producing blocks.
PAUSE_HEIGHT=${PAUSE_HEIGHT:-999999999999}
# Seconds between Bitcoin blocks once Nakamoto is active.
MINE_INTERVAL_EPOCH3=${MINE_INTERVAL_EPOCH3:-10}
# Seconds a frozen Stacks tip is tolerated while Bitcoin keeps advancing.
STALL_SECS=${STALL_SECS:-240}

log() { printf '\n== %s\n' "$*" >&2; }
die() { printf 'harness: %s\n' "$*" >&2; exit 1; }

# The node RPC endpoint Hacknet publishes for one miner index.
peer_url() {
    case ${1:?miner index} in
    1) echo "http://127.0.0.1:20443" ;;
    2) echo "http://127.0.0.1:21443" ;;
    3) echo "http://127.0.0.1:22443" ;;
    *) die "no such participant: $1" ;;
    esac
}

chainstate_dir() { echo "$SRC/docker/chainstate/genesis"; }

# Run docker compose against the project, from the checkout it belongs to.
compose() {
    (cd "$SRC" && CHAINSTATE_DIR="$(chainstate_dir)" \
        PAUSE_HEIGHT="$PAUSE_HEIGHT" MINE_INTERVAL_EPOCH3="$MINE_INTERVAL_EPOCH3" \
        docker compose -f docker/docker-compose.yml --profile default -p "$PROJECT" "$@")
}

# Read a value the compose file hardcodes, so the harness never restates it.
compose_value() {
    python3 - "$SRC/docker/docker-compose.yml" "${1:?key}" "${2:-}" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
key, service = sys.argv[2], sys.argv[3]
if service:
    text = text[text.index(f"\n  {service}:\n"):]
match = re.search(rf"^\s*(?:- &{re.escape(key)}|{re.escape(key)}:)\s*(\S+)", text, re.M)
if not match:
    raise SystemExit(f"{key} is not set in the compose file")
print(re.sub(r"^\$\{[A-Za-z0-9_]+:-|\}$", "", match.group(1)))
PY
}

# Read a value out of the checkpoint the export step wrote.
checkpoint_value() {
    python3 - "$RUN/checkpoint/checkpoint.toml" "${1:?key}" <<'PY'
import sys
wanted = sys.argv[2]
for line in open(sys.argv[1]):
    key, _, value = line.partition("=")
    if key.strip() == wanted:
        print(value.strip().strip('"'))
        break
else:
    raise SystemExit(f"{wanted} is not in the checkpoint")
PY
}

peer_info() {
    curl -sf --max-time 5 "${1:?peer}/v2/info" |
        python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["burn_block_height"], d["stacks_tip_height"], d.get("tenure_height", 0))'
}

need_source() { [ -d "$SRC/docker" ] || die "run 'harness.sh setup' first"; }

running() { docker compose ls --filter "name=$PROJECT" -q | grep -qx "$PROJECT"; }

# --- stages ------------------------------------------------------------------

# Clone Hacknet at the pinned commit and apply the compatibility patches.
setup() {
    mkdir -p "$HOME_DIR" "$RUN"
    if [ ! -d "$SRC/.git" ]; then
        log "cloning Hacknet into $SRC"
        git clone --quiet "$HACKNET_REMOTE" "$SRC"
    fi
    git -C "$SRC" fetch --quiet origin "$HACKNET_COMMIT" 2>/dev/null ||
        git -C "$SRC" fetch --quiet origin
    git -C "$SRC" -c advice.detachedHead=false checkout --quiet --force "$HACKNET_COMMIT"
    git -C "$SRC" clean --quiet -fd -e docker/chainstate
    log "applying the nano compatibility patches"
    for patch in "$ROOT"/hacknet/hacknet-main.patch "$ROOT"/hacknet/hacknet-api-main.patch; do
        git -C "$SRC" apply --unidiff-zero "$patch"
        printf '  applied %s\n' "$(basename "$patch")" >&2
    done
}

# Boot from genesis, mirroring `make genesis`.
up() {
    need_source
    ! running || die "project $PROJECT is already up; run 'harness.sh down' first"
    wipe
    mkdir -p "$(chainstate_dir)"
    chainstate_dir > "$SRC/.current-chainstate-dir"
    log "building the Hacknet images"
    COMPOSE_BAKE=true compose build
    log "starting $PROJECT from genesis"
    compose up -d
}

down() {
    need_source
    log "stopping $PROJECT"
    compose down --remove-orphans
}

# Remove the chainstate from inside a container: Docker writes it with
# container-side ownership the host user cannot unlink directly.
wipe() {
    local dir
    dir=$(chainstate_dir)
    [ -d "$dir" ] || return 0
    log "removing the previous chainstate"
    docker run --rm -v "$dir:/chainstate" "$(compose_value IMAGE_BITCOIN)" \
        find /chainstate -mindepth 1 -delete
    rmdir "$dir"
}

status() {
    need_source
    local index
    for index in 1 2 3; do
        printf 'participant %s: %s\n' "$index" \
            "$(peer_info "$(peer_url "$index")" 2>/dev/null || echo "unreachable")"
    done
    python3 - "$(curl -sf --max-time 5 "$(peer_url 1)/v2/pox" || echo '{}')" <<'PY' || true
import json, sys
pox = json.loads(sys.argv[1])
cycle, following = pox["current_cycle"], pox["next_cycle"]
print(
    "pox {}: cycle {} active={}, cycle {} starts at burn {}".format(
        pox["contract_id"],
        cycle["id"],
        cycle["is_pox_active"],
        following["id"],
        following["reward_phase_start_block_height"],
    )
)
PY
    printf 'nano: %s\n' "$(nano_state)"
}

nano_state() {
    local pid_file=$RUN/nano-signer.pid
    if [ -f "$pid_file" ] && kill -0 "$(cat "$pid_file")" 2>/dev/null; then
        echo "signer for participant $(cat "$RUN/replaced-participant") running as pid $(cat "$pid_file")"
    else
        echo "not running"
    fi
}

# Wait for the Bitcoin chain to reach a height while Stacks keeps advancing.
#
# A Hacknet stall shows up as Bitcoin blocks arriving with a frozen Stacks tip,
# which is exactly what a replaced participant must not cause, so the wait
# fails loudly instead of timing out silently.
wait_for() {
    local target=${1:?target Bitcoin height} timeout=${2:-2400} peer
    peer=$(peer_url 1)
    local deadline=$((SECONDS + timeout)) highest=0 progress_at=$SECONDS
    local burn=0 stacks=0 tenure=0
    while [ "$SECONDS" -lt "$deadline" ]; do
        read -r burn stacks tenure < <(peer_info "$peer" 2>/dev/null || echo "0 0 0")
        if [ "$stacks" -gt "$highest" ]; then
            highest=$stacks
            progress_at=$SECONDS
        elif [ $((SECONDS - progress_at)) -gt "$STALL_SECS" ]; then
            die "stalled: burn $burn, Stacks tip frozen at $stacks for ${STALL_SECS}s"
        fi
        if [ "$burn" -ge "$target" ]; then
            printf 'reached burn %s with Stacks tip %s in tenure %s\n' "$burn" "$stacks" "$tenure" >&2
            return 0
        fi
        sleep 10
    done
    die "burn height $target not reached within ${timeout}s (burn $burn, stacks $stacks)"
}

# Export the state a nano participant validates from.
checkpoint() {
    need_source
    log "exporting a checkpoint from participant 1"
    rm -rf "$RUN/checkpoint"
    STATE_DIR="$(chainstate_dir)" OUT="$RUN/checkpoint" PEER="$(peer_url 1)" \
        "$ROOT/hacknet/signer-checkpoint.sh"
    compose_value BITCOIN_RPC_PASS > "$RUN/bitcoin-rpc.pass"
}

# Replace one stock participant: stop its node and its signer, then sign with
# the key that participant staked.
replace() {
    need_source
    local index=${1:?participant index}
    [ -f "$RUN/checkpoint/checkpoint.toml" ] || die "run 'harness.sh checkpoint' first"
    [ "$(nano_state)" = "not running" ] || die "a nano participant is already running"

    local key
    key=$(compose_value SIGNER_PRIVATE_KEY "stacks-signer-$index")
    log "stopping stock participant $index: stacks-miner-$index and stacks-signer-$index"
    compose stop "stacks-miner-$index" "stacks-signer-$index"

    # nano reads the chain from a participant it did not replace: the one it
    # took over no longer serves anything.
    local follow
    follow=$(peer_url "$([ "$index" = 1 ] && echo 2 || echo 1)")
    log "starting the nano signer for participant $index against $follow"
    echo "$index" > "$RUN/replaced-participant"
    "$ROOT/target/debug/stacks-signer" run \
        --peer "$follow/" \
        --bitcoin-rpc "$BITCOIN_RPC" \
        --bitcoin-rpc-user "$(compose_value BITCOIN_RPC_USER)" \
        --bitcoin-rpc-password-file "$RUN/bitcoin-rpc.pass" \
        --miner-contract "ST000000000000000000002AMW42H/miners" \
        --private-key "${key%01}" \
        --state-file "$RUN/signer.json" \
        --checkpoint "$RUN/checkpoint/marf.sqlite" \
        --tenure-accounting "$RUN/checkpoint/native-effects.json" \
        --source-state-id "$(checkpoint_value source_state_id)" \
        --state-root "$(checkpoint_value state_index_root)" \
        --anchor-block "$RUN/checkpoint/anchor-block.bin" \
        --anchor-bitcoin-height "$(checkpoint_value anchor_bitcoin_height)" \
        >> "$RUN/nano-signer.log" 2>&1 &
    echo $! > "$RUN/nano-signer.pid"
    printf 'nano signs for participant %s as pid %s, logging to %s\n' \
        "$index" "$(cat "$RUN/nano-signer.pid")" "$RUN/nano-signer.log" >&2
}

# Put the stock participant back and stop nano.
restore() {
    need_source
    local index
    index=$(cat "$RUN/replaced-participant" 2>/dev/null || echo "")
    [ -n "$index" ] || die "no participant is replaced"
    if [ -f "$RUN/nano-signer.pid" ]; then
        kill "$(cat "$RUN/nano-signer.pid")" 2>/dev/null || true
        rm -f "$RUN/nano-signer.pid"
    fi
    log "restoring stock participant $index"
    compose start "stacks-miner-$index" "stacks-signer-$index"
    rm -f "$RUN/replaced-participant"
}

# Deploy a contract and call it, using Hacknet's own transaction tooling.
traffic() {
    need_source
    local key
    key=$(sed -n 's/^BOOTSTRAPPER_KEY=//p' "$SRC/docker/stacker/stacking/tx-broadcaster.env")
    log "deploying a contract and calling it through the Hacknet broadcaster"
    compose exec -e NUM_FLOODERS=2 -e TX_PER_FLOOD=3 -e BOOTSTRAPPER_KEY="$key" \
        tx-broadcaster npx tsx /root/flood.ts
}

case ${1:-} in
setup) setup ;;
up) up ;;
down) down ;;
wipe) wipe ;;
status) status ;;
wait) shift && wait_for "$@" ;;
checkpoint) checkpoint ;;
replace) shift && replace "$@" ;;
restore) restore ;;
traffic) traffic ;;
*)
    cat >&2 <<'USAGE'
usage: harness.sh <command>

  setup              clone Hacknet at the pinned commit and patch it
  up                 build and boot the network from genesis
  wait <height> [s]  wait for a Bitcoin height, failing on a Stacks stall
  checkpoint         export the state a nano participant validates from
  replace <1|2|3>    stop one stock participant and run nano in its place
  traffic            deploy a contract and call it
  status             heights, reward cycle, and nano state
  restore            put the stock participant back
  down               stop the network
  wipe               delete the chainstate
USAGE
    exit 2
    ;;
esac
