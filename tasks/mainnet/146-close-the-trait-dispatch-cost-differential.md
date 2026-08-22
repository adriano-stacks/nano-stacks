---
id: "146"
title: "Close the trait-dispatch cost differential against the canonical record"
status: in-progress
priority: critical
effort: large
dependencies: ["097", "098"]
tags: ["mainnet", "vm", "costs", "conformance", "release"]
created_at: 2026-08-21
type: bug
---

# Close the trait-dispatch cost differential against the canonical record

## Objective

nano charges more than the canonical stacks-core record for at least one
mainnet trait-dispatch contract call, on three of the five cost dimensions,
while sealing the identical state root. Costs are consensus-visible near the
block limits, so per the plan's own rule — every cost differential blocks
release, whether or not a current block hits it — this is open until the
accounting matches or the difference is proven to be the oracle's.

## The measurement that opened this

2026-08-21, from the completed 24-hour hold's deferred receipt verification.
Block 8,808,752 (lone transaction `0xfc71c88f…d604`, a successful
`contract-call?` to `SPNWZ5V2TPWGQGVDR6T7B6RQ4XMGZ4PXTEE0VQ0S.change-price-v1
::change-price-a`):

| dimension | nano (16e0928a follower AND witness, and the older dc447744 node) | canonical (Hiro record of stacks-core) | difference |
|---|---|---|---|
| read_count | 30 | 30 | 0 |
| read_length | 77,622 | 76,653 | **+969** |
| runtime | 165,496 | 161,448 | **+4,048** |
| write_count | 4 | 4 | 0 |
| write_length | 901 | 341 | **+560** |

Three independent facts bound the defect:

- **The root matched.** The hold verified this block's consensus identity,
  including the sealed state root, byte-for-byte against two independent
  stock nodes. The durable writes are identical; only nano's *accounting* of
  lengths and runtime differs.
- **Both nano lineages agree with each other.** The frozen 16e0928a follower,
  the same-revision witness and the older pre-map-write-cost-fix node all
  report the identical inflated figures, so this is not the recent compiler
  change — it is long-standing.
- **It is path-specific.** The same field-by-field verifier passed complete
  blocks elsewhere in the window (e.g. 8,800,750), and hacknet's stock-node
  receipt comparisons have always matched on all five dimensions — hacknet
  traffic never exercised this shape.

The contract is trait-heavy: `change-price-a` dynamically dispatches
`unlist-asset` and `list-asset` through trait references into two other
marketplace contracts, which is the cost surface tasks 097 and 098 worked —
this looks like a residual on the *charging* side of dynamic dispatch
(callee load sizes and write-length accounting), not the semantic side.

## Why self-checks never saw it

The mainnet receipt gates replay against payloads nano's own observer
recorded, which proves determinism, not canonical agreement. The hacknet
gates do compare against stock observers, but never ran this call shape. The
hold's deferred verifier is the first mainnet-scale field-by-field
cost comparison against the canonical record — and it caught this on its
second block.

## Tasks

- [ ] Complete the block-level cost sweep over the hold window (in flight at
      creation time: `/home/aldur/mainnet-hold/cost-mismatches.jsonl`) and
      classify every mismatching block by contract and call shape.
- [x] Reproduce the differential offline through the dual-engine tooling on a
      state snapshot, dimension by dimension, and localize the charging site
      (dynamic callee load, trait argument casting, write-length measurement).
- [x] Fix the length dimensions: charge data words at the executing epoch and
      size trait references as values, closing every `read_length` and
      `write_length` difference in the window.
- [x] Build a canonical-oracle measurement that does not need a re-attested
      checkpoint: `xtask replay-window` replays consecutive blocks at their
      exact prestates with the real transaction prefix and verified roots, and
      a conformance gate pins block 8,808,752 against the chain's own record.
      First window: 39 blocks, 66 transactions, 64 matching on all five
      dimensions.
- [x] Close the largest chain-level differential: `concat` was charged for the
      bytes its copies moved rather than the items it joined, sixteen times too
      much for a list of `uint`. Found through a single-transaction block where
      the canonical comparison is exact; closed 29 of the 55 differing
      transactions, taking the ten-window audit from 793/848 to **822/848**.
- [ ] Close the remaining 26 (3.1%), all runtime-only: `+8` on fourteen, then
      `-184` on three, `+1002` on two and eight singletons. A label-level diff
      cannot localize these — the engines decompose the same work into
      different charge events whose costs nearly cancel — so this needs a
      comparison that aligns charges to source positions rather than to labels.
- [ ] Close the earlier per-call findings, now known not to be chain
      divergences on their own: `dlmm-liquidity-router-v-1-2`'s
      `withdraw-liquidity-multi` over-charges runtime by 2 units per folded
      position (1,228 and 98 on the two mainnet calls). The interpreter agrees
      with the chain here, so it is a usable oracle; a one-element call
      reproduces it at the same prestate, and reduced probes of the shape do
      not yet.
- [ ] Re-run the checkpoint builder ceremony for the new compiler identity.
      A node refuses a bundle whose recorded compiler identity is not its own
      and the section is mandatory, so the cost fixes invalidate the attested
      bundle (`sha256:1f78d344`, profile `5561d364`) and no fixed-compiler
      replay can start until an attested bundle exists for the new identity.
      Hand-editing a copied bundle would forge the builders' binding.
- [ ] Re-execute the hold window's blocks with the fixed compiler and compare
      every receipt to the canonical record. This is the measurement that
      decides the task: the compiler-versus-interpreter census cannot, because
      on at least one mainnet shape (`swag`/`psis`) nano's sealed runtime is
      362 *below* canonical while the compiler is 62 *above* the interpreter,
      putting the interpreter further from the chain than the engine under
      test. `cost-both-tx` is not a substitute — it runs one call without the
      block's transaction prefix.
- [ ] Close the runtime-only residual. Confirmed so far, each with a probe in
      `clar2wasm`'s `borrowed_operand_charges`: the block-info words,
      `to-ascii?` and `secp256k1-verify` charged a copy for an operand the
      reference borrows. Still open: `from-consensus-buff?` charges 23 units
      too many on a buff parameter, and skipping the operand copy overshoots
      to 4 under, so the cause is the word's charge input rather than the read
      (recorded as an ignored probe naming this task); the `+62`, `+8` and
      `+1281` families are unattributed; and `BNS-V2::name-claim-fast`
      performs one extra charged read and write, which is operational rather
      than pricing.
- [ ] Add the reproduced call shape to the conformance corpus so the gate
      that catches it cannot skip itself, and re-run the mainnet cost sweep
      to zero mismatches.
- [ ] Re-run a hold window's deferred receipt verification end to end green,
      which is what unblocks task 106.

## Acceptance Criteria

- Every cost dimension of every transaction in a verified mainnet window
  matches the canonical record exactly. The canonical record is the oracle
  here, not the interpreter: where the two disagree, the chain decides.
- The differential's call shape is a permanent regression test.
- No production path consults the interpreter to compute or repair costs.
