# One durable linearization point: the decision-record seal

Design for task 141: give each executed block one durable linearization point
and one canonical decision record, shrinking the reasoning and recovery
surface of today's MARF-plus-side-store protocol without breaking checkpoint
compatibility. Task 140's `epoch4-consensus` crate defines the record this
design makes durable.

## What exists today, measured

A committed block today is spread across stores with a deliberate ordering:

1. one `clarity.sqlite` transaction (`PRAGMA synchronous=FULL`) carrying the
   block's Clarity metadata, its header row and the encoded `ChainLedger`
   (tenure accounting, executed list, tenure heights, parent proof);
2. the MARF's own commit in `marf.sqlite`, last and atomic — *it* is the
   decision today: the block exists exactly when everything beside it is
   already durable, and a crash in between leaves side rows nothing can reach.

Beside that pair sit `staging.sqlite` (candidates), `executed.sqlite`
(bounded served-block and receipt-commitment archive), the sortition
tracker's JSON files and, in the dev node, `accounting.json`. Each has its own
crash story; `kill_during_replay`, `kill_during_import`, `storage_faults` and
`binary_restart` prove the pair's parent-or-child shape, and the torn-copy
incident recorded on task 053 proves what the spread costs: four files
written in sequence cannot be snapshotted or reasoned about as one thing.

## The record to make durable

`epoch4_consensus::DecisionRecord` (schema `nano-stacks/epoch4-decision/v1`):
block and parent identity, height, burn view, typed verdict, sealed state
root, five cost dimensions, the bounded receipt commitment, compiler identity
and profile fingerprint — content-addressed by `Sha512_256` over its canonical
JSON. Task 141 binds it into the seal so that "this block is committed" and
"this is what committing it decided" become one durable fact. The record adds
what the side rows do not state today: the verdict itself, the refusal
identity when there is one, and the receipt commitment, which currently lives
only in the prunable archive.

## Evaluated designs

**A. One database, one transaction (evaluated first, as the task requires).**
Fold the MARF trie tables and the side store into a single SQLite database;
one transaction writes trie, metadata, header, ledger and decision record,
and its commit is the linearization point.
*For:* one WAL, one fsync discipline, one file to snapshot; recovery is
SQLite's alone. *Against, and decisive for now:* `marf.sqlite` on mainnet is
tens of gigabytes with its own page-cache and checkpoint behavior; merging
doubles checkpoint-import surface (the bundle format pins
`stacks-core-marf-sqlite-v2`); and the whole checkpoint compatibility story —
import, provenance, published manifests, the CI sample — would change format
in the same release that changes authority boundaries (task 140). Two large
consensus-format migrations in one step is exactly the kind of coupled risk
this program has refused elsewhere.

**B. Keep the two stores; make the decision record the visibility point
(chosen).** The record is appended in the *same* `clarity.sqlite` transaction
that already carries metadata, header and ledger, into a `decision_records`
table keyed by block id with its content hash; the MARF commit stays the
durability tail exactly as today. One rule then changes what "committed"
means to every reader: **a block is visible if and only if its decision
record row exists and the MARF holds its sealed trie.** Readers that today
infer commitment from the MARF alone (chain reads, RPC, restart recovery,
fork switching) move to the record, which turns today's unreachable-side-rows
crash residue into a defined state: record present, trie absent → the block
is not committed, recovery discards the row; trie present, record absent →
impossible, because the record's transaction precedes the seal and the seal
refuses without it.
*For:* no storage-format migration of the trie, no checkpoint-format change
(the record table is additive; an imported checkpoint starts it empty), the
existing kill/fault evidence carries over, and the record lands in the same
already-`synchronous=FULL` transaction — no added fsync.
*Against:* two stores remain; the simplification is in the reasoning rule and
the record, not in file count.

**C. Append-only seal log.** A separate fsynced log of decision records as
the linearization point. Rejected: it *adds* a third durability domain and
re-derives everything SQLite's WAL already gives the side store.

## Filesystem and power-loss assumptions, stated

Both stores run SQLite WAL with `synchronous=FULL` on the commit path; the
protocol relies on: WAL commit records being atomic and ordered per database;
`fsync` honoring barriers (no volatile write-cache lying); and nothing
cross-database — the parent-or-child shape is achieved by ordering the two
commits, never by assuming they are atomic together. `ENOSPC`/`EIO` at either
boundary must surface as typed storage errors (the `nano-marf` panic-removal
under task 079 is the precedent) and leave the pre-block state readable.
Btrfs reflink copies of a *running* directory remain non-snapshots; that is
documented operator guidance, not a recovery mode.

## Migration and evidence plan

1. Add the `decision_records` table and write records for every newly sealed
   block (additive; old blocks have none, and the visibility rule falls back
   to the MARF for heights at or below the record floor — the checkpoint
   anchor).
2. Move readers (chain reads, restart recovery, fork switch, RPC/health) to
   the record-and-trie rule behind the existing test gates; `binary_restart`,
   `kill_during_replay` and `storage_faults` must pass unchanged, plus new
   injections at the record write, the seal, and between them.
3. Replay the offline corpus and a mainnet catch-up range; every root and
   receipt commitment in the records must equal the oracle's.
4. Only then remove the legacy inference paths — no runtime fallback stays,
   matching the engine rule.

Performance bound: the record is one small row inside an existing
transaction; the catch-up and tip-following bounds recorded for the qualified
follower are the acceptance floor, re-measured after step 2.
