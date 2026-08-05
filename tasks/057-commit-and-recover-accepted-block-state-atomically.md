---
title: "Commit and recover accepted block state atomically"
id: "057"
status: in-progress
priority: critical
type: bug
group: mainnet
tags: ["mainnet", "chainstate", "persistence", "recovery"]
created_at: "2026-08-03"
effort: large
dependencies: ["056"]
---

# Commit and recover accepted block state atomically

## Objective

Make an accepted block durable as one recoverable operation across the MARF,
Clarity side-store metadata, header database, parent/executed ancestry and tenure
accounting.

Those stores currently commit independently. The VM commits the MARF before it
flushes metadata, header write failures are logged instead of returned, and the
runtime writes accounting separately. A crash between boundaries can therefore
leave a root sealed without the data required to execute its child or answer
historical queries.

The existing restart test covers an orderly close and manually carries
accounting into the second half. It does not establish crash consistency.

## Tasks

- [x] Enumerate and document the durability boundary for every store changed by
      an accepted block.
- [ ] Add the smallest commit journal or equivalent recovery protocol that can
      distinguish prepared, accepted and fully durable blocks.
- [ ] Propagate header, metadata and accounting write failures to the block
      commit instead of logging and continuing.
- [ ] Make startup complete or roll back an interrupted commit idempotently
      before exposing an executed tip.
- [ ] Inject a failure after every commit boundary and reopen the state for each
      case.
- [ ] Exercise hard process termination during catch-up, tenure transition and
      restart, not only an orderly drop/reopen.

## Acceptance Criteria

- After termination at any durability boundary, reopening exposes either the
  complete parent or the complete accepted child, never a mixture.
- No sealed MARF root can lack its Clarity metadata, header, parent link or
  corresponding tenure accounting.
- Recovery is idempotent across repeated crashes and never re-executes or
  double-applies an accepted block.
- Fault-injection and kill tests reach the same final root, receipts and
  accounting as uninterrupted execution.

## The durability boundaries an accepted block crosses

In the order they happen, for one accepted block:

| # | Store | Written by | Transaction |
|---|---|---|---|
| 1 | MARF trie and block index (`marf.sqlite`, `.blobs`) | `MarfStore::seal_to` → `VersionedMarf::seal_to` | its own |
| 2 | Clarity metadata (`metadata_table` in the side store) | `flush_metadata`, called by `seal_to` **after** 1 commits | a second one |
| 3 | Block header (`block_header` row) | `Vm::record_block_header`, once per block | a third, and its failure is logged inside `nano-vm`, not returned |
| 4 | VM in-memory maps: `context.headers`, `burn_headers`, `tenure_starts`, `checkpoint_heights` | `record_block_header`, `record_burn_header`, `set_checkpoint_height` | none; 1–3 rebuild the first two on open |
| 5 | `TenureAccounting` (`accounting.json`) | `nano-node`'s `persist_accounting`, once per **catch-up round**, not per block | none; `fs::write` truncates in place |
| 6 | `tenure_start_heights`, `executed`, `parent_tenure_proof` | nothing | **never persisted at all** |

Row 6 is the finding that was not in the task. A restart begins with those three
empty, whatever the tip is:

- `executed` empty means `retract` and `retract_to` cannot rewind past the
  restart — a Bitcoin reorganization that reaches back that far is a silent
  no-op that reports "resume from the checkpoint".
- `parent_tenure_proof` absent means the first tenure after every restart skips
  the committed-seed check. It says so on stderr, and the message claims this is
  "expected only for the first tenure after a checkpoint", which is now wrong.
- `tenure_start_heights` empty means any header written for a tenure whose start
  block was executed before the restart records `tenure_start_height` = that
  block's own height. That is a Clarity-visible field (`get-tenure-info?`), and
  it is wrong for the rest of the tenure.

Row 5 is the crash hole the task names: a round that executes a hundred blocks
and dies before `persist_accounting` loses all hundred blocks of fee accrual
against a MARF sealed at the last of them, and a crash *during* the write leaves
a truncated file that fails `from_json` at the next start.

## Done inside `nano-chainstate`

The seal is now the commit point, and nothing durable happens before it. The
block header used to be written between the state-root check and the seal;
building it needs the block's pending state (the tenure height comes out of it),
so it is built there and returned, and written down only after `seal_block_to`
returns. A block that fails between the two therefore leaves no `block_header`
row and — more to the point — no `tenure_starts` entry, which is
first-write-wins and would have fixed a tenure's start height for every later
block.

`an_accepted_block_is_durable_with_its_header_and_parent_link` executes a block,
drops the chainstate, reopens the directory, and asserts the tip, the content
root, the parent link and the header all survived together. The rejection test in
[[056]] asserts the other side: neither the header nor the block state is there
for a block that failed.

## What remains, and where it has to go

None of this can be finished inside `nano-chainstate`; the stores are not its.

- **One transaction for rows 1–3, and 5 with them.** The natural shape is to
  persist the whole `ChainLedger` as a row in the side store, written inside
  `flush_metadata`'s transaction, so "the ledger is as of the tip" is an
  invariant rather than a hope, and to retire `accounting.json`. That needs a
  `nano-vm` API for a caller-supplied blob committed with the seal, a
  `Serialize`/`Deserialize` ledger here, and `nano-node` to stop writing the file.
  It also fixes row 6 for free.
- **Returning write failures.** `Vm::record_block_header` returns `()` and prints
  on failure; `flush_metadata`'s error is returned but only from inside `seal_to`,
  after the MARF has already committed. Both are `nano-vm`.
- **Completing or rolling back an interrupted commit at startup.** Depends on the
  journal above; belongs with whoever owns the open path in `nano-node`.
- **Kill tests.** Hard termination at each boundary is a `nano-node` or
  `nano-conformance` harness, not a unit test: it needs a process to kill.
