#!/usr/bin/env bash
# Prove an Epoch 4.0 executor binary has no network listener or client.
#
#   scripts/check-executor-artifact.sh <binary> [--expect-network]
#
# The executor is the sole chainstate writer, so the whole point of the
# boundary is that nothing can reach it except its parent's bounded local
# protocol. `cargo tree` says what was intended; this asks the executable.
#
# Two questions, and the first is the one that cannot be argued with: a process
# that never calls into the socket API cannot listen or dial, whatever its
# source says. The second catches a network path that a future dependency links
# statically, before it can acquire a syscall.
#
# `--expect-network` inverts the verdict, so a binary that *does* speak to the
# network can be used as this gate's positive control. Without one, a check that
# refuses nothing looks identical to a check that inspects nothing.
set -euo pipefail

binary=${1:?usage: check-executor-artifact.sh <binary> [--expect-network]}
expect_network=${2:-}

test -x "$binary"

work=$(mktemp -d "${TMPDIR:-/tmp}/executor-inspection.XXXXXX")
trap 'rm -rf -- "$work"' EXIT

# Undefined symbols are what the loader must supply: the socket API arrives here
# or not at all.
nm -u "$binary" | awk '{print $NF}' | sort -u >"$work/undefined"
nm -C --defined-only "$binary" >"$work/defined"
test -s "$work/defined"

# Anchored, because the lesson of writing this was a loose pattern: `bind`
# matches clar2wasm's `Bindings`, cranelift's `bind_label` and rusqlite's
# `bind_parameters`, and `connect` matches `rusqlite::Connection`. None of those
# is a socket, and a gate that cries wolf on them would be turned off.
readonly syscalls='^(socket|socketpair|bind|listen|accept|accept4|connect|getaddrinfo|getnameinfo|recvfrom|sendto|recvmsg|sendmsg|gethostbyname)$'
readonly types='std::net::|TcpListener|TcpStream|UdpSocket|reqwest::|hyper::|tokio::runtime'

grep -E "$syscalls" "$work/undefined" >"$work/syscall-matches" || true
grep -E "$types" "$work/defined" >"$work/type-matches" || true

networked=false
if test -s "$work/syscall-matches" || test -s "$work/type-matches"; then
  networked=true
fi

if test "$expect_network" = --expect-network; then
  if test "$networked" = false; then
    echo "$binary shows no network capability, so this control proves nothing" >&2
    exit 1
  fi
  printf 'control: %s does speak to the network, as required\n' "$binary"
  exit 0
fi

if test "$networked" = true; then
  echo "$binary has network capability:" >&2
  sed 's/^/  syscall: /' "$work/syscall-matches" >&2
  sed 's/^/  type: /' "$work/type-matches" >&2
  exit 1
fi

jq -n \
  --arg schema nano-stacks/executor-inspection/v1 \
  --arg binary "$binary" \
  --arg sha256 "$(sha256sum "$binary" | awk '{print $1}')" \
  --argjson defined_symbols "$(wc -l <"$work/defined")" \
  --argjson undefined_symbols "$(wc -l <"$work/undefined")" \
  '{schema:$schema,binary:$binary,sha256:$sha256,
    symbol_table_inspected:true,
    defined_symbols:$defined_symbols,undefined_symbols:$undefined_symbols,
    socket_api_symbols:0,network_type_symbols:0}'
