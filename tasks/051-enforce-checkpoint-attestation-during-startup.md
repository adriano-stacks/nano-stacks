---
id: "051"
title: "Enforce checkpoint attestation during startup"
status: pending
priority: high
effort: medium
type: bug
group: mainnet
dependencies: ["031"]
tags: ["mainnet", "checkpoint", "trust"]
created_at: 2026-08-02
---

# Enforce checkpoint attestation during startup

## Objective

[[031-establish-a-trust-root-for-the-checkpoint]] implemented and documented
`attest_checkpoint` and `adopt_checkpoint`, but the binary bypasses both. Runtime
passes the configured source and root directly to `open_from_checkpoint`, and
the mainnet state has no `checkpoint-provenance.toml`.

Make the documented trust procedure the only production import path.

## Tasks

- [ ] Add configuration for the checkpoint manifest, attesting header and a
      reward set obtained independently of the checkpoint.
- [ ] Call `adopt_checkpoint` before any imported state is opened or copied.
- [ ] Refuse missing, unsigned, wrong-height, wrong-state or wrong-root inputs.
- [ ] Record provenance in the role's state directory and verify it on restart.
- [ ] Refuse to reuse a directory descended from a different checkpoint.
- [ ] Keep `docs/checkpoint-trust.md` executable as an operator procedure.

## Acceptance Criteria

- The shipped binary cannot import an unattested checkpoint.
- Provenance survives restart and names the manifest, signed header, signer
  weight and threshold used for adoption.
- Tests cover a valid import and every mismatch before any chainstate mutation.
