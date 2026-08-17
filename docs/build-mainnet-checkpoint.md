# Build a mainnet checkpoint from the Hiro archive

Yes, you can use a [Hiro mainnet archive][hiro-archive]. The archive is a saved
stacks-core data directory. It is not a ready nano-stacks checkpoint. The
`capture-fixtures` command converts it. Hiro explains the archive format in its
[archive guide][hiro-guide].

The archive is large. Keep the archive file, the extracted data, and the new
checkpoint on a disk with at least 1 TB free. Do not use the system disk.

The commands below use the latest archive. Run them from the nano-stacks
repository root.

## 1. Set the paths

Choose a new work directory on the large disk:

```sh
export NANO_CHECKPOINT_WORK=/srv/nano-checkpoint-build
export HIRO_ARCHIVE_URL=https://archive.hiro.so/mainnet/stacks-blockchain/mainnet-stacks-blockchain-latest.tar.zst
export HIRO_SHA_URL=https://archive.hiro.so/mainnet/stacks-blockchain/mainnet-stacks-blockchain-latest.sha256
export HIRO_ARCHIVE_FILE="$NANO_CHECKPOINT_WORK/mainnet-stacks-blockchain-latest.tar.zst"
export HIRO_SHA_FILE="$HIRO_ARCHIVE_FILE.sha256"
export HIRO_EXTRACT_ROOT="$NANO_CHECKPOINT_WORK/extracted"
export NANO_CAPTURE_ROOT="$NANO_CHECKPOINT_WORK/nano-capture"

mkdir -p "$NANO_CHECKPOINT_WORK" "$HIRO_EXTRACT_ROOT"
test ! -e "$NANO_CAPTURE_ROOT"
mkdir "$NANO_CAPTURE_ROOT"
```

Use a new `NANO_CAPTURE_ROOT`. The converter replaces capture directories with
the same names.

## 2. Download and check the archive

The download can restart after a network failure:

```sh
nix develop --command curl \
  --fail \
  --location \
  --retry 20 \
  --retry-all-errors \
  --continue-at - \
  --output "$HIRO_ARCHIVE_FILE" \
  "$HIRO_ARCHIVE_URL"

nix develop --command curl \
  --fail \
  --location \
  --output "$HIRO_SHA_FILE" \
  "$HIRO_SHA_URL"

nix develop --command bash -c '
  archive_hash="$(awk "NR == 1 {print \$1}" "$HIRO_SHA_FILE")"
  printf "%s  %s\n" "$archive_hash" "$HIRO_ARCHIVE_FILE" |
    sha256sum --check -
'
```

The last command must print `OK`. This check finds a broken download. It does
not prove that the chain data is correct. Hiro made both the archive and its
SHA-256 file.

## 3. Extract the stacks-core data

```sh
nix develop --command tar \
  --use-compress-program=unzstd \
  -xf "$HIRO_ARCHIVE_FILE" \
  -C "$HIRO_EXTRACT_ROOT"

export HIRO_NODE_ROOT="$HIRO_EXTRACT_ROOT/mainnet"

test -f "$HIRO_NODE_ROOT/chainstate/vm/index.sqlite"
test -f "$HIRO_NODE_ROOT/chainstate/blocks/nakamoto.sqlite"
test -f "$HIRO_NODE_ROOT/burnchain/sortition/marf.sqlite"
```

Stop here if any `test` command fails. Find the directory that contains both
`chainstate` and `burnchain`, then set `HIRO_NODE_ROOT` to that directory.

## 4. Choose the checkpoint height

Keep the last 100 blocks as replay data. The converter uses these blocks to
check that nano-stacks can continue from the checkpoint.

```sh
export NANO_ARCHIVE_TIP="$(nix develop --command sqlite3 \
  "$HIRO_NODE_ROOT/chainstate/blocks/nakamoto.sqlite" \
  'SELECT MAX(height) FROM nakamoto_staging_blocks WHERE processed = 1 AND orphaned = 0;')"

test "$NANO_ARCHIVE_TIP" -gt 100
export NANO_CHECKPOINT_HEIGHT="$((NANO_ARCHIVE_TIP - 100))"
export NANO_FIRST_HEIGHT="$((NANO_CHECKPOINT_HEIGHT + 1))"

printf 'archive tip: %s\ncheckpoint: %s\nfirst replay block: %s\n' \
  "$NANO_ARCHIVE_TIP" \
  "$NANO_CHECKPOINT_HEIGHT" \
  "$NANO_FIRST_HEIGHT"
```

## 5. Check that the archive and Hiro API use the same release

The converter gets raw blocks and PoX data from a Stacks API. Use the Hiro API
only for this conversion source. The independent signer check comes later.

```sh
export HIRO_STACKS_RPC=https://api.mainnet.hiro.so
export BITCOIN_ESPLORA=https://blockstream.info/api

export HIRO_ARCHIVE_VERSION="$(nix develop --command sed -n \
  's|.*mainnet-stacks-blockchain-\([0-9][0-9.]*\)-[0-9][0-9]*\.tar\.zst.*|\1|p' \
  "$HIRO_SHA_FILE")"
export HIRO_SERVER_VERSION="$(nix develop --command bash -c \
  'curl -fsS "$HIRO_STACKS_RPC/v2/info" | jq -r .server_version')"
export HIRO_RPC_VERSION="$(printf '%s\n' "$HIRO_SERVER_VERSION" | awk '{print $2}')"
export HIRO_STACKS_REVISION="$(printf '%s\n' "$HIRO_SERVER_VERSION" |
  sed -n 's/.*(\([^,]*\),.*/\1/p')"

test -n "$HIRO_ARCHIVE_VERSION"
test "$HIRO_ARCHIVE_VERSION" = "$HIRO_RPC_VERSION"
test -n "$HIRO_STACKS_REVISION"
printf 'archive and API release: %s, revision: %s\n' \
  "$HIRO_ARCHIVE_VERSION" \
  "$HIRO_STACKS_REVISION"
```

Stop if the versions differ. Wait for Hiro to publish matching data, or use a
Stacks API that runs the archive release.

The example uses the public Blockstream Esplora service for a small Bitcoin
block range. Set `BITCOIN_ESPLORA` to your own Esplora service if the public
service is slow or blocks the requests.

## 6. Convert the archive

This command can take hours:

```sh
nix develop --command cargo xtask capture-fixtures \
  --out-dir "$NANO_CAPTURE_ROOT" \
  --state-dir "$HIRO_NODE_ROOT" \
  --node-root "$HIRO_NODE_ROOT" \
  --stacks-rpc "$HIRO_STACKS_RPC" \
  --bitcoin-rest "$BITCOIN_ESPLORA" \
  --hacknet-commit "hiro-mainnet-archive" \
  --accept-node-revision "$HIRO_STACKS_REVISION" \
  --first-height "$NANO_FIRST_HEIGHT" \
  --replay-blocks 100 \
  --checkpoint-height "$NANO_CHECKPOINT_HEIGHT" \
  --bitcoin-magic X2 \
  --pox-v1-unlock-height 781552 \
  --pox-v2-unlock-height 787652 \
  --pox-v3-unlock-height 840361 \
  --pox-v4-unlock-height 960230 \
  --sbtc-registry-contract \
    SM3VDXK3WZZSA84XXFKAFAF15NNZX32CTSG82JFQ4.sbtc-registry
```

`--hacknet-commit` is an old option name. Here its value is only a source name
in `provenance.toml`.

Do not add `--full-sortition-history true`. The normal command saves the needed
Bitcoin window and all leader keys. A full history needs much more time and
network data.

Check the capture:

```sh
nix develop --command env NANO_FIXTURES="$NANO_CAPTURE_ROOT" \
  cargo xtask validate-fixtures
```

## 7. Make one checkpoint bundle

The converter writes the checkpoint state, the first block, and the sortition
data in separate capture directories. Copy the last two into the checkpoint
directory:

```sh
export NANO_CHECKPOINT_DIR="$NANO_CAPTURE_ROOT/chainstate/checkpoint-H"
export NANO_ANCHOR_PREFIX="$(printf '%08d' "$NANO_FIRST_HEIGHT")"
export NANO_ANCHOR_SOURCE="$(find \
  "$NANO_CAPTURE_ROOT/nakamoto/blocks" \
  -maxdepth 1 \
  -type f \
  -name "${NANO_ANCHOR_PREFIX}-*.bin" \
  -print \
  -quit)"

test -n "$NANO_ANCHOR_SOURCE"
test -f "$NANO_CHECKPOINT_DIR/block.bin"
test ! -e "$NANO_CHECKPOINT_DIR/sortition"

cp "$NANO_ANCHOR_SOURCE" "$NANO_CHECKPOINT_DIR/anchor-block.bin"
cp -R "$NANO_CAPTURE_ROOT/sortition" "$NANO_CHECKPOINT_DIR/sortition"
```

## 8. Get the signer set from another source

Do not use Hiro for this step. The commands below ask two public stacks-core
nodes. Their host names are outside `hiro.so`, but they may have one operator.
For stronger trust, replace them with nodes that you or other known operators
run.

These public RPC URLs use plain HTTP. Use a trusted network. For a stronger
check, use your own nodes through HTTPS, a VPN, or an SSH tunnel.

First find the reward cycle of the checkpoint block:

```sh
export NANO_CHECKPOINT_CONSENSUS="$(nix develop --command sqlite3 \
  "$HIRO_NODE_ROOT/chainstate/blocks/nakamoto.sqlite" \
  "SELECT consensus_hash FROM nakamoto_staging_blocks WHERE height = $NANO_CHECKPOINT_HEIGHT AND processed = 1 AND orphaned = 0 LIMIT 1;")"
export NANO_CHECKPOINT_BURN="$(nix develop --command sqlite3 \
  "$HIRO_NODE_ROOT/burnchain/sortition/marf.sqlite" \
  "SELECT block_height FROM snapshots WHERE consensus_hash = '$NANO_CHECKPOINT_CONSENSUS' AND pox_valid = 1 ORDER BY block_height DESC LIMIT 1;")"
test -n "$NANO_CHECKPOINT_BURN"
test "$NANO_CHECKPOINT_BURN" -ge 666050
export NANO_REWARD_CYCLE="$(((NANO_CHECKPOINT_BURN - 666050) / 2100))"

printf 'checkpoint burn height: %s\nreward cycle: %s\n' \
  "$NANO_CHECKPOINT_BURN" \
  "$NANO_REWARD_CYCLE"
```

Now download the same set from both nodes:

```sh
export NANO_REWARD_SET_CET="$NANO_CHECKPOINT_WORK/reward-set-cet.json"
export NANO_REWARD_SET_SGT="$NANO_CHECKPOINT_WORK/reward-set-sgt.json"

nix develop --command curl -fsS \
  "http://cet.stacksnodes.org:20443/v3/stacker_set/$NANO_REWARD_CYCLE" \
  -o "$NANO_REWARD_SET_CET"
nix develop --command curl -fsS \
  "http://sgt.stacksnodes.org:20443/v3/stacker_set/$NANO_REWARD_CYCLE" \
  -o "$NANO_REWARD_SET_SGT"

export NANO_CET_SET_HASH="$(nix develop --command bash -c \
  'jq -S -c . "$NANO_REWARD_SET_CET" | sha256sum | awk "{print \$1}"')"
export NANO_SGT_SET_HASH="$(nix develop --command bash -c \
  'jq -S -c . "$NANO_REWARD_SET_SGT" | sha256sum | awk "{print \$1}"')"
test "$NANO_CET_SET_HASH" = "$NANO_SGT_SET_HASH"

cp "$NANO_REWARD_SET_CET" "$NANO_CHECKPOINT_DIR/reward-set.json"
```

Stop if the hashes differ. Do not choose one set without more checks.

Check that the independent set accepts the signed checkpoint block:

```sh
nix develop --command cargo xtask verify-block \
  "$NANO_CHECKPOINT_DIR/block.bin" \
  "$NANO_CHECKPOINT_DIR/reward-set.json"
```

The command must print `accepted with weight` and exit with status 0.

## 9. Build the content manifest with local Bitcoin Core

The Esplora service used during capture is not accepted as checkpoint trust.
Use the builder's own synced Bitcoin Core for the exact checkpoint-height hash:

```sh
export NANO_BITCOIN_RPC=http://127.0.0.1:8332
export NANO_BITCOIN_USER=nano-checkpoint
export NANO_BITCOIN_PASSWORD_FILE=/run/secrets/nano-bitcoin-rpc-password

nix develop --command cargo build --release -p nano-node --bin stacks-node
./target/release/stacks-node build-checkpoint-manifest \
  --bundle "$NANO_CHECKPOINT_DIR" \
  --bitcoin-rpc-url "$NANO_BITCOIN_RPC" \
  --bitcoin-rpc-user "$NANO_BITCOIN_USER" \
  --bitcoin-rpc-password-file "$NANO_BITCOIN_PASSWORD_FILE"
```

The command recomputes the block/reward-set threshold and active compiler
profile before writing the new `checkpoint-bundle.toml`. It refuses to replace
an existing manifest.

## 10. Rebuild independently

The Hiro procedure above produces one builder's candidate. It is not sufficient
release evidence by itself. At least one other builder in a distinct failure
domain must independently acquire an archive or node state, convert it, obtain
the reward set and Bitcoin header view independently, and run the same manifest
command in a different directory.

Exchange only these results first:

```sh
sha256sum "$NANO_CHECKPOINT_DIR/checkpoint-bundle.toml"
sed -n 's/^content_root = "\([0-9a-f]*\)"/\1/p' \
  "$NANO_CHECKPOINT_DIR/checkpoint-bundle.toml"
```

Both the complete manifest bytes and content root must agree. A matching root
from two processes which share the same archive, host, storage or operator is a
useful reproducibility check but not independent failure-domain evidence. Stop
on any mismatch; do not choose one candidate by majority or edit either
manifest.

## 11. Sign and publish the agreed manifest

Each builder uses an independently held key named by the operator-pinned
policy. Start from
[`checkpoint-builders.example.toml`](checkpoint-builders.example.toml), replace
every example key, and keep the policy and signatures outside the bundle:

```sh
export NANO_BUILDER_POLICY=/srv/nano-checkpoints/policy/builders.toml
export NANO_BUILDER_SIGNATURES=/srv/nano-checkpoints/$NANO_CHECKPOINT_HEIGHT/signatures

./target/release/stacks-node sign-checkpoint-manifest \
  --bundle "$NANO_CHECKPOINT_DIR" \
  --policy "$NANO_BUILDER_POLICY" \
  --signatures "$NANO_BUILDER_SIGNATURES" \
  --builder archive-east \
  --private-key /run/secrets/archive-east-checkpoint-key \
  --bitcoin-rpc-url "$NANO_BITCOIN_RPC" \
  --bitcoin-rpc-user "$NANO_BITCOIN_USER" \
  --bitcoin-rpc-password-file "$NANO_BITCOIN_PASSWORD_FILE"
```

The signature file is created once and never replaced. Publish the manifest and
signatures in append-only release storage, and distribute the builder policy
through a separately authenticated operator channel. Never publish the private
key.

## 12. Verify and use the bundle

A fresh operator verifies every byte, local Bitcoin view and builder threshold
without opening node state:

```sh
./target/release/stacks-node verify-checkpoint \
  --bundle "$NANO_CHECKPOINT_DIR" \
  --policy "$NANO_BUILDER_POLICY" \
  --signatures "$NANO_BUILDER_SIGNATURES" \
  --bitcoin-rpc-url "$NANO_BITCOIN_RPC" \
  --bitcoin-rpc-user "$NANO_BITCOIN_USER" \
  --bitcoin-rpc-password-file "$NANO_BITCOIN_PASSWORD_FILE"
```

The finished bundle now has all paths used by
[`mainnet-node.example.toml`](mainnet-node.example.toml), plus
`checkpoint-bundle.toml`. Configure `checkpoint.bundle`, `builder_policy` and
`builder_signatures` as well as the payload paths below.

Copy the example and set these values:

| Config value | File |
|---|---|
| `checkpoint.marf` | `marf.sqlite` |
| `checkpoint.anchor_block` | `anchor-block.bin` |
| `checkpoint.tenure_accounting` | `native-effects.json` |
| `checkpoint.attesting_block` | `block.bin` |
| `checkpoint.attesting_reward_set` | `reward-set.json` |
| `checkpoint.sortition` | `sortition/` |
| `checkpoint.authentication_history` | `authentication-history/` |

Copy `source_state_id`, `published_state_index_root`, and
`first_bitcoin_height` from `checkpoint.toml` as explained in
[`Run a mainnet node`](running-a-mainnet-node.md).

Keep the downloaded and extracted data until nano-stacks imports the checkpoint
and executes new blocks. Retain at least one complete bundle, policy version and
all signatures in archival storage. After a clean post-import restart, the node
host may discard the large source MARF, but it must keep the small configured
evidence and the external signing evidence available.

## Trust limit

Hiro publishes the archive and its checksum. This gives one source for both
items. The separate reward-set check proves that the checkpoint block has
enough signer weight. nano-stacks also rebuilds the state root during import.
Only an independently acquired second build and threshold builder signatures
turn this one-source procedure into release evidence.

These checks lower the risk, but they do not remove all trust. Read
[`Checkpoint trust`](checkpoint-trust.md) before you use this data.

[hiro-archive]: https://archive.hiro.so/mainnet/stacks-blockchain/
[hiro-guide]: https://www.hiro.so/blog/sync-your-stacks-node-and-api-services-faster-with-the-hiro-archive
