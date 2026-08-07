---
id: "079"
title: "Eliminate fallible MARF read panics after startup"
status: in-progress
priority: critical
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

- [~] Replace production `expect("trie storage")` read paths with typed results
      through `nano-marf`, `nano-vm`, chainstate and RPC callers.
- [ ] Preserve `MarfError` and side-store I/O errors through `MarfStore::get`,
      `value_of` and every `ClarityBackingStore` caller. Never convert a failed read
      to `None`, because corruption and a key that never existed are different
      consensus-visible inputs.
- [ ] Either verify the complete reachable trie graph at startup or preserve
      typed errors for nodes not covered by the bounded startup check.
- [x] Add a fixture whose tip record and root survive while a reachable non-root
      trie node is missing; opening or reading it must return an actionable error
      without a panic.
- [ ] Cover storage I/O failure after a successful open and prove no partial
      block or repair write is committed.
- [ ] Retain the clean restart, SIGKILL and commit-boundary recovery gates from
      tasks 057 and 065.

## Where this stands, 2026-08-07

**The MARF layer's read paths no longer panic, but the VM boundary is still
fail-open.** `MarfTrie` and `VersionedMarf::get`/`get_path` return `Result`; the two
callers that matter -- `MarfStore::get` and `value_of` in `nano-vm` -- immediately
apply `.ok().flatten()` and return absence. That keeps the process alive but loses
the typed refusal this task requires. A missing value can change Clarity control
flow, receipts or RPC answers before a later state-root comparison has any chance
to catch a write, so "read as absent" is not completion of error propagation.

**The fixture is real corruption, built the only honest way.** There is no API that
produces a trie with a hole, so the test deletes a `marf_node` row directly, reopens,
and reads every key. It asserts two things and neither is the interesting one alone:
that reads *refuse* -- otherwise the fixture is not the corruption it claims to be --
and that the process is alive to be asked afterwards. Which keys descend through the
removed node is a detail of the trie's shape and is deliberately not asserted.

It also found the gap it was written for: with `MarfTrie` converted the test still
panicked, on `VersionedMarf::get_path`, which is exactly the "deeper read API" this
task names.

**Eleven `expect("trie storage")` remain**, in paths a running node does not take on a
block: `tip`, `root`, `leaves_at`, `pointers_at`, the jump list and the block record.
They are startup, diagnostic and export paths. Converting them is the rest of the
first bullet and ripples further -- `tip` alone has callers everywhere -- so it is
left visible rather than half-done.

## Acceptance Criteria

- No production MARF read can panic because SQLite data is absent, corrupt or
  unavailable.
- No production MARF or side-store error is observable as an absent Clarity key,
  metadata entry or RPC value.
- Errors name the affected state directory, block and trie node without exposing
  keys or values.
- Refusal mutates no inspected database and leaves the last coherent tip usable.
- Existing crash recovery remains green.

## Evidence that opened this task

`VersionedMarf::verify_tip` checks the tip record, root and skip-list ancestors,
but deeper read APIs still use `expect("trie storage")`. A surviving root with a
missing reachable child can therefore pass startup and panic on a later lookup.
