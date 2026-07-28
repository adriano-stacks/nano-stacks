---
id: "014"
title: "M13: implement mining and hacknet interop"
status: completed
priority: high
effort: large
dependencies: ["013"]
tags: ["m13", "miner", "hacknet"]
created_at: 2026-07-27
completed_at: 2026-07-28
---

# M13: implement mining and hacknet interop

## Objective

Construct and submit an epoch-4 Bitcoin leader commitment, assemble the
corresponding Nakamoto block from checkpointed state, and interoperate with
stock signers and nodes on Hacknet.

## Tasks

- [x] Build a transaction-selection and block-execution candidate API that derives the committed state root.
- [x] Construct and submit canonical Bitcoin leader commitments with managed UTXOs.
- [x] Publish a valid block proposal to the active signer contract and collect signer responses.
- [x] Submit a threshold-signed Nakamoto block through a nano-won sortition.

## Acceptance Criteria

- A locally assembled block has the same state root as independent checkpoint execution.
- Stock signers accept the proposal and a stock node accepts the finalized block.
- Hacknet advances through a Bitcoin sortition won by the nano miner.
