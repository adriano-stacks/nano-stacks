---
id: "137"
title: "Add continuous adversarial and cross-architecture verification"
status: in-progress
priority: critical
effort: large
dependencies: ["130", "131", "132", "136"]
tags: ["mainnet", "testing", "fuzzing", "chaos", "conformance"]
created_at: 2026-08-14
parent: 053
type: chore
---

# Add continuous adversarial and cross-architecture verification

## Objective

Continuously search for parser, state-machine, recovery, resource and
architecture-specific failures outside the finite mainnet fixtures, and retain
every finding as a deterministic regression.

## Tasks

- [x] Add structure-aware fuzz targets for P2P framing/session state,
      transaction and block codecs, signer/StackerDB messages, checkpoint
      manifests/import, MARF operations and clarity-wasm ABI boundaries.
- [x] Differentially fuzz consensus-visible outputs against compatible
      stacks-core implementations and persist minimized corpora in CI.
- [ ] Run a bounded deterministic fuzz corpus on every change and continuous
      long-running fuzzers with crash artifact retention and ownership.
- [x] Add Miri where applicable plus address, undefined-behavior and thread
      sanitizer jobs for the unsafe cache boundary, FFI/dependencies and
      concurrent adapters.
- [ ] Inject `ENOSPC`, `EIO`, short reads/writes, read-only filesystems, corrupt
      pages, torn/truncated files and failure at every fsync/rename/commit point,
      in addition to process-kill timing tests.
- [ ] Add deterministic network-chaos scenarios for partitions, peer churn,
      equivocation, slow/fragmented peers, delayed Bitcoin views, reorgs and
      reward-boundary restarts.
- [ ] Run the executable Epoch 4.0 corpus on x86-64 and AArch64 and compare roots,
      receipts, costs, events and refusal classes byte-for-byte.
- [x] Use mutation testing on authentication, fork-choice and commit decisions to
      prove the mandatory suite kills deliberately weakened checks.

## Acceptance Criteria

- Every target has an owned persistent corpus, a CI smoke budget and a continuous
  job whose findings cannot disappear as expired CI artifacts.
- Every discovered crash, hang, divergence or unbounded trend has a minimized
  checked-in regression before closure.
- Cross-architecture results are identical for all consensus-visible outputs.
- Fault injection exposes neither partial committed state nor a false
  consensus-invalid verdict.
- The release inventory reports the status and last successful run of every
  mandatory adversarial job.
