---
title: "Reject inconsistent state directories without panicking"
id: "065"
status: pending
priority: high
effort: small
type: bug
group: mainnet
dependencies: ["057"]
tags: ["storage", "recovery", "operations"]
created_at: "2026-08-06"
---

# Reject inconsistent state directories without panicking

## Objective

Opening a reflink copy taken while the node was writing exposed an inconsistent
set of MARF and side-store files. The inconsistency was correctly detected, but
the read path panicked at `crates/nano-marf/src/lib.rs` with:

```
trie storage: Storage("trie storage is missing block 8679483")
```

A hot filesystem copy is not a supported atomic snapshot and does not indicate
that [[057-commit-and-recover-accepted-block-state-atomically]] failed: repeated
hard kills of the real node directory recover coherently. Still, corrupted,
partial or externally copied state is an operator error the binary should name
and refuse cleanly rather than turning a storage inconsistency into a Rust
panic.

## Tasks

- [ ] Reproduce the missing-MARF-block case with a deterministic fixture rather
      than depending on a race while copying a live directory.
- [ ] Propagate a typed storage/startup error through the node boundary instead
      of `expect`/panic when a sealed block's trie data is unavailable.
- [ ] Include the missing block identifier and affected database/path in the
      diagnostic without dumping keys, values or unrelated state.
- [ ] Prove the node opens a clean shutdown directory and every crash-injection
      directory from [[057-commit-and-recover-accepted-block-state-atomically]]
      exactly as before.
- [ ] Document that copying a live working directory is not an atomic backup;
      name the supported stop/snapshot procedure.

## Acceptance Criteria

- An inconsistent state directory exits startup with a bounded, actionable
  error and no panic/backtrace requirement.
- Refusal mutates none of the database files it inspected.
- Clean restart, SIGKILL recovery and commit-boundary recovery tests remain
  green.
- The task does not claim that a file-by-file copy of a running node is a valid
  snapshot format.
