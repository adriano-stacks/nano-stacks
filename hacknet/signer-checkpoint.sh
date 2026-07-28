#!/usr/bin/env bash
# Export everything a nano signer or miner needs to validate Hacknet blocks
# from a recent checkpoint: the Clarity MARF, the checkpoint block's identity
# and state root, the block that follows it, and the miner rewards that mature
# over the next MATURITY tenures.
set -euo pipefail

STATE_DIR=${STATE_DIR:?path to the Hacknet chainstate directory}
OUT=${OUT:?output directory}
PEER=${PEER:-http://127.0.0.1:20443}
DEPTH=${DEPTH:-30}          # blocks below the tip to checkpoint at
MATURITY=${MATURITY:-100}   # tenures of matured rewards to precompute

NODE=$STATE_DIR/stacks-miner-1/nakamoto-neon
BLOCKS_DB=$NODE/chainstate/blocks/nakamoto.sqlite
INDEX_DB=$NODE/chainstate/vm/index.sqlite
CLARITY=$NODE/chainstate/vm/clarity

# Copy each database first: the live files hold recent commits in their
# write-ahead log, which an immutable read would silently miss.
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
snapshot() { sqlite3 "$1" ".backup $WORK/$2"; echo "$WORK/$2"; }
query() { sqlite3 "file:$1?immutable=1" "$2"; }

BLOCKS_DB=$(snapshot "$BLOCKS_DB" blocks.sqlite)
INDEX_DB=$(snapshot "$INDEX_DB" index.sqlite)

tip_height=$(curl -sf "$PEER/v2/info" | python3 -c 'import sys,json;print(json.load(sys.stdin)["stacks_tip_height"])')
checkpoint_height=$((tip_height - DEPTH))
mkdir -p "$OUT"

read -r checkpoint_id < <(query "$BLOCKS_DB" \
  "select index_block_hash from nakamoto_staging_blocks where processed = 1 and orphaned = 0 and height = $checkpoint_height")
read -r anchor_id < <(query "$BLOCKS_DB" \
  "select index_block_hash from nakamoto_staging_blocks where processed = 1 and orphaned = 0 and height = $((checkpoint_height + 1))")

curl -sf "$PEER/v3/blocks/$checkpoint_id" -o "$OUT/checkpoint-block.bin"
curl -sf "$PEER/v3/blocks/$anchor_id" -o "$OUT/anchor-block.bin"
# The state index root sits at a fixed offset in the consensus block header.
state_root=$(python3 -c "print(open('$OUT/checkpoint-block.bin','rb').read()[101:133].hex())")
anchor_consensus_hash=$(python3 -c "print(open('$OUT/anchor-block.bin','rb').read()[17:37].hex())")
anchor_bitcoin_height=$(curl -sf "$PEER/v3/sortitions/consensus/$anchor_consensus_hash" |
  python3 -c 'import sys,json;print(json.load(sys.stdin)[0]["burn_block_height"])')

sqlite3 "$CLARITY/marf.sqlite" ".backup $OUT/marf.sqlite"
cp "$CLARITY/marf.sqlite.blobs" "$OUT/marf.sqlite.blobs"

tenure_height=$(curl -sf "$PEER/v2/info" | python3 -c 'import sys,json;print(json.load(sys.stdin)["tenure_height"])')
# Replaying from the checkpoint re-executes tenures the peer already passed, so
# the window starts below the current tenure by more than the checkpoint depth.
python3 - "$INDEX_DB" "$((tenure_height > DEPTH + 10 ? tenure_height - DEPTH - 10 : 1))" "$((DEPTH + 10 + MATURITY))" > "$OUT/native-effects.json" <<'PY'
import json, sqlite3, sys

# A tenure's own reward matures 100 tenures later: the coinbase pays the tenure's
# own recipient and its anchored fees pay the previous tenure's recipient. Reading
# the scheduled payments rather than the settled ones lets the window run ahead of
# the peer's tip, which is what a live signer or miner needs.
MATURITY = 100

database, first_height, span = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
connection = sqlite3.connect(f"file:{database}?immutable=1", uri=True)


def scheduled_payment(coinbase_height):
    tenure = connection.execute(
        "SELECT block_id FROM nakamoto_tenure_events WHERE cause = 0 AND coinbase_height = ? LIMIT 1",
        (coinbase_height,),
    ).fetchone()
    if tenure is not None:
        return connection.execute(
            "SELECT COALESCE(recipient, address), CAST(coinbase AS INTEGER), "
            "CAST(tx_fees_anchored AS INTEGER) FROM payments WHERE index_block_hash = ? AND miner = 1",
            (tenure[0],),
        ).fetchone()
    # Before Nakamoto a tenure is one block, so the schedule is keyed by height.
    return connection.execute(
        "SELECT COALESCE(recipient, address), CAST(coinbase AS INTEGER), "
        "CAST(tx_fees_anchored AS INTEGER), schedule_type FROM payments "
        "WHERE stacks_block_height = ? AND miner = 1 ORDER BY rowid LIMIT 1",
        (coinbase_height,),
    ).fetchone()


effects = []
for coinbase_height in range(first_height, first_height + span + 1):
    if coinbase_height <= MATURITY:
        continue
    earned = scheduled_payment(coinbase_height - MATURITY)
    if earned is None:
        continue
    previous = scheduled_payment(coinbase_height - MATURITY - 1)
    # A Nakamoto tenure hands its anchored fees to the previous tenure; before
    # Nakamoto the miner kept them. Both shares are credited even when zero,
    # because the write itself is consensus state, and a parent share with no
    # tenure lands on the boot address.
    own, parent = (earned[1], earned[2]) if earned[3] == "nakamoto" else (earned[1] + earned[2], 0)
    recipient = previous[0] if previous is not None else "ST000000000000000000002AMW42H"
    effects.append(
        {
            "coinbase_height": coinbase_height,
            "credits": [
                {"recipient": earned[0], "amount": own},
                {"recipient": recipient, "amount": parent},
            ],
            "liquid_supply_increase": earned[1],
        }
    )
json.dump({"matured_effects": effects}, sys.stdout, indent=2)
PY

cat > "$OUT/checkpoint.toml" <<EOF
checkpoint_stacks_height = $checkpoint_height
source_state_id = "$checkpoint_id"
state_index_root = "$state_root"
anchor_bitcoin_height = $anchor_bitcoin_height
EOF
cat "$OUT/checkpoint.toml"
