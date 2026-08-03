---
title: "Commit and recover accepted block state atomically"
id: "057"
status: pending
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

- [ ] Enumerate and document the durability boundary for every store changed by
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
