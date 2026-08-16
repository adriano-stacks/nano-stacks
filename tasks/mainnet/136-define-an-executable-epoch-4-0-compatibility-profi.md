---
id: "136"
title: "Define an executable Epoch 4.0 compatibility profile"
status: in-progress
priority: critical
effort: medium
dependencies: ["060", "064"]
tags: ["mainnet", "consensus", "epoch-4", "specification", "conformance"]
created_at: 2026-08-14
parent: 053
type: improvement
---

# Define an executable Epoch 4.0 compatibility profile

## Objective

Replace the implicit rule "whatever the pinned stacks-core currently does" with
a versioned, executable description of the exact Epoch 4.0 consensus surface
nano promises to preserve.

## Tasks

- [x] Define a machine-readable profile covering network and chain IDs,
      activation heights, PoX-5 transition, reward-cycle constants, system
      contract sources/hashes, transaction and block limits, Clarity semantic
      epoch and all consensus-critical compiler/host identities.
- [x] Bind every field to its SIP, deployed-chain evidence and pinned reference
      source revision. Record disagreements explicitly rather than choosing one
      source silently.
- [x] Convert the profile into implementation-neutral block, transaction,
      sortition, signer, VM, receipt, cost and refusal vectors.
- [ ] Run the vectors against nano and more than one compatible stock
      stacks-core revision so one reference implementation bug is not copied into
      the specification unnoticed.
- [x] Make checkpoint import, state opening and release qualification verify the
      profile fingerprint before execution.
- [x] Document the compatibility policy for security-only engine/compiler
      upgrades: full replay is mandatory and there is no fallback or healing
      mechanism.
- [ ] Fail closed on any unknown post-Epoch-4 activation instead of silently
      applying current rules beyond the declared profile.

## Acceptance Criteria

- One versioned profile completely names the supported consensus domain and is
  embedded in state, checkpoints and release artifacts.
- Independent runners produce the same expected vectors without importing nano's
  internal implementation.
- Every mandatory conformance test names the profile/vector evidence it protects;
  missing or skipped vectors fail qualification.
- A changed consensus parameter or compiler/engine identity cannot open existing
  state or qualify a release without an explicit migration and replay.
