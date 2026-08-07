---
id: "031"
group: build
title: "Establish a trust root for the checkpoint"
status: completed
priority: high
effort: medium
type: improvement
dependencies: []
tags: ["mainnet", "marf", "checkpoint"]
created_at: 2026-07-30
completed_at: 2026-07-30
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

- [x] Decide and write down the trust model for a mainnet checkpoint.
- [x] Verify what is verifiable in-protocol: the root against the header at that
      height, and the header against the signer set that signed it.
- [x] Fail loudly when a checkpoint's declared root and its published root
      disagree.
- [x] Record the checkpoint's provenance in the node's own state.
- [x] Decide whether embedding boot sources for an independent genesis path is
      worth its cost, and record the answer either way.

## Acceptance Criteria

- A checkpoint whose root does not match the block header at its height is
  refused.
- The trust model is documented where an operator will read it.
- Provenance survives a restart.

## Outcome

`docs/checkpoint-trust.md` holds the trust model. `nano_node::attest_checkpoint`
checks a checkpoint's manifest against the signed Nakamoto header at its height
and the reward set that signed it; `adopt_checkpoint` records the result in the
state directory as `checkpoint-provenance.toml`, refusing a directory that
already descends from a different checkpoint. `import_checkpoint` now
cross-checks the caller's declared state and root against the `checkpoint.toml`
the checkpoint publishes, with distinct errors for each.

Boot contract sources stay out: reaching the 4.0 boundary from genesis means
executing epochs 2.0-3.4 bit-exactly, which is the legacy nano exists to drop,
and a signed header is a stronger statement than our own replay agreeing with
itself. The reasoning is in the document.
