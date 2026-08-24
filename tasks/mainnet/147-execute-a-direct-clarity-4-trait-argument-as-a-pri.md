---
title: "Execute a direct Clarity 4 trait argument as a principal"
id: "147"
status: completed
priority: critical
type: bug
tags: ["mainnet", "vm", "clarity-wasm", "consensus", "liveness", "release"]
created_at: "2026-08-22"
completed_at: "2026-08-24"
---

# Execute a direct Clarity 4 trait argument as a principal

## Objective

Restore mainnet liveness at block 8,815,026. A Clarity 4 public function takes
a direct trait reference, passes it to `contract-of`, and the compiled engine
tries to read the argument from an unmaterialized `(offset, length) = (0, 0)`
principal representation. The canonical engine succeeds, so refusing the
block strands every nano follower at its parent.

## Evidence

The node on RPC port 20492, the current witness and the packaged follower all
seal through 8,815,025 and fail transaction
`8b54004787530e3547a2a9316838375eba701a72d55e7a3a72aef2fe3c471e1d`
in the next block with:

```text
principal representation for PrincipalType at value offset 159218 points to
offset 0 with invalid length 0
```

The canonical transaction calls
`SP2VMFSHP3EGCZNSQPAA31AJZKS7V70KXY0TT08RF.reward-claim-registry`
`register-for-claims`, whose second argument is a trait reference. The contract
was published at 8,814,886 under Clarity 4 and immediately binds
`(signer (contract-of signer-manager))`. The canonical receipt is `(ok u2)`
with state root `255c6115d5038ee255afefb26ddaed565417d06e5b17f88a4cd9667e569c476b`
for block 8,815,026.

## Tasks

- [x] Reduce the direct Clarity 4 public trait-argument path to an offline
      compiled/interpreter crosscheck.
      `trait_equality::a_clarity_four_trait_principal_survives_the_registration_shape`
      runs the reduced registration shape through both engines, asserts they agree
      and that the answer is `UInt(2)` — the canonical `(ok u2)`. Unconditional, no
      mainnet state. Its padding sweep runs 0, 1, 64 and 4,096 bytes of preceding
      static data, because the mainnet failure named a *value offset* and where a
      value lands depends on what precedes it; green at all four.
- [x] Fix the nano-owned clarity-wasm ABI path without an interpreter fallback.
      Closed in 26067420: duck-typing an optional projects its payload's slots
      on both branches, so the `none` branch offered a tuple whose principals
      were the unmaterialised `(0, 0)`, and nano tried to materialise it into
      the runtime-shape arena. A shape that cannot be read cannot be preserved,
      so it answers with the same zero handle a value that never crossed the
      host carries. The smallest reproduction needs no trait at all.
- [x] Replay transaction 8b540047…c471e1d and block 8,815,026 to their
      canonical receipt and state root. Executed on mainnet rather than in a
      fixture: the rebuilt node executed the round 8,813,989 → 8,815,989, which
      spans the block, with no refusal of that transaction anywhere in its log.
- [x] Build and restart the node on port 20492, then prove its executed height
      advances beyond 8,815,026. It reached **8,815,989**, 963 blocks past the
      height it had been stuck on for days.

## Closed 2026-08-24, against the canonical chain

**A correction to the evidence above first.** This task recorded
`255c6115d5038ee255afefb26ddaed565417d06e5b17f88a4cd9667e569c476b` as the
canonical *state root* for 8,815,026. It is the canonical **block hash**. The
state root is
`45fff53e2d156f002663ebcc4eef4b12fcb1a079784c384f2545d201c2a16c39`. Anyone
comparing roots against the old value would have chased a mismatch that was not
there.

**The node's copy of the block is the canonical one, field for field.** Fetched
from a stock oracle at `/v3/blocks/height/8815026` and from this node at
`/v3/blocks/{id}`, both 56,259 bytes, and `block-identity` reports the same
`block_hash`, `block_id b7781596…`, `consensus_hash d7cf0ff9…`,
`state_index_root 45fff53e…`, `transaction_merkle_root 5aa0554c…`, 15
transactions and 13 signer signatures. `append_block` refuses a block whose
sealed MARF root differs from the header's, so sealing it *is* the root
comparison — this node computed `45fff53e…`.

**The receipt is the canonical one.** From the node's own event observer,
`new_block/08815026-255c6115….json`:

```text
txid    0x8b54004787530e3547a2a9316838375eba701a72d55e7a3a72aef2fe3c471e1d
status  success
result  0x070100000000000000000000000000000002        (ok u2)
cost    runtime 379891, read_count 24, read_length 345596, write_count 4, write_length 519
```

**The offline gate is unconditional.**
`trait_equality::a_clarity_four_trait_principal_survives_the_registration_shape`
crosschecks compiled against interpreted on the reduced shape and asserts
`UInt(2)`, at four different amounts of preceding static data, with no mainnet
state. `cargo clippy --workspace --all-targets -D warnings` is clean.

**What remains is not this task.** The exact-prestate replay
(`the_mainnet_8815026_trait_registration_executes_at_its_exact_prestate`) stays
ignored and inventoried as infrastructure under this owner: it is a fixture
convenience, and the live chain has now answered the same question against the
canonical block.

## Acceptance Criteria

- The focused compiled/interpreter regression is green for Clarity 4.
- Block 8,815,026 seals with the canonical receipt and state root.
- The restarted node advances beyond the stalled height.
- `cargo clippy` reports no warnings in every changed crate.
