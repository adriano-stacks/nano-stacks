---
id: "070"
title: "Carry leader-key history into proposal validation"
status: in-progress
priority: critical
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
- [x] Pin a proposal whose miner key was registered below the ordinary retained
      burn window, plus wrong-key and wrong-parent-VRF controls. All eight
      `tenure_vrf_enforcement` gates now **run and pass under
      `NANO_REQUIRE_MAINNET`** — no skips — against both the in-tree capture and a
      live pox-5 capture whose three leader keys are registered at burn 204, **171
      burn blocks below its own window of 375–399**. That is the below-the-window
      case in the field rather than contrived.

      Two defects were in the way, and the second hid the first:

      * `captured_bitcoin_snapshots` never read `sortition_hash`, so every captured
        burn context carried **zeros** for it. A coinbase VRF proof is *over* the
        sortition hash, so it could not verify against any key on any capture — and
        the gates blamed a missing leader key. Read from the archive now, like
        `pox_payouts` and for the same reason: it is the oracle.
      * `winning_key` and `winning_registration` searched only the registrations
        inside the captured Bitcoin window. A key registered once and named for
        years is normally outside it, so the checkpoint's `leader-keys.json` is read
        beside the Bitcoin blocks, and a registration is rebuilt from a registry row
        when the window has none — the two fields these gates check, the VRF key and
        the block-signing hash, are exactly what a row states.

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
