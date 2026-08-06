---
id: "070"
title: "Carry leader-key history into proposal validation"
status: pending
priority: high
effort: medium
dependencies: ["050", "051"]
tags: ["mainnet", "signer", "checkpoint", "conformance"]
created_at: 2026-08-06
type: feature
---

# Carry leader-key history into proposal validation

## Objective

Give the proposal validator the authenticated historical leader-key and
sortition context needed to validate a candidate from an imported checkpoint.
The hosted stock signer currently rejects every proposal through nano because
nano rejects it first: the validator cannot verify the committed VRF seed when
the checkpoint omits the old leader-key registration and the local tracker is
not wired into proposal execution.

## Tasks

- [ ] Define the minimal authenticated leader-key registrations and sortition
      snapshots a checkpoint must carry, including keys registered before the
      retained burn window but referenced by later commitments.
- [ ] Export and import that history with provenance tied to the checkpoint
      attestation; do not obtain it ad hoc from the proposal's serving peer.
- [ ] Rebuild the leader-key tracker on startup and after restart or reorg.
- [ ] Wire the same local tracker into proposal validation that canonical block
      execution uses.
- [ ] Pin a proposal whose miner key was registered below the ordinary retained
      burn window, plus wrong-key and wrong-parent-VRF controls.
- [ ] Run a stock `stacks-signer` against nano and retain evidence that it
      accepts and signs a valid proposal after nano validates it locally.

## Acceptance Criteria

- A valid candidate from checkpointed state passes the same leader-key and VRF
  checks as the corresponding canonical block.
- Missing, unauthenticated or inconsistent leader-key history causes a typed
  startup or proposal refusal rather than a guessed key or peer-supplied bypass.
- Wrong committed seeds and keys remain rejected in deterministic tests.
- A stock signer accepts and signs at least one block through nano without
  consulting a stock node for the missing consensus context.
- Restart and an ordinary burnchain reorganization rebuild the same validator
  view.

## Evidence that opened this task

The PoX-5 hosted-signer run proved registration and StackerDB writes but not
block acceptance. Nano logged `committed seed is not the hash of the parent
tenure's VRF proof`; the checkpoint exporter had no sortition history or
`leader-keys.json`. This is checkpoint and validator wiring, not an RPC-format
defect.
