---
id: "131"
title: "Make executable block authentication a typed storage invariant"
status: in-progress
priority: critical
effort: medium
dependencies: ["079"]
tags: ["mainnet", "consensus", "staging", "signer", "safety"]
created_at: 2026-08-14
parent: 053
type: bug
touches: ["crates/nano-chainstate", "crates/nano-node", "crates/nano-conformance"]
---

# Make executable block authentication a typed storage invariant

## Objective

Make it impossible for an unsigned proposal or any other unauthenticated block
representation to enter executable staging. Close the storage-level hole behind
[[122-never-stage-an-unsigned-proposal-as-a-finalized-bl]] rather than relying on
every current and future caller to preserve the routing convention.

## Tasks

- [ ] Introduce distinct `ProposedBlock`, `AuthenticatedBlock`, `ExecutedBlock`
      and `CommittedBlock` types with private constructors at their validation
      transitions.
- [ ] Make the durable staging API accept only `AuthenticatedBlock`; proposal
      validation must use a physically separate ephemeral channel or store.
- [ ] Normalize staged block core bytes and signer certificates. A block ID may
      have multiple valid signer representations, but no unsigned representation
      may satisfy the executable-block query.
- [ ] Replace unconditional `INSERT OR REPLACE` with explicit identical-value,
      additional-certificate and representation-conflict handling.
- [ ] On startup, detect old unsigned or incoherent staged rows and fail closed
      or quarantine them before fork selection.
- [ ] Route P2P fetch, block upload, signer finalization, restart recovery and
      fork switching through the same authenticated constructor.
- [ ] Reproduce task 122's same-ID unsigned/finalized incident, including
      multiple independently valid signature subsets and restart at each step.

## Acceptance Criteria

- No public or internal API can persist an executable candidate without local
  burnchain, miner, VRF and signer-threshold authentication.
- A proposal and its later finalized block sharing one block ID always execute
  the finalized form; insertion order and restart cannot change that result.
- A second valid signer certificate cannot corrupt or hide the block core.
- Existing state either migrates deterministically or is refused with a precise
  recovery error; it is never silently rewritten.
- The complete signer, staging, fork, restart and conformance suites pass under
  strict Clippy.
