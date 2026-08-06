---
title: "Reject inconsistent state directories without panicking"
id: "065"
status: completed
priority: high
effort: small
type: bug
group: mainnet
dependencies: ["057"]
tags: ["storage", "recovery", "operations"]
created_at: "2026-08-06"
completed_at: 2026-08-06
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

- [x] Reproduce the missing-MARF-block case with a deterministic fixture rather
      than depending on a race while copying a live directory.
- [x] Propagate a typed storage/startup error through the node boundary instead
      of `expect`/panic when a sealed block's trie data is unavailable.
- [x] Include the missing block identifier and affected database/path in the
      diagnostic without dumping keys, values or unrelated state.
- [x] Prove the node opens a clean shutdown directory and every crash-injection
      directory from [[057-commit-and-recover-accepted-block-state-atomically]]
      exactly as before.
- [x] Document that copying a live working directory is not an atomic backup;
      name the supported stop/snapshot procedure.

## Acceptance Criteria

- An inconsistent state directory exits startup with a bounded, actionable
  error and no panic/backtrace requirement.
- Refusal mutates none of the database files it inspected.
- Clean restart, SIGKILL recovery and commit-boundary recovery tests remain
  green.
- The task does not claim that a file-by-file copy of a running node is a valid
  snapshot format.

## Asked once, at open, of the rows a torn copy actually loses

`VersionedMarf::verify_tip` reads the tip's block record, its root node, and every
ancestor its Merkle skip-list reaches, and `MarfStore::open` turns a failure into
`MarfStoreError::IncoherentState`. That is the whole check: a trie is immutable per
`(block, index)`, so a store that was whole when it opened stays whole, and every
read after this may keep treating storage failure as impossible. What is not
impossible is opening a store that was *never* whole.

The message is the deliverable as much as the refusal is:

```
this state directory is not whole: /…/chainstate/marf.sqlite: MARF storage error:
trie storage is missing trie node 461/20. Nothing was written. A node's working
directory is not a backup format while it runs -- stop the node, then copy.
```

It names the file, the missing node, that nothing was written, and the supported
procedure. It dumps no keys and no values.

Three tests, and the second and third are the ones that could have been skipped:

- the tip's trie rows deleted outright — the same absence a mid-write copy leaves,
  with none of the timing — is refused, and the assertions are on the *message*
  rather than only on the failure;
- refusing **changes no file**: every file's length and modification time is
  fingerprinted either side of the attempt. A startup check that repaired,
  truncated or vacuumed would be a second way to lose state, and an operator's
  next move after this error is usually to copy the directory and look at it;
- a clean directory still opens to the tip it sealed. The check runs on every
  start, so one that was too strict would refuse the state every node has —
  `kill_during_replay`, `kill_during_import` and `binary_restart` are the rest of
  that argument and run unchanged.

**What this does not claim.** A file-by-file copy of a running node is not a
snapshot format and this does not make it one. It makes the failure legible.
