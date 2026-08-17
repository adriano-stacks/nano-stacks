---
id: "134"
title: "Make checkpoint trust independently reproducible"
status: in-progress
priority: critical
effort: large
dependencies: ["050", "083"]
tags: ["mainnet", "checkpoint", "consensus", "security", "release"]
created_at: 2026-08-14
parent: 053
type: improvement
---

# Make checkpoint trust independently reproducible

## Objective

Make the Epoch 4.0 checkpoint an independently reproducible consensus trust root
rather than an operator-created directory assembled from one service's data and
claims.

## Tasks

- [ ] Define a versioned content-addressed checkpoint manifest covering every
      file and chunk, size, state format, source height and block ID, state root,
      signer threshold proof, Bitcoin view and Epoch 4.0/compiler identity.
- [ ] Verify the attesting block, signer set and threshold locally from the
      bundle and a locally verified Bitcoin header chain; do not accept a remote
      node's derived conclusion as evidence.
- [ ] Build the same checkpoint from at least two independently sourced archives
      or nodes operated in distinct failure domains and require identical
      manifests.
- [ ] Have independent builders sign the manifest and publish signatures in an
      append-only location with documented key rotation and revocation.
- [ ] Package ready-to-import bundles and an offline verifier. Import must remain
      restart-safe and reject truncation, substitution, extra files and partial
      manifests before touching production state.
- [ ] Rebuild a sample checkpoint from its published inputs in CI or scheduled
      infrastructure and compare it with the released manifest.
- [ ] Document recovery, retention and incremental/new-checkpoint procedures
      without introducing a hosted service dependency into consensus following.

## Acceptance Criteria

- Two independent builders derive the same state root and byte-level manifest
  from independently acquired inputs.
- A fresh operator can authenticate and import the bundle offline, then follow
  using only local Bitcoin and Stacks P2P.
- Every modification, omission, truncation, wrong signer set, wrong Bitcoin view
  and wrong compiler/epoch identity is refused before the checkpoint is usable.
- Published builder signatures, provenance and verifier commands are bound into
  the release evidence.
- Existing checkpoint continuation and interruption tests remain green.
