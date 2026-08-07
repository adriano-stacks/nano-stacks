---
id: "013"
group: build
title: "M12: implement StackerDB and embedded signer"
status: completed
priority: high
effort: large
dependencies: ["012"]
tags: ["m12", "signer"]
created_at: 2026-07-27
completed_at: 2026-07-28
---

# M12: implement StackerDB and embedded signer

## Objective

Sign validated Nakamoto proposals through `StackerDB` without equivocation,
using persistent writer-slot state and authenticated live Bitcoin context.

## Tasks

- [x] Encode, decode, and authenticate `StackerDB` chunks and signer messages.
- [x] Persist signer responses and writer-slot versions before publication.
- [x] Authenticate each live proposal against its current Bitcoin sortition and reward cycle.
- [x] Wire a checkpoint-backed signer process from explicit local configuration.
- [x] Confirm a response from a separately registered nano signer is accepted in Hacknet.

## Acceptance Criteria

- A restart cannot produce an equivocal response or reuse a consumed writer-slot version.
- The signer rejects a proposal whose miner, Bitcoin height, consensus hash, or reward cycle is invalid.
- A signature from a registered nano signer appears in a stock miner's accepted block on Hacknet.
