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
# Hacknet otherwise locks for one cycle and can miss its only renewal window.
# Twelve is the maximum accepted by pox-4 and remains valid under pox-5.
STACKING_CYCLES=${STACKING_CYCLES:-12}
# Keep Hacknet's epoch defaults, but pass them explicitly so an anchor-window
# diagnostic can move Nakamoto earlier without editing the upstream checkout.
STACKS_30_HEIGHT=${STACKS_30_HEIGHT:-232}
STACKS_31_HEIGHT=${STACKS_31_HEIGHT:-233}
STACKS_32_HEIGHT=${STACKS_32_HEIGHT:-234}
STACKS_33_HEIGHT=${STACKS_33_HEIGHT:-235}
# Where the node binaries are. Release by default: this node executes every
# block it is given, and a debug build is too slow to keep up with a tenure.
BIN=${NANO_BIN_DIR:-$ROOT/target/release}

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

# A participant index nano did not replace, to read the chain from.
stock_index() { case ${1:?index} in 1) echo 2 ;; *) echo 1 ;; esac; }

# Run docker compose against the project, from the checkout it belongs to.
compose() {
    (cd "$SRC" && CHAINSTATE_DIR="$(chainstate_dir)" \
        PAUSE_HEIGHT="$PAUSE_HEIGHT" MINE_INTERVAL_EPOCH3="$MINE_INTERVAL_EPOCH3" \
        STACKING_CYCLES="$STACKING_CYCLES" \
        STACKS_30_HEIGHT="$STACKS_30_HEIGHT" STACKS_31_HEIGHT="$STACKS_31_HEIGHT" \
        STACKS_32_HEIGHT="$STACKS_32_HEIGHT" STACKS_33_HEIGHT="$STACKS_33_HEIGHT" \
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
    (stacking) || true
    printf 'nano: %s\n' "$(nano_state)"
}

# Report how many future pox-5 cycles already contain at least one signer.
stacking() {
    local peer cycle contract horizon=0 probe
    peer=$(peer_url "${1:-1}")
    read -r cycle contract < <(pox_cycle "$peer") ||
        die "cannot read the reward cycle from $peer"
    case $contract in
    *.pox-5) ;;
    *)
        printf 'stacking: %s is active, not pox-5; nothing to measure yet\n' "$contract"
        return 0
        ;;
    esac
    while [ "$horizon" -lt 96 ]; do
        probe=$((cycle + horizon + 1))
        pox5_has_signers "$peer" "$probe" || break
        horizon=$((horizon + 1))
    done
    if [ "$horizon" -gt 0 ]; then
        printf 'stacking: cycles %s..%s have a pox-5 signer set (%s ahead of cycle %s)\n' \
            "$((cycle + 1))" "$((cycle + horizon))" "$horizon" "$cycle"
    else
        printf 'stacking: no future pox-5 signer set after cycle %s\n' "$cycle"
    fi
    [ "$horizon" -ge 2 ] ||
        die "only $horizon cycle(s) stacked ahead of $cycle: without renewal the chain stops at cycle $((cycle + horizon + 1))"
}

pox_cycle() {
    curl -sf --max-time 5 "${1:?peer}/v2/pox" 2>/dev/null |
        python3 -c 'import json,sys
pox = json.load(sys.stdin)
print(pox["current_cycle"]["id"], pox["contract_id"])' 2>/dev/null
}

pox5_has_signers() {
    local peer=${1:?peer} cycle=${2:?cycle} argument
    argument=$(printf '0x01%032x' "$cycle")
    curl -sf --max-time 10 -X POST -H 'content-type: application/json' \
        -d "{\"sender\":\"ST000000000000000000002AMW42H\",\"arguments\":[\"$argument\"]}" \
        "$peer/v2/contracts/call-read/ST000000000000000000002AMW42H/pox-5/get-signer-set-first-item-for-cycle" |
        python3 -c 'import json,sys
result = json.load(sys.stdin).get("result", "")
raise SystemExit(0 if result.startswith("0x0a") else 1)'
}

nano_state() {
    nano_running || { echo "not running"; return 0; }
    local roles=()
    grep -q '^\[signer\]' "$RUN/nano.toml" && roles+=(signing)
    grep -q '^\[miner\]' "$RUN/nano.toml" && roles+=(mining)
    grep -q '^rpc_bind' "$RUN/nano.toml" && roles+=("serving an RPC")
    printf 'node %s for participant %s as pid %s\n' \
        "$(IFS=,; echo "${roles[*]:-following only}")" \
        "$(cat "$RUN/replaced-participant")" "$(cat "$RUN/nano.pid")"
}

nano_running() {
    [ -f "$RUN/nano.pid" ] && kill -0 "$(cat "$RUN/nano.pid")" 2>/dev/null
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

# Follow the stock chain across reward-cycle boundaries and fail if the new
# cycle has no usable signer set or the Stacks tip stops advancing.
cross_cycles() {
    local count=${1:-2} timeout=${2:-3600} peer cycle current burn stacks tenure
    peer=$(peer_url 1)
    local deadline=$((SECONDS + timeout)) highest=0 progress_at=$SECONDS crossed=0
    read -r cycle _ < <(pox_cycle "$peer") ||
        die "cannot read the reward cycle from $peer"
    log "crossing $count reward-cycle boundaries from cycle $cycle"
    stacking
    while [ "$crossed" -lt "$count" ]; do
        [ "$SECONDS" -lt "$deadline" ] ||
            die "only crossed $crossed of $count cycles in ${timeout}s"
        read -r burn stacks tenure < <(peer_info "$peer" 2>/dev/null || echo "0 0 0")
        if [ "$stacks" -gt "$highest" ]; then
            highest=$stacks
            progress_at=$SECONDS
        elif [ $((SECONDS - progress_at)) -gt "$STALL_SECS" ]; then
            die "stalled at burn $burn, Stacks tip frozen at $stacks for ${STALL_SECS}s"
        fi
        read -r current _ < <(pox_cycle "$peer" || echo "$cycle -")
        if [ "$current" -gt "$cycle" ]; then
            cycle=$current
            crossed=$((crossed + 1))
            printf 'crossed into cycle %s at burn %s, Stacks tip %s, tenure %s: %s\n' \
                "$cycle" "$burn" "$stacks" "$tenure" \
                "$(reward_set_summary "$peer" "$cycle")" >&2
            stacking
        fi
        sleep 5
    done
}

reward_set_summary() {
    curl -sf --max-time 10 "${1:?peer}/v3/stacker_set/${2:?cycle}" |
        python3 -c '
import json, sys
signers = json.load(sys.stdin)["stacker_set"]["signers"] or []
if not signers:
    raise SystemExit("empty reward set")
print("{} signers, total weight {}".format(
    len(signers), sum(s["weight"] for s in signers)
))
' || die "cycle ${2} has no usable reward set"
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

# Write the one configuration file the nano node starts from.
#
# Every value it needs is already known here — the chain the peers report, the
# state the checkpoint exported, the keys the run generated — so the node takes
# a file rather than a command line, and a restart takes the same file.
nano_config() {
    local index=${1:?participant index} peer chain_id key
    peer=$(peer_url "$(stock_index "$index")")
    chain_id=$(curl -sf "$peer/v2/info" |
        python3 -c 'import json,sys; print(json.load(sys.stdin)["network_id"])')
    key=$(compose_value SIGNER_PRIVATE_KEY "stacks-signer-$index")
    {
        cat <<EOF
[node]
working_dir = "$RUN/nano"
network = "testnet"
chain_id = $chain_id
peers = ["$peer/"]
${NANO_RPC_BIND:+rpc_bind = \"$NANO_RPC_BIND\"}
${NANO_PROPOSAL_TOKEN:+block_proposal_token = \"$NANO_PROPOSAL_TOKEN\"}
event_observers = [${NANO_EVENT_OBSERVERS:+\"${NANO_EVENT_OBSERVERS//,/\", \"}\"}]

[burnchain]
rpc_url = "$BITCOIN_RPC"
rpc_user = "$(compose_value BITCOIN_RPC_USER)"
rpc_password = "$(compose_value BITCOIN_RPC_PASS)"
magic = "${NANO_BITCOIN_MAGIC:-T3}"

[checkpoint]
marf = "$RUN/checkpoint/marf.sqlite"
source_state_id = "$(checkpoint_value source_state_id)"
state_root = "$(checkpoint_value published_state_index_root)"
anchor_block = "$RUN/checkpoint/anchor-block.bin"
anchor_bitcoin_height = $(checkpoint_value first_bitcoin_height)
tenure_accounting = "$RUN/checkpoint/native-effects.json"
attesting_block = "$RUN/checkpoint/checkpoint-block.bin"
attesting_reward_set = "$RUN/checkpoint/reward-set.json"
sortition = "$RUN/checkpoint/sortition"

EOF
        # The signer half is a stock stacks-signer when this node hosts one, so
        # the node runs none of its own: one validator, one chain state.
        if [ -z "${NANO_HOSTED_SIGNER:-}" ]; then
            cat <<EOF

[signer]
private_key = "$key"
EOF
        fi
        # The miner half only exists once its Bitcoin identity does, and a node
        # hosting somebody else's signer is not competing for tenures.
        if [ -s "$RUN/leader-key.txt" ] && [ -z "${NANO_HOSTED_SIGNER:-}" ]; then
            cat <<EOF

[miner]
bitcoin_wallet = "${NANO_BITCOIN_WALLET:-nano-miner}"
key_txid = "$(grep -oE '[0-9a-f]{64}' "$RUN/leader-key.txt" | head -1)"
block_signing_private_key = "$(cat "$RUN/miner-signing.key")"
vrf_private_key = "$(cat "$RUN/miner-vrf.key")"
commitment_sats = ${NANO_COMMITMENT_SATS:-20000}
EOF
        fi
    } > "$RUN/nano.toml"
}

# Start the node from the configuration, replacing whatever is running.
nano_start() {
    local index=${1:?participant index}
    nano_stop
    nano_config "$index"
    "$BIN/stacks-node" start --config "$RUN/nano.toml" \
        >> "$RUN/nano.log" 2>&1 &
    echo $! > "$RUN/nano.pid"
    printf 'nano runs as pid %s, logging to %s\n' "$(cat "$RUN/nano.pid")" "$RUN/nano.log" >&2
}

# Stop the node the way an operator would, and wait for it to go.
nano_stop() {
    nano_running || return 0
    local pid
    pid=$(cat "$RUN/nano.pid")
    log "stopping the nano node (pid $pid)"
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 30); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 1
    done
    kill -9 "$pid" 2>/dev/null || true
    rm -f "$RUN/nano.pid"
}

# Replace one stock participant: stop its node and its signer, then sign with
# the key that participant staked.
replace() {
    need_source
    local index=${1:?participant index}
    [ -f "$RUN/checkpoint/checkpoint.toml" ] || die "run 'harness.sh checkpoint' first"
    [ "$(nano_state)" = "not running" ] || die "a nano participant is already running"

    log "stopping stock participant $index: stacks-miner-$index and stacks-signer-$index"
    compose stop "stacks-miner-$index" "stacks-signer-$index"

    # nano reads the chain from a participant it did not replace: the one it
    # took over no longer serves anything.
    log "starting the nano node for participant $index against $(peer_url "$(stock_index "$index")")"
    echo "$index" > "$RUN/replaced-participant"
    nano_start "$index"
}

# Replace one participant with nano as the *node* half only, and let a stock
# stacks-signer be the signer half against nano's RPC.
#
# `replace` proves nano can sign for a network. This proves the other direction,
# which is the harder one: a stock signer has no chain state and no peers, so
# every proposal it sees, every verdict it acts on and every chunk it writes goes
# through nano's RPC. The network still needs all three signatures, so a chain
# that keeps advancing is the stock signer having done all of that through nano.
#
# The signer runs on the host network because a Hacknet container cannot reach a
# host port through the bridge here, and nano is a host process.
host() {
    need_source
    local index=${1:?participant index}
    local port=${NANO_HOST_PORT:-24443} endpoint=${NANO_HOSTED_SIGNER_PORT:-30003}
    local sink=${NANO_SINK_PORT:-3801} key token image
    [ -f "$RUN/checkpoint/checkpoint.toml" ] || die "run 'harness.sh checkpoint' first"
    [ "$(nano_state)" = "not running" ] || die "a nano participant is already running"
    key=$(compose_value SIGNER_PRIVATE_KEY "stacks-signer-$index")
    token=$(sed -n 's/^auth_password = "\(.*\)"$/\1/p' "$SRC/docker/stacks/stacks-signer.toml")
    [ -n "$token" ] || die "the signer template sets no auth_password"

    # nano comes up *before* the participant it replaces goes down, and the
    # participant only goes down once nano is answering. Hacknet needs all three
    # signatures, so a signer missing for a whole prepare phase leaves the cycle
    # after it with no PoX anchor block and the chain never recovers — which is
    # what every failed start of this command used to cost.
    echo "$index" > "$RUN/replaced-participant"
    # nano's own events, recorded beside the stock nodes' so the two can be read
    # against each other for the same blocks.
    nano_sink "$sink"
    log "starting the nano node for participant $index, hosting a signer on :$endpoint"
    NANO_HOSTED_SIGNER=1 \
    NANO_RPC_BIND="0.0.0.0:$port" \
    NANO_PROPOSAL_TOKEN="$token" \
    NANO_EVENT_OBSERVERS="http://127.0.0.1:$endpoint,http://127.0.0.1:$sink" \
        nano_start "$index"
    nano_ready "$port" || { nano_stop; die "nano did not come up; nothing was stopped"; }

    log "stopping stock participant $index: stacks-miner-$index and stacks-signer-$index"
    compose stop "stacks-miner-$index" "stacks-signer-$index"

    image=$(docker inspect --format '{{.Config.Image}}' "stacks-signer-$index")
    docker rm -f "$HOSTED_SIGNER" > /dev/null 2>&1 || true
    mkdir -p "$RUN/hosted-signer"
    log "starting a stock stacks-signer against nano at 127.0.0.1:$port"
    docker run -d --name "$HOSTED_SIGNER" --network host \
        -v "$SRC/docker/stacks/stacks-signer.toml:/data/config.toml.in:ro" \
        -v "$RUN/hosted-signer:/data/signer" \
        -e "SIGNER_PRIVATE_KEY=$key" \
        -e "STACKS_NODE_HOST=127.0.0.1:$port" \
        -e "STACKS_SIGNER_ENDPOINT=0.0.0.0:$endpoint" \
        --entrypoint /bin/bash "$image" -c \
        'cd /data && perl -pe '\''s/\$\{?([A-Za-z_][A-Za-z0-9_]*)\}?/$ENV{$1}/ge'\'' \
            < config.toml.in > config.toml && exec stacks-signer run --config config.toml' \
        > /dev/null
    printf 'the stock signer runs as %s, logging to docker logs %s\n' \
        "$HOSTED_SIGNER" "$HOSTED_SIGNER" >&2
}

HOSTED_SIGNER=nano-hosted-signer

# Wait until nano can host a signer: a tip to serve, and the reward set derived.
#
# The reward set is the gate that matters. Until nano has walked pox-5 for the
# active cycle it configures no `signers-*` contract, so a signer pointed at it
# holds no slot and writes nowhere — and the node it replaced would already be
# stopped.
nano_ready() {
    local port=${1:?rpc port} deadline=$((SECONDS + ${NANO_READY_SECS:-600})) cycle
    while [ "$SECONDS" -lt "$deadline" ]; do
        nano_running || { printf 'nano exited: see %s\n' "$RUN/nano.log" >&2; return 1; }
        cycle=$(curl -sf --max-time 5 "http://127.0.0.1:$port/v2/pox" 2>/dev/null |
            python3 -c 'import json,sys;print(json.load(sys.stdin)["current_cycle"]["id"])' \
            2>/dev/null || true)
        if [ -n "$cycle" ] &&
            curl -sf --max-time 5 "http://127.0.0.1:$port/v3/stacker_set/$cycle" > /dev/null; then
            printf 'nano serves cycle %s and its signers\047 StackerDB contracts\n' "$cycle" >&2
            return 0
        fi
        sleep 5
    done
    printf 'nano did not derive a reward set within the wait\n' >&2
    return 1
}

# Record nano's own events on the host, where nano runs.
nano_sink() {
    local port=${1:-3801} out=$RUN/nano-events
    if [ -f "$RUN/nano-sink.pid" ] && kill -0 "$(cat "$RUN/nano-sink.pid")" 2>/dev/null; then
        log "nano's event sink is already recording into $out"
        return 0
    fi
    mkdir -p "$out"
    log "recording nano's events into $out"
    python3 "$ROOT/hacknet/event-sink.py" "$out" "$port" \
        >> "$RUN/nano-sink.log" 2>&1 &
    echo $! > "$RUN/nano-sink.pid"
}

# Stop the hosted signer and nano, and put the stock participant back.
unhost() {
    need_source
    docker rm -f "$HOSTED_SIGNER" > /dev/null 2>&1 || true
    [ -f "$RUN/nano-sink.pid" ] && kill "$(cat "$RUN/nano-sink.pid")" 2>/dev/null
    rm -f "$RUN/nano-sink.pid"
    restore
}

# Assert a stock signer, a submitter and an observer all work through nano.
verify_hosted() {
    need_source
    local index port=${NANO_HOST_PORT:-24443} key
    index=$(cat "$RUN/replaced-participant" 2>/dev/null || echo "")
    [ -n "$index" ] || die "no participant is hosted"
    [ -n "$(docker ps -q --filter "name=^${HOSTED_SIGNER}$")" ] ||
        die "the hosted stock signer is not running"
    key=$(compose_value SIGNER_PRIVATE_KEY "stacks-signer-$index")
    # The first account the broadcaster sends transfers from, which genesis funds.
    log "verifying the stock signer, a submitter and an observer against nano"
    NANO_HOSTED_RPC="http://127.0.0.1:$port/" \
    NANO_HOSTED_SIGNER_PUBLIC_KEY=$(cd "$ROOT" && cargo xtask public-key "${key%01}") \
    NANO_HACKNET_PEER="$(peer_url "$(stock_index "$index")")/" \
    NANO_EVENT_DIR="$RUN/nano-events" \
    NANO_STOCK_EVENT_DIR="$RUN/events" \
    NANO_FUNDED_KEY="$(compose_value ACCOUNT_KEYS tx-broadcaster | cut -d, -f1)" \
        cargo test --manifest-path "$ROOT/Cargo.toml" -p nano-conformance \
        --test conformance hosted_signer -- --ignored --nocapture --test-threads 1
}

# Assert the network keeps doing every kind of work while nano signs for it.
verify() {
    need_source
    local index key
    index=$(cat "$RUN/replaced-participant" 2>/dev/null || echo "")
    [ -n "$index" ] || die "no participant is replaced"
    [ "$(nano_state)" != "not running" ] || die "the nano participant is not running"
    key=$(compose_value SIGNER_PRIVATE_KEY "stacks-signer-$index")
    # The window has to contain a contract deploy and a call, so the traffic
    # runs alongside the assertions rather than before them.
    log "generating contract traffic for the verification window"
    traffic "${VERIFY_TRAFFIC_SECS:-600}" > "$RUN/traffic.log" 2>&1 &
    local traffic_pid=$!
    trap 'kill "$traffic_pid" 2>/dev/null || true' RETURN
    log "verifying the network with participant $index replaced"
    local miner_key=""
    [ -s "$RUN/miner-signing.key" ] && miner_key=$(cd "$ROOT" && cargo xtask public-key "$(cat "$RUN/miner-signing.key")")
    NANO_SIGNER_PUBLIC_KEY=$(cd "$ROOT" && cargo xtask public-key "${key%01}") \
    NANO_MINER_PUBLIC_KEY="$miner_key" \
    NANO_HACKNET_PEER="$(peer_url "$(stock_index "$index")")/" \
        cargo test --manifest-path "$ROOT/Cargo.toml" -p nano-conformance \
        --test hacknet_replacement -- --ignored --nocapture
}

# Give nano a Bitcoin identity: a wallet that holds keys, funded by the wallet
# Hacknet already uses for deposits, and a registered leader key.
#
# Hacknet's own miner wallets are watch-only, because a stock node signs its own
# burnchain transactions; nano funds and signs through the wallet, so it needs
# one of its own. It is deliberately not registered with the bitcoin-miner
# service, whose on-demand trigger sums confirmations across the wallets it
# watches: joining that sum would suppress block production for everyone else.
fund() {
    need_source
    local wallet=${NANO_BITCOIN_WALLET:-nano-miner} funding=${1:-100} address
    mkdir -p "$RUN"
    for key in miner-signing miner-vrf; do
        [ -s "$RUN/$key.key" ] || openssl rand -hex 32 > "$RUN/$key.key"
    done
    compose_value BITCOIN_RPC_PASS > "$RUN/bitcoin-rpc.pass"
    bitcoin -named createwallet "wallet_name=$wallet" descriptors=false > /dev/null 2>&1 || true
    address=$(bitcoin "-rpcwallet=$wallet" getnewaddress nano legacy)
    echo "$address" > "$RUN/miner-btc.addr"
    log "funding nano's Bitcoin wallet $wallet at $address with $funding"
    bitcoin -rpcwallet=depositor sendtoaddress "$address" "$funding" > /dev/null
}

bitcoin() {
    compose exec -T bitcoin bitcoin-cli \
        "-rpcuser=$(compose_value BITCOIN_RPC_USER)" \
        "-rpcpassword=$(compose_value BITCOIN_RPC_PASS)" "$@"
}

# Register the leader key that identifies nano's blocks, once per network.
register() {
    need_source
    [ -s "$RUN/miner-signing.key" ] || die "run 'harness.sh fund' first"
    local consensus_hash
    consensus_hash=$(curl -sf "$(peer_url 1)/v2/info" |
        python3 -c 'import json,sys; print(json.load(sys.stdin)["pox_consensus"])')
    log "registering nano's leader key against consensus hash $consensus_hash"
    "$BIN/stacks-register-leader-key" \
        --bitcoin-rpc "$BITCOIN_RPC/wallet/${NANO_BITCOIN_WALLET:-nano-miner}" \
        --bitcoin-rpc-user "$(compose_value BITCOIN_RPC_USER)" \
        --bitcoin-rpc-password-file "$RUN/bitcoin-rpc.pass" \
        --consensus-hash "$consensus_hash" \
        --vrf-private-key-file "$RUN/miner-vrf.key" \
        --block-signing-private-key-file "$RUN/miner-signing.key" |
        tee "$RUN/leader-key.txt"
}

# Turn the mining role on, which is a restart with a larger configuration.
#
# The restart is the point as much as the mining is: the node comes back to the
# state it left on disk instead of importing the checkpoint again.
mine() {
    need_source
    local index
    index=$(cat "$RUN/replaced-participant" 2>/dev/null || echo "")
    [ -n "$index" ] || die "run 'harness.sh replace' first"
    [ -s "$RUN/leader-key.txt" ] || die "run 'harness.sh register' first"
    grep -qE '[0-9a-f]{64}' "$RUN/leader-key.txt" ||
        die "could not read the leader-key transaction from $RUN/leader-key.txt"
    log "restarting nano with the mining role on"
    nano_start "$index"
}

# Put the stock participant back and stop nano.
# Record the receipts a capture needs, which Hacknet otherwise never writes.
#
# The node's only observer is its signer, and those keys carry no `new_block`,
# so a `cargo xtask capture-fixtures` run has no receipts to read. Adding an
# observer means re-rendering the node's config, which is a container restart —
# the chainstate survives it, the chain does not restart.
observe() {
    need_source
    local sink=$RUN/events port=${NANO_EVENT_PORT:-3800} name=nano-event-sink network
    network=$( (docker network inspect "$PROJECT"_default stacks --format \
        '{{.Name}}' 2>/dev/null || true) | head -1)
    [ -n "$network" ] || die "the Hacknet network is not up"

    # The sink runs on the Hacknet network rather than on the host: a node
    # cannot reach a host port through the bridge gateway here, and an
    # observer it cannot reach it retries forever.
    mkdir -p "$sink"
    if [ -n "$(docker ps -q --filter "name=^${name}$")" ]; then
        log "the event sink is already recording into $sink"
    else
        log "recording new_block events into $sink"
        docker rm -f "$name" >/dev/null 2>&1 || true
        docker run -d --name "$name" --network "$network" \
            -v "$ROOT/hacknet/event-sink.py:/sink.py:ro" -v "$sink:/events" \
            "${NANO_SINK_IMAGE:-python:3-slim}" python3 /sink.py /events "$port" >/dev/null
        sleep 2
    fi

    local template=$SRC/docker/stacks/stacks-miner_signer.toml
    if grep -q "nano event sink" "$template"; then
        log "the node template already posts to the sink"
    else
        log "adding the sink to the node template and restarting the miners"
        cat >> "$template" <<EOF

# nano event sink: the receipts a fixture capture reads.
[[events_observer]]
endpoint = "$name:$port"
events_keys = ["*"]
timeout_ms = 10_000
EOF
        compose restart stacks-miner-1 stacks-miner-2
    fi
    printf 'events land in %s\n' "$sink" >&2
}

stop_observing() {
    docker rm -f nano-event-sink >/dev/null 2>&1 || true
}

restore() {
    need_source
    local index
    index=$(cat "$RUN/replaced-participant" 2>/dev/null || echo "")
    [ -n "$index" ] || die "no participant is replaced"
    nano_stop
    log "restoring stock participant $index"
    compose start "stacks-miner-$index" "stacks-signer-$index"
    rm -f "$RUN/replaced-participant"
}

# Deploy a contract and call it, using Hacknet's own transaction tooling.
#
# The broadcaster image ships the flood script but not the contract it deploys,
# and the script reads nonces from the indexer, so both are supplied here.
traffic() {
    need_source
    local key seconds=${1:-120}
    # The first account the broadcaster already sends transfers from, which the
    # genesis chainstate funds, unlike the key the flood script defaults to.
    key=$(compose_value ACCOUNT_KEYS tx-broadcaster | cut -d, -f1)
    compose cp docker/stacker/stacking/flooder.clar tx-broadcaster:/root/flooder.clar
    # The script deploys once per run and then only calls, so it runs in short
    # rounds: a verification window has to contain a deploy as well as calls.
    log "deploying a contract and calling it for ${seconds}s"
    compose exec -e NUM_FLOODERS=2 -e TX_PER_FLOOD=3 -e BOOTSTRAPPER_KEY="$key" \
        -e STACKS_CORE_RPC_HOST=stacks-api -e STACKS_CORE_RPC_PORT=3999 \
        tx-broadcaster sh -c "cd /root && end=\$((\$(date +%s) + ${seconds})); \
            while [ \$(date +%s) -lt \$end ]; do timeout 90 npx tsx flood.ts || true; done"
}

main() {
case ${1:-} in
setup) setup ;;
up) up ;;
down) down ;;
wipe) wipe ;;
status) status ;;
wait) shift && wait_for "$@" ;;
cycles) shift && cross_cycles "$@" ;;
stacking) shift && stacking "$@" ;;
checkpoint) checkpoint ;;
fund) shift && fund "$@" ;;
register) register ;;
mine) mine ;;
config) shift && nano_config "${1:-$(cat "$RUN/replaced-participant" 2>/dev/null)}" &&
    cat "$RUN/nano.toml" ;;
replace) shift && replace "$@" ;;
host) shift && host "$@" ;;
unhost) unhost ;;
verify) verify ;;
verify-hosted) verify_hosted ;;
restore) restore ;;
observe) observe ;;
stop-observing) stop_observing ;;
traffic) shift && traffic "$@" ;;
*)
    cat >&2 <<'USAGE'
usage: harness.sh <command>

  setup              clone Hacknet at the pinned commit and patch it
  up                 build and boot the network from genesis
  wait <height> [s]  wait for a Bitcoin height, failing on a Stacks stall
  cycles [n] [s]     cross n reward-cycle boundaries and require signer sets
  stacking           show how many future pox-5 cycles have signers
  checkpoint         export the state a nano participant validates from
  replace <1|2|3>    stop one stock participant and run nano in its place
  host <1|2|3>       run nano as the node half and a stock stacks-signer on it
  unhost             stop the hosted signer and restore the participant
  fund [btc]         give nano a funded Bitcoin wallet and miner keys
  register           register nano's leader key on Bitcoin
  mine               restart nano with the mining role on
  config             print the configuration nano would start from
  traffic [seconds]  deploy a contract and call it for a while
  verify             assert the network keeps working with nano in place
  verify-hosted      assert a stock signer, submitter and observer work through nano
  status             heights, reward cycle, and nano state
  observe            record new_block receipts a fixture capture needs
  restore            put the stock participant back
  down               stop the network
  wipe               delete the chainstate
USAGE
    exit 2
    ;;
esac
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    main "$@"
fi
