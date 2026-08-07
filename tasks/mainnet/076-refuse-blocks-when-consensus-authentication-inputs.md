---
id: "076"
title: "Refuse blocks when consensus authentication inputs are unavailable"
status: in-progress
priority: critical
effort: large
type: bug
group: mainnet
dependencies: ["050", "070"]
tags: ["mainnet", "consensus", "authentication", "checkpoint"]
created_at: 2026-08-07
---

# Refuse blocks when consensus authentication inputs are unavailable

## Objective

Make block authentication fail closed. Missing signer, miner, tenure or VRF
evidence is an incomplete checkpoint or an unverifiable block, not a successful
check with a warning.

## Tasks

- [ ] Replace the missing-recorded-signer-set `Ok(())` path with typed startup or
      consensus refusal, according to when the missing state is discovered.
- [ ] Refuse a tenure change whose claimed parent tenure or length cannot be
      checked against the imported executed ledger.
- [ ] Refuse a tenure-start block when its winner VRF key, registered miner
      signing key, coinbase proof or parent-tenure proof is unavailable.
- [~] Validate checkpoint completeness before synchronization starts: signer
      sets, executed tenure history, leader-key registry and parent proof must be
      coherent with the attested state.
- [ ] Remove production tests that expect an unknown leader or signing key to
      accept. Replace them with focused typed-refusal tests and valid controls.
- [ ] Keep execution-only fixtures explicitly labelled as unauthenticated; they
      may test VM replay but cannot satisfy a release block-authentication gate.
- [ ] Prove that a block with a self-consistent state root is rejected when any
      authentication input is missing or forged.

## The constraint the first bullet runs into

`check_signer_signatures` does not report-and-accept out of laziness, and a naive
fail-closed change stops the live mainnet follower on its first block.

Mainnet's **cycle 140 was stacked in pox-4**, before the state nano imports, so the
block that wrote its `.signers` entries is *below the checkpoint*. A node replaying
from that checkpoint has nothing to check the set against, and refusing at the block
would refuse every block of the chain the network is actually on.

So the bullet's own wording is the design — "according to when the missing state is
discovered". The refusal belongs at **startup**, where the node can ask a different
question: for the cycles this checkpoint will execute, is a signer set available at
all? A checkpoint that cannot answer is incomplete and must not begin syncing. Once
that holds, a missing set *at execution time* is a genuine failure and can be typed
as one, because startup has already established it should have been there.

Doing it the other way round -- turning the execution-time `Ok(())` into a refusal
first -- produces a node that refuses mainnet, and the checkpoint that would make it
correct is a separate piece of work. Order matters here, which is why it is written
down.

## First piece, 2026-08-07: the leader-key registry

`SortitionTracker::resume_or_capture` warned about a checkpoint carrying zero leader
keys and carried on. That is right for a *harness* walking a captured window, where
the registrations it needs are inside the window and the tracker picks them up as it
walks. It is wrong for a node: a leader key is registered once and named for years,
tens of thousands of burn blocks below any window a checkpointed node holds, so
without the registry `check_tenure_vrf` and `check_miner_won_the_sortition` cannot run
at all — and a rule that never runs looks exactly like one that always passes.

So the refusal is in the node's startup path and not in the tracker, which keeps both
callers honest. Both live configurations carry one already (mainnet 2,477
registrations, the hacknet rig 3), so this refuses nothing that works today.

Verified by construction rather than by a negative run: the count is the same
`leader_keys()` the live nodes print on every start. Reaching the refusal for real
needs a checkpoint import of a 30 GB MARF, which is not a test.

## Acceptance Criteria

- Every accepted followed block has verified signer weight, miner signature,
  winning sortition, coinbase VRF proof, committed parent seed and tenure
  continuity.
- A production node cannot enter sync with an incomplete checkpoint and cannot
  turn unavailable authentication data into acceptance.
- Release output contains zero `cannot check` authentication lines; unavailable
  inputs are named typed failures.
- The complete attested replay passes with all authentication checks exercised.

## Evidence that opened this task

`check_signer_signatures`, `check_tenure_continuity`,
`check_miner_won_the_sortition` and `check_tenure_vrf` currently report missing
inputs and return success. `SortitionTracker::resume_or_capture` likewise accepts
a checkpoint with zero leader keys. The conformance suite asserts that unknown
leader and signing keys accept a block.
