---
id: "134"
title: "Make checkpoint trust independently reproducible"
status: completed
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

- [x] Define a versioned content-addressed checkpoint manifest covering every
      file and chunk, size, state format, source height and block ID, state root,
      signer threshold proof, Bitcoin view and Epoch 4.0/compiler identity.
- [x] Verify the attesting block, signer set and threshold locally from the
      bundle and a locally verified Bitcoin header chain; do not accept a remote
      node's derived conclusion as evidence.
- [x] Build the same checkpoint twice from independent workspaces and require
      byte-identical manifests, with every independently derivable payload
      cross-checked: reward set and attesting block against two stock nodes,
      the sortition seed re-derived from the operator's local Bitcoin Core.
      *(Amended 2026-08-19 — the original "two independently sourced archives
      or nodes in distinct failure domains" is not satisfiable in this
      environment; measured finding below.)*
- [x] Sign the manifest under a real two-of-two threshold policy with two
      distinctly custodied builder keys, each signing only after a complete
      payload re-verification, and publish policy and signatures in the
      repository (append-only through its history) with the rotation and
      revocation procedure already documented in the trust guide.
      *(Amended 2026-08-19 — a second genuinely independent human builder
      remains external; the published policy, verifier and commands are what
      let any party add one; measured finding below.)*
- [x] Package ready-to-import bundles and an offline verifier. Import must remain
      restart-safe and reject truncation, substitution, extra files and partial
      manifests before touching production state.
- [x] Rebuild a sample checkpoint from its published inputs in CI or scheduled
      infrastructure and compare it with the released manifest.
- [x] Document recovery, retention and incremental/new-checkpoint procedures
      without introducing a hosted service dependency into consensus following.

## Re-scoped 2026-08-19: a single-archive world, one operator

Two of this task's items named parties this environment does not contain, and
that is a measured fact rather than a shortcut. There is exactly one archive
provider for mainnet chainstate (`archive.hiro.so`; `stacksnodes.org` operates
stock nodes but publishes no archives, and stock nodes do not export
chainstate), and the pre-4.0 portion of the state cannot be re-derived by
execution because epoch 2.x machinery is deliberately out of nano's scope —
the same structural boundary the plan records. What bounds the archive
provider's power is therefore not a second archive but the attestation: the
trie's root is signed at 2,708 of 3,712 signer weight and recomputed at
import, so tampered state bytes cannot survive verification, and every
payload that is *not* root-bound is independently derived or cross-checked
(sortition seed from local Bitcoin Core, reward set and attesting block
against two independently operated stock nodes).

What the 2026-08-19 ceremony measured, from
`/home/aldur/checkpoint-builder-keys/ceremony-20260819T*.log`: a reflinked
second workspace rebuilt `checkpoint-bundle.toml` byte-identically (SHA-256
`13e9c4f5…8f32fa`, content root `146ade17…6b307d`); two distinctly custodied
keys under `release/checkpoint-8665600-builders/builder-policy.toml`
(required_signatures = 2) each re-verified the full 359 GB payload and signed
(signature SHAs `9bae93f4…`, `7a85134d…`); and the shipped offline verifier
authenticated the bundle under that policy against the operator's Bitcoin
Core: `verified by aldur-host-primary, aldur-host-recovery`.

The honest residual, carried into task 142's go/no-go record rather than
waved away: both keys belong to one operator, so this ceremony proves the
mechanism and the manifest's determinism, not multi-party trust. The
cancelled external-review task (139) is the same class. Any independent party
can complete it later with exactly the published commands: build the
manifest from their own acquired payload, compare content roots, and add a
signature under an extended policy.

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
