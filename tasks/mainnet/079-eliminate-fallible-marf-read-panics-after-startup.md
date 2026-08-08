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

- [x] Replace production `expect("trie storage")` read paths with typed results
      through `nano-marf`, `nano-vm`, chainstate and RPC callers.
- [x] Preserve `MarfError` and side-store I/O errors through `MarfStore::get`,
      `value_of` and every `ClarityBackingStore` caller. Never convert a failed read
      to `None`, because corruption and a key that never existed are different
      consensus-visible inputs.
- [x] Either verify the complete reachable trie graph at startup or preserve
      typed errors for nodes not covered by the bounded startup check.
- [x] Add a fixture whose tip record and root survive while a reachable non-root
      trie node is missing; opening or reading it must return an actionable error
      without a panic.
- [x] Cover storage I/O failure after a successful open and prove no partial
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

## The fail-open is closed, 2026-08-07

Three read paths turned storage failure into absence, and one of them is the path
Clarity takes:

- `VersionedMarf::get_active_path` panicked outright on reads of the block being
  executed — the single hottest read there is.
- `MarfStore::value_of` printed the error and returned `None`. This is what
  `ClarityBackingStore::get_data` calls, so a trie that could not be read looked
  to Clarity exactly like a key the chain never wrote.
- `MarfStore::get` did the same for sealed reads, and `xtask state-value` with it.

All three carry the error now. `get_data_from_path` already returned `Result` and
simply propagates. `xtask state-value` has three answers rather than two.

Pinned by `a_read_that_cannot_be_answered_is_an_error_and_not_an_absence`, which
deletes a sealed block's `marf_node` rows, leaves the tip's record and root intact
so the store still opens, and reads through the hole.

## What remains, and one of it is a boundary rather than work

**Ten `expect("trie storage")` remain** in `nano-marf`: `tip`, `contains`,
`leaves`, `pointers_at`, `parent`, `block_at_height` and the `record` they share.
`leaves` and `pointers_at` are export and probe paths. `tip`, `contains`,
`parent` and `block_at_height` are not — `block_at_height` is how
`get-block-info?` resolves a height.

**And converting `block_at_height` runs into clarity's own trait.**
`ClarityBackingStore::get_block_at_height` returns `Option<StacksBlockId>`, so a
storage failure underneath it has exactly two exits: panic, or answer `None`. The
second is the fail-open this task exists to remove, so the panic is the *correct*
one of the two at that boundary — it stops the block loudly and cannot be
mistaken for something the chain said. What is still worth doing is carrying the
typed error up to that boundary so the decision is made there and says so, rather
than being made in `nano-marf` by an `expect`.

That is a real refactor (`tip` alone has callers everywhere) and it is not the
dangerous direction, which is closed. Left visible rather than half-done.

## Typed read propagation completed, 2026-08-08

The remaining `VersionedMarf` reads now return `Result` through `MarfStore`,
`Vm`, `ChainState`, node startup and the diagnostic callers. Errors are never
converted into an absent block, root, pointer set or key. Clarity's fixed
`get_block_at_height -> Option` trait remains the one explicit fail-closed
boundary: storage failure reaches that boundary as `MarfError` and stops the
evaluation with a message naming the height instead of returning `None`.

File-backed MARFs retain their path in read errors. The regression output names
the state path, requested block and missing trie node without naming the key or
value. `a_storage_failure_after_open_seals_no_partial_block` opens a coherent
store, deletes a reachable node afterwards, begins a child, observes the typed
refusal, aborts, and proves both the last coherent tip and sealed-block count are
unchanged.

Verified with the workspace all-target check and the focused missing-node and
post-open failure tests. The crash/restart gates remain open here until their
serialized release run completes.
