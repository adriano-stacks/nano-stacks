# Replacing a Hacknet participant with nano-stacks

A Hacknet participant is a `stacks-node` and the `stacks-signer` it feeds. nano
replaces both halves with one binary: it holds the Stacks key that participant
staked, executes every block from its own checkpoint, and answers the miner over
`StackerDB`. Following, signing and mining are roles a single `stacks-node
start --config` process switches on from its configuration file.

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
hacknet/harness.sh replace 3        # stop participant 3 and run nano in its place
hacknet/harness.sh fund             # give nano a funded Bitcoin wallet and miner keys
hacknet/harness.sh register         # register nano's leader key on Bitcoin
hacknet/harness.sh mine             # restart nano with the mining role on
hacknet/harness.sh verify           # assert the network keeps doing every kind of work
hacknet/harness.sh restore          # put the stock participant back
hacknet/harness.sh down
```

`replace` covers the signer half and is enough on its own: the participant then
stops competing for tenures and the other two miners carry the chain. `fund`,
`register` and `mine` add the other half, so nano commits on Bitcoin and mines
the tenures it wins.

`replace` writes `run/nano.toml` and starts the node from it; `mine` rewrites
that file with a `[miner]` table and restarts the same process, which comes back
to the state it left on disk rather than importing the checkpoint again. `config`
prints what would be written without starting anything, and `restore` stops the
node with `SIGTERM`. Set `NANO_RPC_BIND` to serve the public RPC as well, and
`NANO_EVENT_OBSERVERS` to a comma-separated list to post events.

`status` prints the heights of all three participants, the reward cycle, and
whether nano is running. `wait` fails as soon as Bitcoin advances with a frozen
Stacks tip, which is what a broken replacement looks like.

Each command is independent, so a run can be inspected or interrupted at any
stage. The clone, the checkpoint, the configuration, the log and every byte of
nano's own state live under `~/.cache/nano-stacks/hacknet` (`NANO_HACKNET_HOME`),
with the node's state in `run/nano`.

Compose is driven directly rather than through Hacknet's Makefile, whose Linux
path assumes rootful Docker: it removes chainstate with `sudo` and extracts
archives with `sudo tar`. The commands mirror `make build`, `make genesis`,
`make down` and `make stop/start` one for one.

## What a passing run shows

`verify` runs `cargo test -p nano-conformance --test hacknet_replacement`, which
follows the canonical chain until it has both a number of new blocks and a reward
cycle rollover, then asserts what those blocks contained. A recorded run:

```
observed 30 canonical blocks across cycles 17..=18
every one of the 30 blocks carries nano's signature
nano mined 12 of the 30 canonical blocks, at heights [370, 371, 372, 373, 374, 391, …]
18 transfer transactions, including 2520b739… which the network reports as success
3 deploy transactions, including 6308c3e5… which the network reports as success
95 call transactions, including b3778288… which the network reports as success
7 tenure change transactions, including f5c71522… which the network reports as success
6 coinbase transactions, including 4855b785… which the network reports as success
6 sortitions across 2 distinct miners
reward cycle 18 pays a waterfall set in which nano holds weight 10 of 30
```

The consecutive runs of mined heights are whole tenures: nano keeps building on
its own tip for as long as it owns the tenure, confirming what the mempool holds.

Every signature is recovered from the block header and checked against the reward
set, every miner signature is recovered the same way, and every receipt is read
back from the indexer. A signer-only run reports the same lines without the
mining one.

## Exercising a long tenure

Hacknet produces a sortition roughly every ten seconds, so a tenure rarely lasts
long enough to need extending. Stopping the Bitcoin miner while nano owns the
tenure is what a slow burnchain looks like:

```
docker compose -p hacknet -f docker/docker-compose.yml stop bitcoin-miner
```

nano then keeps confirming what the mempool holds, and once the tenure has run
past the idle timeout its signers offer an extension after, it says so on chain.
A recorded run produced eighteen blocks in one tenure and a `tenure_change` with
cause `extended` at height 292, all accepted by the stock signers.

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
