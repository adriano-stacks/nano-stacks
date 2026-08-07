---
id: "079"
title: "Eliminate fallible MARF read panics after startup"
status: pending
priority: high
effort: medium
type: bug
group: mainnet
dependencies: ["057", "065"]
tags: ["mainnet", "storage", "recovery", "reliability"]
created_at: 2026-08-07
---

# Eliminate fallible MARF read panics after startup

## Objective

Propagate storage failures through the MARF read API. Verifying the tip root once
does not prove that every reachable trie child exists or that later I/O cannot
fail.

## Tasks

- [ ] Replace production `expect("trie storage")` read paths with typed results
      through `nano-marf`, `nano-vm`, chainstate and RPC callers.
- [ ] Either verify the complete reachable trie graph at startup or preserve
      typed errors for nodes not covered by the bounded startup check.
- [ ] Add a fixture whose tip record and root survive while a reachable non-root
      trie node is missing; opening or reading it must return an actionable error
      without a panic.
- [ ] Cover storage I/O failure after a successful open and prove no partial
      block or repair write is committed.
- [ ] Retain the clean restart, SIGKILL and commit-boundary recovery gates from
      tasks 057 and 065.

## Acceptance Criteria

- No production MARF read can panic because SQLite data is absent, corrupt or
  unavailable.
- Errors name the affected state directory, block and trie node without exposing
  keys or values.
- Refusal mutates no inspected database and leaves the last coherent tip usable.
- Existing crash recovery remains green.

## Evidence that opened this task

`VersionedMarf::verify_tip` checks the tip record, root and skip-list ancestors,
but deeper read APIs still use `expect("trie storage")`. A surviving root with a
missing reachable child can therefore pass startup and panic on a later lookup.
