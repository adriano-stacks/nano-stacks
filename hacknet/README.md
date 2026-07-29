# Running nano-stacks against hacknet

nano joins a [hacknet](https://github.com/stacks-network/hacknet) epoch-4 network as
an ordinary participant: it holds Stacks and Bitcoin keys of its own, stacks for a
signer slot, and commits on Bitcoin for a tenure. Nothing here is nano-specific
protocol; it is the same setup a stock signer or miner needs.

## Released PoX-5 baseline

Hacknet's current default is still a pre-merge PoX-5 integration and API
`9.0.0-pox5.8`. Apply the compatibility patch before starting a new network:

```
git -C ../hacknet apply /home/aldur/nano-stacks/hacknet/hacknet-main.patch
(cd ../hacknet && make genesis)
```

The patch selects Stacks Core `main` and the stable API `9.0.1`. It preserves
Hacknet's configured sBTC contracts, so the existing PoX-5 bootstrap and
Bitcoin-staking helpers keep working. Those sBTC contracts are still external
dependencies, so `pox5-setup` remains required. It applies to Hacknet commit
`bf821e9d556eab8c7a30c6e86a7dc1f9b200f1a1` and can be removed once upstream
merges the equivalent change.

## Keys

Four secrets, kept outside the repository in a gitignored `.hacknet/`:

| File | Purpose |
|---|---|
| `signer.key` | 32-byte Stacks key that stacks, and signs block responses |
| `miner-signing.key` | 32-byte Stacks key that signs blocks and leader-key registrations |
| `miner-vrf.key` | 32-byte ed25519 key seeding the coinbase VRF proof |
| `bitcoin-rpc.pass` | Bitcoin Core RPC password |

Generate the three keys with `openssl rand -hex 32`.

## Bitcoin wallet

The miner funds its own commitments, so it needs a wallet that holds private keys —
not the watch-only wallets hacknet creates for its own miners:

```
bitcoin-cli -named createwallet wallet_name=nano-miner descriptors=false
bitcoin-cli -rpcwallet=nano-miner getnewaddress "nano" legacy
bitcoin-cli -rpcwallet=depositor sendtoaddress <address> 100
```

Do not register this wallet with hacknet's `bitcoin-miner` service: its on-demand
trigger sums confirmations across the wallets it watches, so joining that sum
suppresses block production for the rest of the network.

## Checkpoint

Both the signer and the miner execute blocks, so they start from a checkpoint of a
node's Clarity MARF plus the miner rewards that mature over the tenures they will
validate:

```
STATE_DIR=<hacknet chainstate> OUT=/tmp/nano-checkpoint ./hacknet/signer-checkpoint.sh
```

## Signing

The signer derives its contracts and slot from the active reward set, so it only
needs the boot address:

```
stacks-signer run --peer http://127.0.0.1:20443/ \
  --bitcoin-rpc http://127.0.0.1:18443 --bitcoin-rpc-user hacknet \
  --bitcoin-rpc-password-file .hacknet/bitcoin-rpc.pass \
  --miner-contract ST000000000000000000002AMW42H/miners \
  --private-key "$(cat .hacknet/signer.key)" --state-file /tmp/nano-checkpoint/signer.json \
  --checkpoint /tmp/nano-checkpoint/marf.sqlite \
  --tenure-accounting /tmp/nano-checkpoint/native-effects.json \
  --source-state-id <id> --state-root <root> \
  --anchor-block /tmp/nano-checkpoint/anchor-block.bin --anchor-bitcoin-height <height> \
  --pox-5-activation-height 262 --pox-v1-unlock-height 205 \
  --pox-v2-unlock-height 207 --pox-v3-unlock-height 210
```

The key must be stacked through a PoX-5 signer manager before the cycle it signs
for. Weight matters in both directions: too little and the stock signers reach the
threshold without waiting, too much and the network stalls whenever nano is down.

## Mining

Register a leader key once, then commit on every Bitcoin block. Commitments must
chain through one another's change output, or the sortition treats each one as a
first-time miner and weights it accordingly:

```
stacks-register-leader-key --bitcoin-rpc http://127.0.0.1:18443/wallet/nano-miner ...
stacks-commit-block --commitment-chain-file /tmp/nano-commit-chain.txt --after-new-block ...
stacks-mine-tenure --sortition-hash-cache /tmp/nano-sortition-hash.json ...
```

Win a sortition and you owe the network a block: leave the tenure unmined and the
chain stops until the reward set for the next cycle can be resolved again.
