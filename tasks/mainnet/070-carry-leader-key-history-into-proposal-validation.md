---
id: "070"
title: "Carry leader-key history into proposal validation"
status: in-progress
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

- [x] Define the minimal authenticated leader-key registrations and sortition
      snapshots a checkpoint must carry, including keys registered before the
      retained burn window but referenced by later commitments.
      `leader-keys.json` (`block_height`, `vtxindex`, `public_key`, `memo`, in
      stacks-core's own `leader_keys` column names so an export is a copy of the
      rows rather than a translation of them) plus the seed snapshot's
      `winner_vrf_seed` and `last_sortition_height`, which are the two fields a
      resumed chain cannot derive and must not guess.
- [x] Export and import that history with provenance tied to the checkpoint
      attestation; do not obtain it ad hoc from the proposal's serving peer.
      `xtask export-leader-keys`, and `write_capture` writes it beside the
      snapshots and the consensus hashes because it answers the same kind of
      question they do.
- [x] Rebuild the leader-key tracker on startup and after restart or reorg.
      `SortitionTracker::resume_or_capture` takes the saved registry first and the
      capture's second, because the saved copy also holds the registrations this
      chain walked past above the checkpoint; `save` writes it out with the
      history, since a resumed chain reads the blocks *after* its tip and never
      those before it.
- [x] Wire the same local tracker into proposal validation that canonical block
      execution uses. `hosting::LocalBurnView`. See *What was actually broken*.
- [~] Pin a proposal whose miner key was registered below the ordinary retained
      burn window, plus wrong-key and wrong-parent-VRF controls. The wrong-key and
      wrong-parent-VRF halves are `conformance/tenure_vrf_enforcement.rs`, and that
      file no longer hardcodes the in-tree capture: it takes any capture through
      `nano_conformance::capture_root`, and it resolves a winning commitment's key
      through `sortition/leader-keys.json` as well as through the registrations
      inside the captured Bitcoin window. That second source is the whole point —
      a key is registered once and named for years, so on mainnet the registration
      is below any window a capture keeps, and searching the captured Bitcoin
      blocks alone could only ever find the exotic case.
      Not yet closed: on the live pox-5 capture (keys at burn 204, window 375–399)
      none of the three registered keys verifies the chosen tenure's coinbase proof
      against the captured sortition hash, so the gate still reports that it cannot
      run rather than passing. That is either the tenure the helper picks or that
      chain's data, and it wants its own measurement — the mainnet capture, whose
      keys *do* resolve, is the one to pin it with.
- [ ] Run a stock `stacks-signer` against nano and retain evidence that it
      accepts and signs a valid proposal after nano validates it locally.

## What was actually broken

The reported failure was `committed seed is not the hash of the parent tenure's
VRF proof`, and the missing `leader-keys.json` was only half of it. The other half
is that `ChainstateProposalValidator` refreshed exactly two fields of its burn
context per proposal — `height` and the accumulated coinbase — through
`context_at`. `sortition_hash`, `vrf_seed`, `winner_vrf_public_key` and
`winner_signing_key_hash` stayed at whatever the validator was *constructed* with,
which is the checkpoint anchor's burn block, for the life of the process.
`ActiveSortitionValidator::set_context` refreshed the outer sortition check and
never reached the inner one.

So every tenure-start proposal was checked against the anchor's committed seed and
rejected. The node was not failing to check the VRF — it was checking it against
the wrong burn block, and answering a stock signer that a valid block was invalid.

`hosting::LocalBurnView` derives the proposal's burn view the way the canonical
path does: `SortitionTracker::locate_view` walks this node's own Bitcoin blocks
until one derives the consensus hash the proposal names, and
`LocalSortition::from_snapshot` — now the one description of that mapping, shared
with the executor — fills the context. A view the burnchain has not reached is
`NoSuchTenure` with the reason named, rather than a validation run under a stale
anchor.

`a_proposal_is_validated_under_its_own_burn_block` pins it, and asserts first that
the two captured burn blocks disagree about the seed at all — a validator carrying
the wrong one would otherwise look correct.

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
