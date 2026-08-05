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
- [x] Add the smallest commit journal or equivalent recovery protocol that can
      distinguish prepared, accepted and fully durable blocks.
- [x] Propagate header, metadata and accounting write failures to the block
      commit instead of logging and continuing.
- [x] Make startup complete or roll back an interrupted commit idempotently
      before exposing an executed tip.
- [x] Inject a failure after every commit boundary and reopen the state for each
      case.
- [x] Exercise hard process termination during catch-up, tenure transition and
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

## The protocol that landed: prepare, then decide

Two boundaries, not five, and the second one *is* the decision. `Vm::commit_block`
takes the header and the encoded ledger and does exactly this:

1. one side-store transaction carrying the block's Clarity metadata, its
   `block_header` row and its `chain_ledger` row, committed with
   `synchronous = FULL` for that transaction only;
2. `marf.seal_to`, which is one SQLite transaction of its own.

The MARF commit is last and atomic, so a block is in the MARF **only if**
everything describing it is already on disk. There is no journal because there is
nothing to reconcile: a crash in the window leaves rows keyed by a block the MARF
never got, and the only way to read them is to stand on that block. Startup
therefore neither completes nor rolls anything back — it reads the ledger for the
block it resumes at and that ledger is the one that block sealed.

The whole ledger is one row rather than four because these four fields are only
ever meaningful together and as of one block; separate rows would be four ways to
be a block apart from each other, which is the bug. The one field Clarity queries
*by key* during execution — the tenure-start map — is not a second row either: it
is restored from the same blob into the map the VM already answers from, so there
is one writer and one reader.

`synchronous = FULL` for the prepare is what makes the ordering an ordering on
disk. At the store's usual `NORMAL` a power cut could keep the MARF commit and
lose the ledger row — precisely the sealed root with a ledger a block behind that
the ordering exists to prevent. It costs one fsync per block, and it is switched
back afterwards because every Clarity value write is its own autocommit and there
are thousands to a block.

Also landed:

- `Vm::record_block_header` returns its error instead of printing it, and so do
  `ChainState::backfill_ancestor_header` and `record_burn_header`. A header a
  later block reads through is not optional.
- Burn headers are persisted (`burn_header` table). They were memory-only, so a
  restart answered `none` for any burn block outside the 32-block window it
  re-seeded, where the run before it answered a hash — an sBTC withdrawal naming
  a deeper one is rejected on a chain that accepted it.
- `accounting.json` is no longer written. It is still read, once, for a state
  directory that predates this: the message says exactly what such a run cannot
  do, and the first block it seals writes a ledger.
- `ChainLedger::executed` is bounded at `REORG_REACH` (256), matching
  `nano-node`'s `RESUME_ANCESTORS`. It used to grow with uptime, and it is now
  serialized on every block.

### Verification

- `nano-vm`: `a_crash_between_the_two_boundaries_leaves_the_parent_and_its_ledger`
  prepares a child and stops exactly where a SIGKILL would land, then reopens: the
  tip is the parent and the ledger is the parent's.
- `nano-conformance`: `kill_during_replay` spawns `replay-blocks`, waits for it to
  seal its first block, sleeps a scattered 0–80 ms and sends **SIGKILL**, then
  reopens and compares all four ledger fields and the sealed content root against
  an uninterrupted run at the same block — 20 kills a run. The first version killed
  on a wall-clock delay alone and was wrong: as the state grew, reopening it took
  longer than the delay, so the later kills all landed during the open and sealed
  nothing. Waiting for the first sealed block is what makes every kill land inside
  the replay.
- `nano-conformance`: `restart.rs` no longer carries the accounting across by hand.
  A test that hands the state over cannot catch the fields nobody thought to hand
  over.

### Still open

- **A killed checkpoint import is still unrecoverable.** The import runs with
  journalling off, and `open_from_checkpoint` decides it has already imported when
  `marf_block` holds any row — so a process killed mid-import resumes on a partial
  trie, and the provenance file was written before the import even started. The
  kill test lets its first run finish for that reason. This is import atomicity
  rather than block commit atomicity, and it belongs with [[051]].
- **`xtask rebuild-accounting` repairs `accounting.json`, which the node now only
  reads during migration.** After a state has sealed one block under this change,
  the tool's correction is ignored. Either it has to write the ledger row or the
  repair path has to be "remove the ledger rows, then run it".
- **`TenureAccounting::earnings` is still unbounded**, so the ledger blob grows by
  about 130 bytes a tenure (13 KB on the real mainnet state today) and is now
  written per block. Nothing older than the maturity window is read, so it could be
  pruned; that changes payout derivation, so it wants its own change and its own
  test.
- **`BitcoinContext::headers` keeps every header this process recorded in memory as
  well as on disk.** Bounded by uptime rather than by anything that reads it.
