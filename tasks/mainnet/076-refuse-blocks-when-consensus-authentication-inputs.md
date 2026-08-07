---
id: "076"
title: "Refuse blocks when consensus authentication inputs are unavailable"
status: pending
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
- [ ] Validate checkpoint completeness before synchronization starts: signer
      sets, executed tenure history, leader-key registry and parent proof must be
      coherent with the attested state.
- [ ] Remove production tests that expect an unknown leader or signing key to
      accept. Replace them with focused typed-refusal tests and valid controls.
- [ ] Keep execution-only fixtures explicitly labelled as unauthenticated; they
      may test VM replay but cannot satisfy a release block-authentication gate.
- [ ] Prove that a block with a self-consistent state root is rejected when any
      authentication input is missing or forged.

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
