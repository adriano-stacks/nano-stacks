#!/usr/bin/env bash
# Capture whatever stopped a mainnet descent and decode it offline.
#
# A node that cannot decode a block stops at that height and nowhere else, and
# finding out which byte by restarting it against a live peer costs minutes an
# attempt. The node names the tenure it failed on, so this fetches exactly that
# tenure once and hands it to both decoders — nano's and stacks-core's — which
# turns the next one of these into a diff.
#
#   scripts/diagnose-descent.sh [node.log] [peer]
set -euo pipefail

log="${1:-/home/aldur/mainnet-node/node.log}"
peer="${2:-https://api.mainnet.hiro.so}"
capture=/tmp/failing-tenure.bin

tenure=$(grep -o 'descending through tenure [0-9a-f]*' "$log" | tail -1 | awk '{print $4}')
if [ -z "$tenure" ]; then
    echo "no descent failure in $log" >&2
    exit 1
fi

echo "failing tenure: $tenure"
curl -sS --max-time 120 "$peer/v3/tenures/$tenure" -o "$capture" \
    -w 'captured %{size_download} bytes, status %{http_code}\n'

cargo run -q -p xtask -- decode-blocks "$capture" | tail -3 || true
NANO_MAINNET_BLOCKS="$capture" cargo test -q -p nano-conformance --test conformance mainnet_codec 2>&1 | tail -20
