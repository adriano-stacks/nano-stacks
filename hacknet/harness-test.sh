#!/usr/bin/env bash
# The mock functions below are called through functions sourced from the harness.
# shellcheck disable=SC2016,SC2329
set -euo pipefail

TEST_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
# shellcheck source=./harness.sh
# shellcheck disable=SC1091
source "$TEST_ROOT/hacknet/harness.sh"

fail() { printf 'harness-test: %s\n' "$*" >&2; exit 1; }
equal() { [ "$1" = "$2" ] || fail "expected [$2], got [$1]"; }

test_dir=$(mktemp -d)
trap 'rm -rf -- "$test_dir"' EXIT
export SRC=$test_dir

docker() {
    printf '%s|%s|%s|%s|%s|%s|%s|%s|%s\n' "$STACKING_CYCLES" "$STACKS_30_HEIGHT" \
        "$STACKS_31_HEIGHT" "$STACKS_32_HEIGHT" "$STACKS_33_HEIGHT" \
        "$STACKS_34_HEIGHT" "$STACKS_40_HEIGHT" "$POX_PREPARE_LENGTH" \
        "$POX_REWARD_LENGTH"
}
equal "$(compose config)" '12|232|233|234|235|242|262|5|20'
STACKING_CYCLES=7
STACKS_30_HEIGHT=227
STACKS_31_HEIGHT=228
STACKS_32_HEIGHT=229
STACKS_33_HEIGHT=230
STACKS_34_HEIGHT=242
STACKS_40_HEIGHT=272
POX_PREPARE_LENGTH=10
POX_REWARD_LENGTH=15
equal "$(compose config)" '7|227|228|229|230|242|272|10|15'

peer_url() { printf 'mock-peer\n'; }
pox_cycle() { printf '22 ST000000000000000000002AMW42H.pox-5\n'; }
pox5_has_signers() { [ "$2" -le 34 ]; }
equal "$(stacking)" 'stacking: cycles 23..34 have a pox-5 signer set (12 ahead of cycle 22)'

pox5_has_signers() { [ "$2" -le 23 ]; }
if output=$(stacking 2>&1); then
    fail 'a one-cycle stacking horizon passed'
fi
case $output in
*'only 1 cycle(s) stacked ahead of 22: without renewal the chain stops at cycle 24'*) ;;
*) fail "low-horizon diagnostic was not explicit: $output" ;;
esac

pox_cycle() { printf '12 ST000000000000000000002AMW42H.pox-4\n'; }
equal "$(stacking)" 'stacking: ST000000000000000000002AMW42H.pox-4 is active, not pox-5; nothing to measure yet'

curl() {
    case $* in
    *'"arguments":["0x0100000000000000000000000000000017"]'*get-signer-set-first-item-for-cycle*)
        printf '%s\n' '{"okay":true,"result":"0x0a"}'
        ;;
    *) return 22 ;;
    esac
}
pox5_has_signers mock-peer 23 || fail 'encoded cycle 23 was not recognized'
pox5_has_signers mock-peer 24 && fail 'the wrong cycle encoding was accepted'

curl() {
    printf '%s\n' '{"stacker_set":{"signers":[{"weight":1},{"weight":2}]}}'
}
equal "$(reward_set_summary mock-peer 23)" '2 signers, total weight 3'

RUN=$test_dir/run
mkdir -p "$RUN/stock-follower/events/new_mempool_tx" \
    "$RUN/stock-follower/events/new_block"
printf 'abc123\n' > "$RUN/hosted-txid"
printf 'deadbeef\n' > "$RUN/hosted-transaction"
printf '["0xdeadbeef"]\n' > "$RUN/stock-follower/events/new_mempool_tx/00000001.json"
printf '{"transactions":[{"txid":"0xabc123"}]}\n' \
    > "$RUN/stock-follower/events/new_block/00000001.json"
stock_follower_verify 1

SRC=$test_dir/source
mkdir -p "$SRC/docker/stacks" "$RUN/nano"
printf '%s\n' \
    '[node]' \
    'rpc_bind = "0.0.0.0:20443"' \
    'p2p_bind = "0.0.0.0:20444"' \
    'prometheus_bind = "0.0.0.0:9153"' \
    'bootstrap_node = "$BOOTSTRAP_NODE"' \
    '[burnchain]' \
    'peer_host = "$BITCOIN_PEER_HOST"' \
    > "$SRC/docker/stacks/stacks-follower.toml"
printf '%s\n' \
    'rpc_bind = "127.0.0.1:24443"' \
    'p2p_bind = "127.0.0.1:25444"' \
    > "$RUN/nano.toml"
printf 'aabb\n' > "$RUN/nano/p2p-seed"
nano_running() { return 0; }
cargo() { printf '%066d\n' 2; }
curl() { printf '{"stacks_tip_height":321}\n'; }
compose() { printf 'stock-container\n'; }
compose_value() { printf '1\n'; }
start_sink() { :; }
docker() {
    case $* in
    'container inspect '*) return 1 ;;
    'inspect --format '*) printf 'stock-image\n' ;;
    'run '*) printf '%s\n' "$@" > "$test_dir/docker-run.args"; printf 'container\n' ;;
    'logs --follow '*) return 0 ;;
    *) fail "unexpected docker call: $*" ;;
    esac
}
stock_follower_start
grep -Fq 'rpc_bind = "$FOLLOWER_RPC_BIND"' "$RUN/stock-follower/config.toml.in"
grep -Fq 'p2p_bind = "$FOLLOWER_P2P_BIND"' "$RUN/stock-follower/config.toml.in"
grep -Fq '@127.0.0.1:25444' "$RUN/stock-follower/bootstrap-node"
grep -Fxq -- '--network' "$test_dir/docker-run.args"
grep -Fxq -- 'host' "$test_dir/docker-run.args"

printf 'harness-test: all checks passed\n'
