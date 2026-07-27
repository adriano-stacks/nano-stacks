---
id: "010"
title: "M9: implement Nakamoto envelope validation and reward sets"
status: in-progress
priority: critical
effort: large
dependencies: ["005", "007", "008", "009"]
tags: ["m9", "chainstate"]
created_at: 2026-07-27
---

# M9: implement Nakamoto envelope validation and reward sets

## Objective

Validate Nakamoto block envelopes and signer reward sets against captured blocks.

## Tasks

- [x] Decode and re-encode Nakamoto headers and transaction vectors byte-for-byte.
- [x] Validate tenure linkage, signer signatures, and signer-weight thresholds.
- [ ] Derive reward sets and compare them with captured `stacker_set` fixtures.

## Acceptance Criteria

- Every captured block has the reference block ID, signer signature hash, and transaction Merkle root.
- Envelope validation accepts and rejects the same blocks as stacks-core.
- Reward sets and signer weights match the network for each captured reward cycle.
