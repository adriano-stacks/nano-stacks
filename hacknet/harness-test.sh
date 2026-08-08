#!/usr/bin/env bash
set -euo pipefail

TEST_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
# shellcheck source=./harness.sh
source "$TEST_ROOT/hacknet/harness.sh"

fail() { printf 'harness-test: %s\n' "$*" >&2; exit 1; }
equal() { [ "$1" = "$2" ] || fail "expected [$2], got [$1]"; }

test_dir=$(mktemp -d)
trap 'rm -rf -- "$test_dir"' EXIT
SRC=$test_dir

docker() {
    printf '%s|%s|%s|%s|%s\n' "$STACKING_CYCLES" "$STACKS_30_HEIGHT" \
        "$STACKS_31_HEIGHT" "$STACKS_32_HEIGHT" "$STACKS_33_HEIGHT"
}
equal "$(compose config)" '12|232|233|234|235'
STACKING_CYCLES=7
STACKS_30_HEIGHT=222
STACKS_31_HEIGHT=223
STACKS_32_HEIGHT=224
STACKS_33_HEIGHT=225
equal "$(compose config)" '7|222|223|224|225'

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

printf 'harness-test: all checks passed\n'
