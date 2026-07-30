---
id: "031"
title: "Establish a trust root for the checkpoint"
status: pending
priority: high
effort: medium
type: improvement
dependencies: []
tags: ["mainnet", "marf", "checkpoint"]
created_at: 2026-07-30
---

# Establish a trust root for the checkpoint

## Objective

nano can only start from an imported stacks-core MARF, and `import_checkpoint`
verifies its root against a value the caller supplies
(`crates/nano-marf/src/checkpoint.rs:100`). That is self-consistency, not proof:
a wrong checkpoint with a matching declared root imports cleanly and every block
after it is wrong in a way nano cannot see.

There is no second path. W7's boot contract sources were never embedded — the
tree holds no `.clar` files — so nano cannot build the state itself and compare.

Running on mainnet means deciding what an operator is actually trusting, and
making nano check whatever part of it can be checked.

## Tasks

- [ ] Decide and write down the trust model for a mainnet checkpoint.
- [ ] Verify what is verifiable in-protocol: the root against the header at that
      height, and the header against the signer set that signed it.
- [ ] Fail loudly when a checkpoint's declared root and its published root
      disagree.
- [ ] Record the checkpoint's provenance in the node's own state.
- [ ] Decide whether embedding boot sources for an independent genesis path is
      worth its cost, and record the answer either way.

## Acceptance Criteria

- A checkpoint whose root does not match the block header at its height is
  refused.
- The trust model is documented where an operator will read it.
- Provenance survives a restart.
