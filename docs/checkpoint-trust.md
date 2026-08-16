# What you trust when you start nano from a checkpoint

nano is an epoch-4.0-only node. It has no code for epochs 2.05 through 3.4 and
no boot contract sources, so it cannot build the Stacks state from genesis. The
state it runs on always arrives as a file somebody else produced: a checkpoint,
exported from a stacks-core MARF at some Stacks height H.

Every block nano executes afterwards is validated against that state. If the
state is wrong, everything after it is wrong. This document says what nano
checks, what it cannot check, and what is therefore left for the operator to
decide.

## The claims a checkpoint makes

A checkpoint directory carries a `checkpoint.toml` next to its MARF:

```toml
format = "stacks-core-marf-sqlite-v2"
checkpoint_stacks_height = 400
source_state_id = "59fddf16…"          # the block_id of the block that sealed the state
published_state_index_root = "34644adb…" # the state root at that block
first_bitcoin_height = 277
profile_fingerprint = "0123456789abcdef…" # exact `stacks-node compatibility-profile` fingerprint
```

Three separate assertions live in a running node, and they are not the same
thing:

- **the published root** — what the checkpoint's own manifest says;
- **the declared root** — what the operator's configuration says;
- **the computed root** — what nano recomputes by hashing the imported trie
  graph.

The import refuses to proceed unless all three agree, and says which pair
disagreed: `DeclaredRootMismatch` when the configuration and the checkpoint's
manifest name different roots for the state the checkpoint publishes,
`RootMismatch` when the trie graph does not hash to the root that was asked for.

Mainnet import also requires `profile_fingerprint` to equal the profile embedded
in the release artifact. That fingerprint covers the declared Epoch-4 domain,
the clarity-wasm compiler and the exact Wasmtime version/configuration. Missing
or different values are refused before any state is copied. A compiler or
consensus-profile upgrade therefore uses a new directory and a complete replay;
opening an existing state is never an implicit migration.

That agreement is worth having, but it is only self-consistency. A checkpoint
fabricated end to end — a doctored trie whose root is recomputed and written
into its own manifest and into the operator's configuration — passes all three.

## The one external check: the signed header

`signer_signature_hash` is taken over a preimage that contains
`state_index_root`, and `block_id` is derived from that hash together with the
consensus hash. So the Nakamoto block header at height H is a statement, signed
by the reward set of that cycle with at least 70% of its weight, that the state
root at H was exactly this value.

`nano_node::attest_checkpoint` checks a checkpoint's manifest against such a
header:

- `chain_length` equals the checkpoint's Stacks height;
- `block_id()` equals `source_state_id`;
- `state_index_root` equals `published_state_index_root`;
- `SignerSet::verify` recovers unique, reward-set-ordered signatures carrying at
  least the approval threshold.

The header is fetched by identifier — `GET /v3/blocks/<source_state_id>` — so
any peer can serve it and no peer can substitute a different block: the
identifier is the hash of the thing being asked for.

This moves the root from "a number in a configuration file" to "a number the
chain's signers put threshold weight behind".

## Where the trust actually sits

**The reward set has to come from somewhere other than the checkpoint.** The
signer set for a cycle is derived from `.pox-5` state — that is, from the state
being bootstrapped. Deriving it from the imported checkpoint and then using it to
attest that same checkpoint is circular: whoever fabricated the state can
fabricate its signer set and sign a matching header with the keys they invented.

So attestation is only as strong as the independence of the reward set. In
descending order of what it buys you:

1. **Reward set pinned in configuration**, taken out of band (a release
   artifact, a block explorer, several unrelated peers that agree). The
   attestation is then a genuine external check on the checkpoint.
2. **Reward set fetched from peers** via `/v3/stacker_set/<cycle>`. Sound as long
   as the peers are not the same party that supplied the checkpoint. Fetch from
   more than one and require agreement.
3. **Reward set derived from the checkpoint itself.** Circular; it proves the
   checkpoint is internally consistent and nothing more. Do not treat this as
   attestation.

**The checkpoint is not verifiable in-protocol at all in one respect.** PCS
export is out of protocol: no consensus rule commits to a squashed archival
MARF, so there is no rule that can be checked. The header attestation is a check
on the *root*, and the recomputation is a check that the *graph* hashes to that
root. Together they leave one unfalsifiable assumption — that Sha512/256 is not
broken — which is the same assumption the chain itself rests on.

**Divergence is loud, and quickly.** `append_block` compares the state root nano
computes for every block against the `state_index_root` in that block's signed
header and rejects the block on mismatch. A wrong checkpoint therefore fails on
the first block nano executes after it, not silently later. This is the backstop
that makes a bad checkpoint an outage rather than a fork: nano stops, it does not
serve wrong answers. Operators should treat "first block after import fails to
match" as "the checkpoint is wrong", not as a nano bug.

## Recommended procedure for a mainnet checkpoint

1. Obtain the checkpoint and its `checkpoint.toml` from a source you would trust
   with a binary — the same bar, since it is the same power.
2. Obtain the reward set for the cycle containing the checkpoint independently
   of that source, and pin it.
3. Start nano. Before importing it fetches the header at `source_state_id`,
   attests it against the pinned reward set and records what it found
   (`nano_node::adopt_checkpoint`); the import then recomputes the root from the
   trie graph.
4. Watch the first tenure execute. A checkpoint that survives one block of
   execution against signed headers is a checkpoint the network agrees with.

## Provenance

nano writes `checkpoint-provenance.toml` into its state directory when it
imports, holding the manifest it imported and the attestation it obtained:

```toml
[checkpoint]
format = "stacks-core-marf-sqlite-v2"
checkpoint_stacks_height = 400
source_state_id = "59fddf16…"
published_state_index_root = "34644adb…"
first_bitcoin_height = 277
profile_fingerprint = "0123456789abcdef…"

[attestation]
attesting_block_id = "59fddf16…"
signer_weight = 12
approval_threshold = 9
```

A restart reads it back. State on disk descends from the checkpoint it was
imported from, so a directory that already names a different checkpoint is
refused (`ProvenanceMismatch`) rather than resumed — otherwise editing the
configured checkpoint would graft one chain's blocks onto another chain's state,
and nothing downstream could tell. An absent `[attestation]` section is the
record that the state was taken on the operator's word alone.

## Why boot contract sources are not embedded

The alternative trust root would be building the state ourselves: embed the boot
`.clar` sources, run genesis, replay to the 4.0 boundary, and compare the root.
We are not doing that, for three reasons.

*It would not be an independent check.* Reaching the 4.0 boundary from genesis
means executing epochs 2.0 through 3.4 bit-exactly: microblocks, PoX-1 through
PoX-4, cost-voting, `at-block`, every epoch transition and the genesis balance
import. That is precisely the decade of legacy nano exists to not carry — it is
most of stacks-core's 724k lines. A partial reimplementation that disagreed with
the checkpoint would not tell us which side was wrong, and a complete one would
be stacks-core.

*The check we have is stronger.* "The reward set signed this root" is a statement
about what the network accepted. "Our own from-genesis replay agrees" is a
statement about our own code. The first is the thing an operator actually wants
to know.

*The cost is not just the sources.* The 11k lines of `.clar` are the cheap part;
the epoch initializers, the genesis account import and the pre-4.0 consensus
rules around them are not.

This decision is worth revisiting for a chain that begins at epoch 4.0 — a fresh
network has no legacy epochs to replay, so boot sources plus a genesis path
would cost roughly what the sources themselves cost. Hacknet is such a chain, but
nano already replays it from a checkpoint, so building it there would buy a
duplicate of a path we already have rather than a second opinion on mainnet.
