# Run a mainnet node

> [!WARNING]
> nano-stacks is experimental. Its mainnet release tests are not complete. Do
> not use it for money or a service that must stay online.

This guide starts a mainnet follower. The follower downloads, checks, executes,
and serves Stacks data. It does not mine or sign.

The node starts from an Epoch 4 checkpoint. It cannot start from genesis.

## What you need

You need:

- a Linux host with Nix and Git;
- a large local disk;
- a complete mainnet checkpoint bundle;
- a synced Bitcoin Core RPC, or a Bitcoin Esplora endpoint;
- outbound TCP access to Stacks peers on port `20444`.

A mainnet checkpoint can use hundreds of gigabytes. The first import can take
hours. Keep the checkpoint and the node state on a disk with enough free space.

This repository does not publish a ready mainnet checkpoint. You can convert a
[Hiro mainnet archive](build-mainnet-checkpoint.md) into a bundle. Get its
signer reward set from a different source. Read
[Checkpoint trust](checkpoint-trust.md) before you use the bundle.

## Check the checkpoint bundle

The bundle must contain these files:

```text
mainnet-checkpoint/
├── checkpoint.toml
├── marf.sqlite
├── marf.sqlite.blobs
├── block-headers.sqlite
├── anchor-block.bin
├── block.bin
├── reward-set.json
├── native-effects.json
├── sortition/
│   ├── snapshots.json
│   ├── consensus-hashes.json
│   └── leader-keys.json
└── authentication-history/
    ├── boundary.json
    └── blocks/
        └── *.bin
```

Get `reward-set.json` without using the checkpoint as its source. It must be the
signer set for the cycle that signed `block.bin`.

The files have these roles:

| File | Use |
|---|---|
| `checkpoint.toml` | Names the checkpoint height, state ID, root, and Bitcoin height. |
| `marf.sqlite` and `marf.sqlite.blobs` | Hold the Clarity state. |
| `block-headers.sqlite` | Holds old block data used by Clarity reads. |
| `anchor-block.bin` | Is the first Stacks block after the checkpoint. |
| `block.bin` | Is the signed block that sealed the checkpoint. |
| `reward-set.json` | Checks the signatures on the checkpoint block. |
| `native-effects.json` | Holds rewards that will mature after the checkpoint. |
| `sortition/` | Holds the local Bitcoin and Stacks fork history. |
| `authentication-history/` | Holds the block proof before the checkpoint. |

Do not build a reward set from the checkpoint that it will check. That check is
circular and gives no trust.

## Build the node

Run these commands from the repository root:

```sh
nix develop --command cargo build --release -p nano-node --bin stacks-node
```

The binary is `target/release/stacks-node`.

## Write the configuration

Copy the example:

```sh
cp docs/mainnet-node.example.toml mainnet-node.toml
```

Edit `mainnet-node.toml`:

1. Set `node.working_dir` to a new, empty directory. The node user must be able
   to write there.
2. Set the Bitcoin RPC URL, user, and password.
3. Set every checkpoint path.
4. Copy `source_state_id` and `published_state_index_root` from
   `checkpoint.toml`. Put the second value in `state_root`.
5. Copy `first_bitcoin_height` from `checkpoint.toml` to
   `anchor_bitcoin_height`.
6. Set the path to the independent `reward-set.json`.

Keep the configuration file private. It contains the Bitcoin RPC password.

The example uses a local Bitcoin Core node. To use Esplora, remove `rpc_url`,
`rpc_user`, and `rpc_password`, then add:

```toml
rest_url = "https://your-esplora.example/api"
```

The node uses the built-in mainnet P2P seeds when `p2p_seeds` is absent. It
does not need a hosted Stacks HTTP API. The example is outbound-only. Read
[Joining the Stacks peer network](joining-the-peer-network.md) before you open
P2P or RPC ports.

## Check the configuration

Run:

```sh
./target/release/stacks-node check-config --config mainnet-node.toml
```

This checks the TOML values. It does not read all checkpoint files. The first
start checks the checkpoint manifest, state root, block signatures, reward set,
and history.

## Start the node

Run the node in the foreground for the first start:

```sh
./target/release/stacks-node start --config mainnet-node.toml
```

The first start imports the checkpoint. Do not stop the process during the
import. Wait for the log to show that the checkpoint passed attestation and
that blocks are being executed.

The node stores all new files under `node.working_dir`. Never run two node
processes with the same working directory.

## Check node health

The example serves metrics on local port `9153`. Show the three chain heights:

```sh
curl -fsS http://127.0.0.1:9153/metrics |
  rg 'nano_(selected|followed|executed)_stacks_height'
```

- `selected` is the best tip found from peers.
- `followed` is the downloaded tip.
- `executed` is the tip checked and saved by this node.

During sync, `executed` is lower than the other heights. It must keep moving.
At tip, the three heights should stay close. A fixed `executed` height with a
moving `followed` height means that execution is blocked. Read the node log for
the first block refusal.

The log also shows the connected peer count and the peer used for each tenure.
See [Joining the Stacks peer network](joining-the-peer-network.md#reading-the-log)
for common peer errors.

## Stop and restart

Use `Ctrl-C` in a terminal. A service manager should send `SIGTERM`. Wait for
the process to exit before you copy or back up its state.

Restart with the same command and the same configuration:

```sh
./target/release/stacks-node start --config mainnet-node.toml
```

The node reads the saved tip and continues. It does not import the checkpoint
again.

Keep `p2p-seed` under the working directory. It is the node identity. Do not
copy one working directory to two live nodes.

## Current limits

- The repository has source code only. It has no release package, container,
  or service file.
- The repository has no ready mainnet checkpoint bundle. The
  [Hiro archive guide](build-mainnet-checkpoint.md) shows how to build one.
- The full checkpoint-to-tip P2P test is still open in
  [task 054](../tasks/054-join-and-synchronize-over-the-stacks-p2p-network.md).
- The mainnet release gate is still open in
  [task 053](../tasks/053-pass-the-mainnet-node-release-gate.md).
- The 24-hour mainnet tip test is still open in
  [task 106](../tasks/mainnet/106-hold-the-release-candidate-at-mainnet-tip-for-24-h.md).

These limits are why this guide is for testing, not production use.
