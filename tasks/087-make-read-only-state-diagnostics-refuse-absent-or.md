---
title: "Make read-only state diagnostics refuse absent or wrong directories"
id: "087"
status: completed
priority: high
effort: medium
type: bug
group: build
dependencies: ["065"]
tags: ["tooling", "state", "diagnostics", "reproducibility", "release"]
created_at: "2026-08-07"
completed_at: 2026-08-08
---

# Make read-only state diagnostics refuse absent or wrong directories

## Objective

Commands that claim to inspect state must never create or modify state. Today
`state-value`, `check-module` and other xtask readers call `MarfStore::open`, whose
contract is "open, creating if absent." A wrong path therefore creates empty
`marf.sqlite` and `clarity.sqlite` files and reports an absent value, turning an
input error into plausible but false evidence.

## Tasks

- [x] Add an explicit read-only/open-existing state API that refuses unless the
      expected MARF, Clarity side store, network identity and coherent tip already
      exist. Use SQLite read-only mode where practical.
- [x] Route every non-mutating xtask command through it, including `state-value`,
      `check-module`, `probe-root`, `block-info` and source/analysis inspection.
- [x] Keep commands that intentionally repair, import or backfill on an explicit
      writable API and make that distinction visible in help text.
- [x] Add command-level tests proving a nonexistent or one-directory-too-high path
      exits non-zero and creates no directory, database, WAL or metadata row.
- [x] Make an existing but wrong/empty chainstate fail by name rather than returning
      `no value`, defaulting to mainnet or recording compiler identity.
- [x] Correct the block-8,708,126 reproduction notes to use
      `/home/aldur/mainnet-restored/state`, whose `chainstate/` is the real 40 GB
      state, not `/home/aldur/mainnet-restored`.
- [x] Inspect the accidentally created `/home/aldur/mainnet-restored/chainstate`
      before removing it. Do not delete it merely because this task identifies it;
      another running process or operator may still own it.

## Acceptance Criteria

- Every read-only diagnostic is filesystem-non-mutating on success and failure.
- A path typo cannot create a state, choose a default network or produce an
  absence result.
- Tests compare the complete directory tree before and after failed inspection.
- Task 086 can retrieve the real deployed contract source without stopping on a
  false empty store.

## Evidence that opened this task

The divergence README ran `cargo xtask state-value
/home/aldur/mainnet-restored tip ...` and concluded that the contract key was
absent. The live configuration says `working_dir =
"/home/aldur/mainnet-restored/state"`; the real databases are under
`state/chainstate` and are tens of gigabytes. The mistaken command created a second
`/home/aldur/mainnet-restored/chainstate` containing a 24 KiB MARF and 52 KiB
Clarity database at the time of the audit. `MarfStore::open` documents that it
creates the directory and databases if absent, so this is deterministic behavior,
not a shell accident.

## What landed

`MarfStore::open_existing`, `Vm::open_existing` and `ChainState::open_existing`
open a state that is already there and write nothing at all. Not merely
`SQLITE_OPEN_READ_ONLY`: a read-only connection to a WAL database still creates
the `-shm` wal-index, so both databases are opened through
`nano_marf::immutable_uri` with `immutable=1`, which takes no lock and builds no
index. That is only sound if no writer is mid-flight, so
`nano_marf::refuse_uncommitted` refuses a database whose `-wal` or `-journal`
still holds frames — which is also the honest answer when a node owns the state.

The network comes out of the state rather than from the caller, and an absent
`chain_identity` row is a refusal rather than a default. No `engine_identity` row
is written, and a read-only `Vm` keeps its compiled modules in memory rather than
creating the on-disk cache directory inside the state it promised not to touch.

`xtask`'s readers (`state-value`, `check-module`, `block-info`, `probe-header`,
`eval`, `probe-root`, `call-both`, `call-both-tx`) go through it. The writers
(`backfill-header`, `import-headers`, `heal-contracts`) go through
`open_state_vm_for_writing`, and `cargo xtask` with no arguments now lists the
two groups apart.

`xtask/tests/read_only_state.rs` compares the complete directory tree — every
path and every byte, so a metadata row counts — before and after each inspection,
for a path that is not there, a path one directory too high, an empty
chainstate, files that are not databases, a state that answers, and a state a
node still owns.

## Evidence

The behavior this replaces, measured on the writable opener that the repair
commands still use:

```
$ xtask heal-contracts /tmp/ro-demo/oldbehaviour
the state is sealed at no block
$ echo $?
0
$ find /tmp/ro-demo -mindepth 1
  /tmp/ro-demo/oldbehaviour/chainstate/marf.sqlite
  /tmp/ro-demo/oldbehaviour/chainstate/clarity.sqlite
  /tmp/ro-demo/oldbehaviour/chainstate/native-modules/1
```

and after, on a reader:

```
$ xtask state-value /tmp/ro-demo/typo tip 'vm-epoch::epoch-version'
cannot open the state: NotAState("/tmp/ro-demo/typo/chainstate is not a directory")
$ echo $?
1
$ find /tmp/ro-demo -mindepth 1      # nothing
```

The accidental store was inspected and left alone: 24 KiB `marf.sqlite` and
52 KiB `clarity.sqlite` under `/home/aldur/mainnet-restored/chainstate`, both
stamped 19:15 on 2026-08-07, beside the real 40.8 GB / 15.5 GB pair under
`state/chainstate`. It is not deleted here; another operator may still want it as
evidence.

## Final real-state check, 2026-08-08

The node now uses `/home/aldur/mainnet-tip/state`, so the restored state was no
longer owned and the remaining end-to-end check could run. The already-built
`xtask check-module` read
`SP1A27KFY4XERQCCRCARCYD1CC5N7M6688BSYADJ7.v0-5-market` through
`ChainState::open_existing` and wrote the requested 77,100-byte source outside
the state directory.

Before and after the command, the procedure hashed both databases, recorded
their stat tuples and inventoried every entry in the complete state tree. All
three comparisons were byte-identical. The 40,861,437,952-byte MARF remained
`dbd0e5fd8f96edb6c9b6776d5e51efd7ec442cb8b2659097e7abd091eab47148`; the
15,563,264,000-byte Clarity side store remained
`48f34cdca4f3fb3d94b1fd13b19a37ed52c183e92602a4f1265411f35e07118a`.
The evidence bundle is `/tmp/tmp.aohOLWuR3s` on this machine. This closes the
last acceptance criterion without stopping or touching the live node.
