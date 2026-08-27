#!/usr/bin/env bash
# Replay real mainnet blocks against a candidate binary in minutes, not hours.
#
# A compiler change moves `COMPILER_IDENTITY`, and a node refuses state produced
# under another one — deliberately, because mixing them would seal state no single
# engine can reproduce. The release path answers that with a ceremony and a fresh
# import: about four hours before the first block executes. That is the right
# price for evidence and the wrong price for a question.
#
# So this pays the question's price instead. It reflinks an already-imported state
# (instant, copy-on-write), repins the profile fingerprint in both places that
# record it, and runs the candidate against the copy. Three attempts on
# 2026-08-27 found two consensus differentials that a release run would have
# taken five hours each to surface:
#
#   * a cross-contract argument rebuilt rather than measured, and
#   * a list capacity manufactured over a `NoType` entry,
#
# both of which refused mainnet block 8,667,169 with a state root mismatch.
#
# NOT RELEASE EVIDENCE, and the copy says so in its own NOTE.md. A hand-repinned
# profile is precisely what task 053 refuses: it lets a state claim a compiler
# that never produced it. What it establishes is narrower and still worth having —
# *this* candidate executes *these* blocks and agrees with the chain.
#
# usage: fast-mainnet-replay.sh <source-state-dir> <binary> [scratch-dir] [peers-json]
#
#   source-state-dir  a state directory holding executed mainnet state, typically
#                     a stopped release subject or witness. Never written to.
#   binary            the `stacks-node` under test.
#   scratch-dir       where the copy goes; defaults to a stamped directory beside
#                     the source. Removed and recreated if it exists.
#   peers-json        optional `data-endpoints.json` to seed, so the copy does not
#                     have to rediscover a data plane before it can fetch.
set -euo pipefail

if [ "$#" -lt 2 ]; then
    sed -n '/^# usage:/,/^#   peers-json/p' "$0" >&2
    exit 2
fi

source_state=${1%/}
# Resolved before any `cd`, so a relative path still names the same file.
binary=$(readlink -f -- "$2")
scratch=${3:-}
peers=${4:-}

test -d "$source_state/chainstate"
test -x "$binary"

if [ -z "$scratch" ]; then
    scratch=$(dirname "$source_state")/fast-replay-$(date -u +%Y%m%dT%H%M%SZ)
fi

profile=$("$binary" build-identity | python3 -c 'import sys,json; print(json.load(sys.stdin)["compatibility_fingerprint"])')
compiler=$("$binary" build-identity | python3 -c 'import sys,json; print(json.load(sys.stdin)["compiler_identity"])')
test -n "$profile"

echo "== candidate $binary"
echo "   compiler $compiler"
echo "   profile  $profile"

# The source must be at rest: a state directory with a live writer has a WAL that
# a copy cannot interpret, and the copy would be a torn read of a moving trie.
if pgrep -f "working_dir.*$(basename "$source_state")" > /dev/null 2>&1; then
    echo "refusing: something still holds $source_state" >&2
    exit 1
fi
for lock in "$source_state"/*.lock; do
    test -e "$lock" || continue
    if fuser "$lock" > /dev/null 2>&1; then
        echo "refusing: $lock is held" >&2
        exit 1
    fi
done

rm -rf -- "$scratch"
mkdir -p "$scratch"
echo "== reflinking $source_state into $scratch/state"
cp --reflink=always -r -- "$source_state" "$scratch/state"

provenance=$scratch/state/chainstate/checkpoint-provenance.toml
clarity=$scratch/state/chainstate/clarity.sqlite
test -s "$provenance"
test -s "$clarity"

# A state that has executed nothing past its checkpoint needs no repin at all:
# `adopt-imported-state` proves it is that import and changes the record with a
# proof. Only a state that has executed falls through to the hand repin below,
# and that is the part that makes this diagnostic.
if [ -n "${NANO_ADOPT_CHECKPOINT:-}" ] \
    && "$binary" adopt-imported-state \
        --state "$scratch/state/chainstate" \
        --checkpoint "$NANO_ADOPT_CHECKPOINT" 2>/dev/null; then
    echo "== adopted under the active profile, with no hand repin"
    adopted=yes
else
    adopted=no
fi

if [ "$adopted" = no ]; then
    echo "== repinning the profile in both records"
    python3 - "$provenance" "$profile" <<'PY'
    import re, sys
    path, profile = sys.argv[1], sys.argv[2]
    text = open(path).read()
    new, count = re.subn(r'profile_fingerprint = "[0-9a-f]+"',
                         f'profile_fingerprint = "{profile}"', text)
    assert count == 1, f"expected one profile_fingerprint in {path}, found {count}"
    open(path, "w").write(new)
PY
    sqlite3 "$clarity" "UPDATE consensus_profile SET fingerprint = '$profile' WHERE only_row = 0;"
    test "$(sqlite3 "$clarity" 'SELECT fingerprint FROM consensus_profile;')" = "$profile"

fi

if [ -n "$peers" ] && [ -s "$peers" ]; then
    cp -- "$peers" "$scratch/state/data-endpoints.json"
    echo "== seeded the data plane from $peers"
fi

cat > "$scratch/NOTE.md" <<EOF
# Diagnostic only, not release evidence

A reflinked copy of \`$source_state\`, with the profile fingerprint repinned by
hand — in \`checkpoint-provenance.toml\` and in the state's \`consensus_profile\`
row — to $profile, the profile of

    $binary

That repin is exactly what the release path refuses, because it lets a state claim
a compiler that never produced it. Nothing here may be presented as replay depth,
as a clean checkpoint-to-tip run, or as any part of task 053.

What it can establish: whether this candidate executes these mainnet blocks and
agrees with the chain. Made at $(date -u +%FT%TZ) by \`scripts/fast-mainnet-replay.sh\`.
EOF

# The node's own configuration, pointed at the copy and off every port the
# release deployment uses.
config=$scratch/config.toml
python3 - "$source_state" "$scratch" "${NANO_PORT_SHIFT:-200}" > "$config" <<'PY'
import os, re, sys
source, scratch, shift_by = sys.argv[1], sys.argv[2], int(sys.argv[3])
text = open(os.path.join(os.path.dirname(source), "config.toml")).read()
text = text.replace(source, scratch + "/state")
# Shift the loopback *listeners* by 200 so a diagnostic never collides with the
# run. Only the binds: the burnchain RPC is somebody else's port and moving it
# points the copy at nothing.
def shift(match):
    return f"{match.group(1)}{match.group(2)}{int(match.group(3)) + shift_by}"
text = re.sub(r'^(\w*bind = ")(127\.0\.0\.1:)(\d+)', shift, text, flags=re.M)
text = re.sub(r"^event_observers = .*$", "event_observers = []", text, flags=re.M)
print(text)
PY

echo "== running the candidate against the copy"
echo "   config $config"
echo "   log    $scratch/replay.log"
cd "$scratch"
exec "$binary" start --config config.toml
