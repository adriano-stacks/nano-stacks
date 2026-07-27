---
id: "005"
title: "M4: implement SIP-005 transaction codec"
status: in-progress
priority: critical
effort: large
dependencies: ["003", "004"]
tags: ["m4", "codec"]
created_at: 2026-07-27
---

# M4: implement SIP-005 transaction codec

## Objective

Implement the epoch-4 SIP-005 transaction wire codec without production
dependencies on Stacks-Core.

## Tasks

- [x] Decode and encode standard and sponsored spending conditions.
- [x] Validate and preserve all fixture transaction payloads, post-conditions,
  principals, and Clarity-value encodings.
- [x] Compute transaction IDs and tagged transaction Merkle roots.
- [ ] Add generated differential tests for every transaction shape and invalid
  encoding boundary.
- [x] Replace preserved wire sections with typed transaction payload and
  post-condition models used by execution.

## Acceptance Criteria

- Every captured transaction decodes and re-encodes byte-identically.
- Transaction IDs and Merkle roots match the reference implementation.
- Generated reference transactions and invalid-wire differential tests are green.
