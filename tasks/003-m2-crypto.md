---
id: "003"
title: "M2: implement secp256k1 and VRF primitives"
status: completed
priority: high
effort: medium
dependencies: ["002"]
tags: ["m2", "crypto"]
created_at: 2026-07-27
completed_at: 2026-07-27
---

# M2: implement secp256k1 and VRF primitives

## Objective

Provide the consensus crypto primitives with byte-level compatibility against
the pinned Hacknet stacks-core reference implementation.

## Tasks

- [x] Use current stable secp256k1 for recoverable signing and recovery.
- [x] Enforce the reference high-S acceptance behavior for transaction and signer verification.
- [x] Implement the 80-byte ed25519 VRF proof format with the audited Dalek crates.
- [x] Add differential tests for signatures, high-S behavior, and VRF proof bytes.

## Acceptance Criteria

- `cargo test -p nano-crypto -p nano-conformance` passes.
- `cargo clippy -p nano-crypto -p nano-conformance --all-targets -- -D warnings` passes.
- Deterministic secp256k1 signatures and VRF proofs match stacks-core byte-for-byte.
