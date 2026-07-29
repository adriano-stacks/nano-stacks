# Replacing a Hacknet participant with nano-stacks

A Hacknet participant is a `stacks-node` and the `stacks-signer` it feeds. nano
replaces one of them: it holds the Stacks key that participant staked, executes
every block from its own checkpoint, and answers the miner over `StackerDB`.

Hacknet stacks three signers of equal weight against a threshold of seven tenths,
so **no block is accepted without all three**. A network that keeps producing
blocks with nano in place is therefore proof that nano's signature counted, and
one that stalls is proof that it did not — the stall is the test.

## Reproducing a run

```
hacknet/harness.sh setup            # clone Hacknet at the pinned commit and patch it
hacknet/harness.sh up               # build and boot from genesis
hacknet/harness.sh wait 285         # epoch 4.0 starts at 262, PoX-5 cycle 14 at 280
hacknet/harness.sh checkpoint       # export the state nano validates from
hacknet/harness.sh replace 3        # stop participant 3 and sign in its place
hacknet/harness.sh verify           # assert the network keeps doing every kind of work
hacknet/harness.sh restore          # put the stock participant back
hacknet/harness.sh down
```

`status` prints the heights of all three participants, the reward cycle, and
whether nano is running. `wait` fails as soon as Bitcoin advances with a frozen
Stacks tip, which is what a broken replacement looks like.

Each command is independent, so a run can be inspected or interrupted at any
stage. The clone, the checkpoint, the signer log and the state file live under
`~/.cache/nano-stacks/hacknet` (`NANO_HACKNET_HOME`).

Compose is driven directly rather than through Hacknet's Makefile, whose Linux
path assumes rootful Docker: it removes chainstate with `sudo` and extracts
archives with `sudo tar`. The commands mirror `make build`, `make genesis`,
`make down` and `make stop/start` one for one.

## What a passing run shows

`verify` runs `cargo test -p nano-conformance --test hacknet_replacement`, which
follows the canonical chain until it has both a number of new blocks and a reward
cycle rollover, then asserts what those blocks contained. A recorded run:

```
observed 211 canonical blocks across cycles 15..=16
every one of the 211 blocks carries nano's signature
152 transfer transactions, including c597a9b3… which the network reports as success
1 deploy transactions, including 29323532… which the network reports as success
816 call transactions, including 1abac26b… which the network reports as success
20 tenure change transactions, including efaee37d… which the network reports as success
19 coinbase transactions, including 60543126… which the network reports as success
19 sortitions across 2 distinct miners
reward cycle 16 pays a waterfall set in which nano holds weight 10 of 30
```

Every signature is recovered from the block header and checked against the reward
set, and every receipt is read back from the indexer.

## Released PoX-5 baseline

Hacknet's default is still a pre-merge PoX-5 integration and API
`9.0.0-pox5.8`. `setup` applies both patches in this directory, which select
Stacks Core `main` and stable API `9.0.1`, and which apply to Hacknet commit
`bf821e9d556eab8c7a30c6e86a7dc1f9b200f1a1`.

The API patch builds the indexer from its release commit and retains raw PoX-5
`stake-update` events its bundled codec cannot decode after Core renamed
`prev-unlock-height` to `prev-unlock-cycle`. Without it the indexer answers
`/new_block` with HTTP 500, which blocks Core's event dispatcher and stops the
chain at the first PoX-5 reward set. Both patches preserve Hacknet's configured
sBTC contracts, so `pox5-setup` still bootstraps `.pox-5`.

## What the checkpoint carries

`signer-checkpoint.sh` exports a node's Clarity MARF at a block below its tip,
that block's identity and state root, the block after it, and two things nano
cannot derive for itself:

- the miner rewards that matured before nano had any history, which is the
  hundred tenures after the checkpoint;
- the burnchain coinbase schedule — the emission table and the per-block bonus
  the pre-mine funded — from which nano derives the rewards of every tenure it
  executes itself, indefinitely.

A payout that neither source covers is an error, not a silently empty write.

## Mining

Registering a leader key and committing on Bitcoin needs keys and a funded
wallet of its own, kept outside the repository in a gitignored `.hacknet/`:

| File | Purpose |
|---|---|
| `miner-signing.key` | 32-byte Stacks key that signs blocks and leader-key registrations |
| `miner-vrf.key` | 32-byte ed25519 key seeding the coinbase VRF proof |

```
bitcoin-cli -named createwallet wallet_name=nano-miner descriptors=false
bitcoin-cli -rpcwallet=nano-miner getnewaddress "nano" legacy
bitcoin-cli -rpcwallet=depositor sendtoaddress <address> 100
```

The miner funds its own commitments, so it needs a wallet that holds private
keys, not the watch-only wallets Hacknet creates for its own miners. Do not
register that wallet with Hacknet's `bitcoin-miner` service: its on-demand
trigger sums confirmations across the wallets it watches, so joining that sum
suppresses block production for the rest of the network.

Commitments must chain through one another's change output, or the sortition
treats each one as a first-time miner and weights it accordingly. Winning a
sortition owes the network a block: leave the tenure unmined and the chain stops
until the next reward set can be resolved.
