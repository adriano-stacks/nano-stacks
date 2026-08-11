#!/usr/bin/env bash
# Export everything a nano signer or miner needs to validate Hacknet blocks
# from a recent checkpoint: the Clarity MARF, the checkpoint block's identity
# and state root, the block that follows it, and the miner rewards that mature
# over the next MATURITY tenures.
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
STATE_DIR=${STATE_DIR:?path to the Hacknet chainstate directory}
OUT=${OUT:?output directory}
PEER=${PEER:-http://127.0.0.1:20443}
DEPTH=${DEPTH:-30}          # blocks below the tip to checkpoint at
MATURITY=${MATURITY:-100}   # tenures of matured rewards to precompute

NODE=$STATE_DIR/stacks-miner-1/nakamoto-neon
BLOCKS_DB=$NODE/chainstate/blocks/nakamoto.sqlite
INDEX_DB=$NODE/chainstate/vm/index.sqlite
SORTITION_DB=$NODE/burnchain/sortition/marf.sqlite
CLARITY=$NODE/chainstate/vm/clarity

# Copy each database first: the live files hold recent commits in their
# write-ahead log, which an immutable read would silently miss.
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
snapshot() { sqlite3 "$1" ".backup $WORK/$2"; echo "$WORK/$2"; }
query() { sqlite3 "file:$1?immutable=1" "$2"; }

BLOCKS_DB=$(snapshot "$BLOCKS_DB" blocks.sqlite)
INDEX_DB=$(snapshot "$INDEX_DB" index.sqlite)
SORTITION_DB=$(snapshot "$SORTITION_DB" sortition.sqlite)

# A sortition collects its own emission plus the per-block bonus the pre-mine
# funded, which is what a snapshot accumulates when its parent chose a miner.
# nano needs it to derive the coinbase of every tenure it executes itself,
# rather than depending on a precomputed window of matured rewards.
initial_mining_bonus=$(query "$SORTITION_DB" "
  select child.accumulated_coinbase_ustx from snapshots child
  join snapshots parent on child.parent_burn_header_hash = parent.burn_header_hash
  where child.pox_valid = 1 and parent.sortition = 1 and parent.total_burn > 0
  order by child.block_height desc limit 1")
first_bitcoin_height=$(curl -sf "$PEER/v2/pox" |
  python3 -c 'import sys,json;print(json.load(sys.stdin)["first_burnchain_block_height"])')
mainnet=$(curl -sf "$PEER/v2/info" |
  python3 -c 'import sys,json;print("true" if json.load(sys.stdin)["network_id"] == 1 else "false")')

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

# The window only has to cover the tenures whose rewards matured before nano
# had any history of its own; past that it derives them from what it executed.
checkpoint_consensus_hash=$(python3 -c "print(open('$OUT/checkpoint-block.bin','rb').read()[17:37].hex())")
checkpoint_tenure=$(query "$INDEX_DB" "
  select coinbase_height from nakamoto_tenure_events
  where cause = 0 and tenure_id_consensus_hash = '$checkpoint_consensus_hash' limit 1")
tenure_height=$(curl -sf "$PEER/v2/info" | python3 -c 'import sys,json;print(json.load(sys.stdin)["tenure_height"])')
# Replaying from the checkpoint re-executes tenures the peer already passed, so
# the window starts below the current tenure by more than the checkpoint depth.
python3 - "$INDEX_DB" "$((tenure_height > DEPTH + 10 ? tenure_height - DEPTH - 10 : 1))" "$((DEPTH + 10 + MATURITY))" \
  "$initial_mining_bonus" "$first_bitcoin_height" "$mainnet" "$((checkpoint_tenure + MATURITY))" \
  > "$OUT/native-effects.json" <<'PY'
import json, sqlite3, sys

# A tenure's own reward matures 100 tenures later: the coinbase pays the tenure's
# own recipient and its anchored fees pay the previous tenure's recipient. Reading
# the scheduled payments rather than the settled ones lets the window run ahead of
# the peer's tip, which is what a live signer or miner needs.
MATURITY = 100

database, first_height, span = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
bonus, first_bitcoin_height, mainnet = int(sys.argv[4]), int(sys.argv[5]), sys.argv[6] == "true"
last_height = int(sys.argv[7])
connection = sqlite3.connect(f"file:{database}?immutable=1", uri=True)


first_nakamoto_tenure = connection.execute(
    "SELECT MIN(coinbase_height) FROM nakamoto_tenure_events WHERE cause = 0"
).fetchone()[0]


def scheduled_payment(coinbase_height):
    if coinbase_height >= first_nakamoto_tenure:
        tenure = connection.execute(
            "SELECT block_id FROM nakamoto_tenure_events WHERE cause = 0 AND coinbase_height = ? LIMIT 1",
            (coinbase_height,),
        ).fetchone()
        # A tenure the peer has not reached has no schedule to read, and its
        # height must not be confused with a Stacks block height.
        if tenure is None:
            return None
        return connection.execute(
            "SELECT COALESCE(recipient, address), CAST(coinbase AS INTEGER), "
            "CAST(tx_fees_anchored AS INTEGER), schedule_type FROM payments "
            "WHERE index_block_hash = ? AND miner = 1",
            (tenure[0],),
        ).fetchone()
    # Before Nakamoto a tenure is one block, so the schedule is keyed by height.
    return connection.execute(
        "SELECT COALESCE(recipient, address), CAST(coinbase AS INTEGER), "
        "CAST(tx_fees_anchored AS INTEGER), schedule_type FROM payments "
        "WHERE stacks_block_height = ? AND miner = 1 ORDER BY rowid LIMIT 1",
        (coinbase_height,),
    ).fetchone()


# Every tenure whose reward has not matured yet has to travel with the
# checkpoint: for the next MATURITY tenures the node pays out tenures it never
# executed, and it has no other way to know what they earned. Stopping at the
# first of them strands the node once it runs that far, which on Hacknet
# appeared as a stalled chain a hundred tenures after the checkpoint.
#
# A tenure's fees are recorded in the *next* tenure's schedule, and the payout
# also credits the tenure before the one maturing, so the window starts one
# below it.
tenures = []
checkpoint_tenure = last_height - MATURITY
for coinbase_height in range(checkpoint_tenure - MATURITY - 1, checkpoint_tenure + 1):
    earned = scheduled_payment(coinbase_height)
    following = scheduled_payment(coinbase_height + 1)
    if earned is None or following is None:
        continue
    tenures.append(
        {
            "coinbase_height": coinbase_height,
            "recipient": earned[0],
            "coinbase": earned[1],
            "fees": following[2],
        }
    )

effects = []
for coinbase_height in range(first_height, min(first_height + span, last_height) + 1):
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
json.dump(
    {
        "matured_effects": effects,
        "tenures": tenures,
        "coinbase_schedule": {
            "mainnet": mainnet,
            "first_bitcoin_height": first_bitcoin_height,
            "initial_mining_bonus_ustx": bonus,
        },
    },
    sys.stdout,
    indent=2,
)
PY

# The reward set that signed the checkpoint block, which is what makes the
# checkpoint's state root worth anything: the checkpoint asserting its own root
# is not evidence. Read from the peer's `/v3/stacker_set`, which is a document
# obtained without the checkpoint and readable by anything.
checkpoint_bitcoin_height=$(curl -sf "$PEER/v3/sortitions/consensus/$checkpoint_consensus_hash" |
  python3 -c 'import sys,json;print(json.load(sys.stdin)[0]["burn_block_height"])')
attesting_cycle=$(curl -sf "$PEER/v2/pox" |
  python3 -c "import sys,json;p=json.load(sys.stdin);print(($checkpoint_bitcoin_height - p['first_burnchain_block_height']) // p['reward_cycle_length'])")
curl -sf "$PEER/v3/stacker_set/$attesting_cycle" -o "$OUT/reward-set.json"

# The sortition history the node derives its own snapshots from: the seed, the
# run of consensus hashes behind it, and the leader-key registry a winning
# commitment names. A checkpoint written without these is one the node cannot
# derive a single burn view from -- so no tenure's coinbase proof is checkable,
# no miner signature is, and `/v3/sortitions` answers 503, which is what a stock
# signer's state machine fails to initialise on.
#
# Exported at the anchor, because everything above it the node walks Bitcoin for
# itself.
(cd "$ROOT" && cargo xtask export-sortition \
  "$SORTITION_DB" "$OUT/sortition" "$anchor_bitcoin_height")

(cd "$ROOT" && cargo xtask export-checkpoint-history \
  --blocks-db "$BLOCKS_DB" \
  --source-id "$checkpoint_id" \
  --state-root "$state_root" \
  --out-dir "$OUT/authentication-history")

# The same manifest a captured fixture publishes, so a checkpoint reads the
# same whether it came from here or from `cargo xtask capture-fixtures`.
cat > "$OUT/checkpoint.toml" <<EOF
format = "stacks-core-marf-sqlite-v2"
checkpoint_stacks_height = $checkpoint_height
source_state_id = "$checkpoint_id"
published_state_index_root = "$state_root"
first_bitcoin_height = $anchor_bitcoin_height
attesting_reward_cycle = $attesting_cycle
EOF
cat "$OUT/checkpoint.toml"
