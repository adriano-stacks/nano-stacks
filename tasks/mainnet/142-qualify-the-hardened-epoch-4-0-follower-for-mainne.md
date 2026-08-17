---
id: "142"
title: "Qualify the hardened Epoch 4.0 follower for mainnet"
status: pending
priority: critical
effort: small
dependencies: ["138"]
tags: ["mainnet", "release", "consensus", "qualification"]
created_at: 2026-08-14
parent: 053
type: chore
---

# Qualify the hardened Epoch 4.0 follower for mainnet

## Objective

Act as the fail-closed roll-up for the new hardening program. Apply the
mainnet-ready label only to the minimal follower artifact whose complete
dependency graph, independently reproduced checkpoint and full-cycle evidence
are finished.

## Tasks

- [ ] Confirm taskmd reports every dependency complete and no critical/high
      release task, blocking semantic ignore, declared differential, advisory
      exception past expiry or unowned qualification input remains.
- [ ] Run the qualifying release report against the signed artifact, checkpoint,
      mainnet capture and state from [[138-run-a-multi-operator-full-reward-cycle-qualificati]].
- [ ] Verify that the report binds the clean source, reproducible artifact,
      Epoch 4.0 profile, engine/SBOM, checkpoint builders and raw operator
      evidence.
- [ ] Reconcile [[053-pass-the-mainnet-node-release-gate]] line by line; a stale
      checked box is not evidence and an unexecuted gate is not a pass.
- [ ] Publish a signed go/no-go record naming residual operational assumptions,
      supported platforms, resource floors, rollback procedure and incident
      contacts.
- [ ] Tag and publish exactly the qualified artifact without rebuilding or
      changing any input.

## Acceptance Criteria

- Every formal dependency and every task 053 acceptance criterion is complete
  with artifact-bound evidence; no waiver converts missing evidence into a pass.
- `cargo xtask release-report` succeeds from the published clean source and
  rejects any substituted artifact, checkpoint, profile or evidence bundle.
- The published checksum is identical to the artifact run by all qualification
  operators.
- The go/no-go record is signed and independently reproducible.
- Only after these conditions hold may tasks 142 and 053 be completed.
