---
id: "133"
title: "Produce clean reproducible signed release artifacts"
status: pending
priority: critical
effort: medium
dependencies: ["074", "078"]
tags: ["mainnet", "release", "reproducibility", "supply-chain", "ci"]
created_at: 2026-08-14
parent: 053
type: chore
---

# Produce clean reproducible signed release artifacts

## Objective

Turn a release from a locally built binary plus a readable report into a clean,
reproducible and signed artifact whose source, dependency closure, compiler,
checkpoint and qualification evidence are inseparable.

## Tasks

- [ ] Make `cargo xtask release-report` fail qualification on tracked, staged,
      untracked or ignored build-relevant changes instead of only printing that
      the tree is dirty.
- [ ] Build the release as a Nix derivation from a clean source closure, not from
      a mutable working tree or pre-existing `target/` directory.
- [ ] Build twice in independent clean stores and compare the binaries and all
      packaged data byte-for-byte. Explain and eliminate any nondeterminism.
- [ ] Generate an SBOM and record the exact Cargo feature/dependency closure,
      Rust toolchain, Wasmtime configuration, clarity-wasm identity and target.
- [ ] Produce signed checksums and provenance for the binary, configuration
      schema, checkpoint manifest and qualification report.
- [ ] Add a release-candidate freeze rule: any source, lock, toolchain,
      configuration or packaged-data change invalidates the qualification run.
- [ ] Package documented systemd/container profiles with explicit memory, file
      descriptor, disk, log and shutdown behavior.

## Acceptance Criteria

- A dirty tree, stale artifact, missing SBOM, advisory-policy failure or unsigned
  manifest makes the release report non-qualifying.
- Two independent clean builds are byte-identical and their published checksum
  matches the artifact exercised by qualification.
- An operator can verify source revision, compiler/engine identity, checkpoint
  identity and every qualification input without trusting the build machine.
- The packaged service shuts down and restarts without exposing partial state.
- CI exercises a deliberately dirty tree and a deliberately stale artifact and
  proves both are rejected.
