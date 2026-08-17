# Checkpoint trust and recovery

nano supports Epoch 4.0 only. It cannot replay mainnet from genesis, so its
initial state is a checkpoint built by somebody else. A wrong checkpoint makes
every later state wrong. Mainnet therefore requires three independent proofs
before the node creates or imports production state.

## What is authenticated

`checkpoint.toml` names the state:

```toml
format = "stacks-core-marf-sqlite-v2"
checkpoint_stacks_height = 8665600
source_state_id = "a87338900f279efc1b1df130004238cac8e09a2a4244fea39436fc66afae932d"
published_state_index_root = "67596465d4a6642ad6fcec1df57c6ef758fcdb0003c7ed7f952e3ced1d7f44ec"
first_bitcoin_height = 960231
profile_fingerprint = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
```

`checkpoint-bundle.toml` then binds every regular payload file by portable
path, exact byte length, whole-file SHA-256 and fixed 4 MiB chunk hashes. Its
domain-separated `content_root` also binds the state claims, exact locally
observed Bitcoin block hash, checkpoint block and signer threshold, Epoch40,
compiler identity and compatibility profile. Missing, extra, changed,
truncated, symlinked and special files are refused.

The import separately recomputes the trie root. The following three values must
agree:

- the root published in `checkpoint.toml`;
- the root selected in the operator configuration;
- the root computed from the imported trie.

That proves byte integrity and self-consistency, but not that the network ever
accepted the state. Two independent checks supply that evidence.

## Network attestation

The Nakamoto block at the checkpoint height commits to `state_index_root` in
the preimage signed by that cycle's reward set. nano requires:

- the block height, block ID and state root to equal the checkpoint claims;
- unique, reward-set-ordered signatures reaching the network threshold;
- the reward set to be obtained independently of the state it attests;
- the Bitcoin block hash at the checkpoint's exact burn height to equal the
  answer from the operator's locally verified Bitcoin Core.

The reward set must not be derived from the checkpoint being authenticated.
That would let one producer invent both the state and the keys which approve
it. A hosted Stacks API may help acquire data, but it is not a consensus trust
root. Mainnet signed-bundle verification deliberately refuses Esplora and
requires local Bitcoin Core for the Bitcoin view.

## Independent builders

The bundle does not declare who is trusted to build it. The operator pins a
separate builder policy and a threshold of distinct builders. Each builder
signs the content root with a height-valid key. The signature command creates
one new file and never replaces it; the policy and signature directory must be
outside the bundle so the payload cannot declare its own trust.

Use builders which acquire and construct the checkpoint in distinct failure
domains. Two processes reading the same archive, host, disk or operator are a
reproducibility test, not independent evidence. Before signing, builders must
compare the complete `checkpoint-bundle.toml` bytes and content root.

The policy schema is shown in
[`checkpoint-builders.example.toml`](checkpoint-builders.example.toml). Entries
are sorted by builder name and `valid_from_height`. Rotation uses adjacent,
non-overlapping height ranges. To revoke a key, publish a new operator-pinned
policy with `revoked_from_height` set to the first checkpoint height that must
reject it. Never edit or delete an already published signature; publish the new
policy and new signatures beside the old evidence.

## Build, sign and verify

Build the release binary, then set paths to local Bitcoin Core and evidence
which is outside the bundle:

```sh
nix develop --command cargo build --release -p nano-node --bin stacks-node

export NANO_BUNDLE=/srv/nano-checkpoints/8665600/bundle
export NANO_BUILDER_POLICY=/srv/nano-checkpoints/policy/builders.toml
export NANO_BUILDER_SIGNATURES=/srv/nano-checkpoints/8665600/signatures
export NANO_BITCOIN_RPC=http://127.0.0.1:8332
export NANO_BITCOIN_USER=nano-checkpoint
export NANO_BITCOIN_PASSWORD_FILE=/run/secrets/nano-bitcoin-rpc-password
```

Each independent builder assembles a new directory without a manifest and
runs:

```sh
./target/release/stacks-node build-checkpoint-manifest \
  --bundle "$NANO_BUNDLE" \
  --bitcoin-rpc-url "$NANO_BITCOIN_RPC" \
  --bitcoin-rpc-user "$NANO_BITCOIN_USER" \
  --bitcoin-rpc-password-file "$NANO_BITCOIN_PASSWORD_FILE"
```

Compare the resulting manifest bytes and reported content root out of band.
Only after they agree does each builder sign its independently built copy:

```sh
./target/release/stacks-node sign-checkpoint-manifest \
  --bundle "$NANO_BUNDLE" \
  --policy "$NANO_BUILDER_POLICY" \
  --signatures "$NANO_BUILDER_SIGNATURES" \
  --builder archive-east \
  --private-key /run/secrets/archive-east-checkpoint-key \
  --bitcoin-rpc-url "$NANO_BITCOIN_RPC" \
  --bitcoin-rpc-user "$NANO_BITCOIN_USER" \
  --bitcoin-rpc-password-file "$NANO_BITCOIN_PASSWORD_FILE"
```

The private-key file is 32-byte lowercase hexadecimal and must never be stored
in the bundle, signature directory or repository. Publish the immutable bundle
manifest and signatures through append-only release storage. Publish the
operator policy through a separately authenticated channel.

A fresh operator verifies everything without opening node state:

```sh
./target/release/stacks-node verify-checkpoint \
  --bundle "$NANO_BUNDLE" \
  --policy "$NANO_BUILDER_POLICY" \
  --signatures "$NANO_BUILDER_SIGNATURES" \
  --bitcoin-rpc-url "$NANO_BITCOIN_RPC" \
  --bitcoin-rpc-user "$NANO_BITCOIN_USER" \
  --bitcoin-rpc-password-file "$NANO_BITCOIN_PASSWORD_FILE"
```

Verification reads every payload byte, recomputes the signer proof and active
profile, checks the local Bitcoin view, and verifies the builder threshold. It
writes no node state. Startup repeats this verification before first import.

## Provenance and restart

Before import, nano durably writes immutable `checkpoint-provenance.toml` into
the state directory:

```toml
[checkpoint]
format = "stacks-core-marf-sqlite-v2"
checkpoint_stacks_height = 8665600
source_state_id = "a87338900f279efc1b1df130004238cac8e09a2a4244fea39436fc66afae932d"
published_state_index_root = "67596465d4a6642ad6fcec1df57c6ef758fcdb0003c7ed7f952e3ced1d7f44ec"
first_bitcoin_height = 960231
profile_fingerprint = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[attestation]
attesting_block_id = "a87338900f279efc1b1df130004238cac8e09a2a4244fea39436fc66afae932d"
signer_weight = 2708
approval_threshold = 2599

[bundle]
content_root = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
bitcoin_height = 960231
bitcoin_block_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
builders = ["archive-east", "archive-west"]
```

A restart refuses a different checkpoint, profile or trust receipt. It rechecks
the persisted content root against the current external builder policy,
signature threshold and local Bitcoin view, but does not hash the imported MARF
again. The durable state records its own profile and network and refuses a
mismatch. Legacy mainnet state without signed-bundle provenance must be imported
again; it is not silently upgraded.

## Retention and recovery

Keep the following indefinitely as release evidence:

- `checkpoint.toml`, `checkpoint-bundle.toml`, `block.bin` and the independent
  reward set;
- the builder policy version used, every builder signature and their publication
  timestamps or object versions;
- the release binary identity and exact verification command/output;
- at least one complete bundle in archival storage.

Keep the local bundle until the import finishes, the first post-checkpoint block
is sealed, and a clean restart succeeds. The large imported MARF source can then
be removed from the node host; retain the small configured evidence and an
archival copy for disaster recovery.

If import is interrupted, nano leaves
`checkpoint-import-unfinished.toml` and refuses the directory. Do not remove the
marker or resume the partial files. Remove the entire target state directory,
verify the archived bundle again, and import into a new empty directory. The
same procedure applies to corrupted state: preserve it for diagnosis, restore
no individual database files, and reimport into a different directory.

## New and incrementally distributed checkpoints

A new checkpoint is a new immutable release:

1. build it independently from at least two separately acquired archives or
   nodes;
2. compare state root, manifest bytes and content root;
3. sign the new root with keys active at the new checkpoint height;
4. publish the bundle, policy version and signatures without changing the old
   release;
5. verify and import it into a new node working directory;
6. follow and execute from the checkpoint using local Bitcoin and Stacks P2P.

Chunk hashes allow transport systems to fetch or deduplicate pieces
incrementally, but nano does not apply an incremental state patch. The complete
new bundle must verify before import. A compiler/profile change likewise uses a
new checkpoint and full replay; opening old state is never an implicit
migration.

## Remaining trust and failure behavior

No consensus rule commits to a particular archival MARF encoding. The signed
header authenticates the state root, and the importer proves the supplied graph
hashes to it. Independent builders reduce archive and construction risk. This
still rests on the chain's hash and signature assumptions.

Every later block is checked against the state root in its signed header. A bad
checkpoint therefore stops at the first disagreement; nano does not fall back
to another engine or continue serving a fork. Treat a first-block root mismatch
as failed checkpoint evidence until independently disproved.
