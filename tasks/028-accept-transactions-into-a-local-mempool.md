---
id: "028"
group: build
title: "Accept transactions into a local mempool"
status: completed
priority: high
effort: medium
type: feature
dependencies: []
tags: ["mainnet", "mempool"]
created_at: 2026-07-30
completed_at: 2026-07-30
---

# Accept transactions into a local mempool

## Objective

nano has no mempool. It borrows one: `SyncClient::mempool_page`
(`crates/nano-sync/src/lib.rs:333`) pages a peer's `/v2/mempool/query` and holds
the result for the tenure it is mining.

That was the right shape for M13 — it proved a nano tenure can carry user
transactions. It leaves nano unable to accept a transaction from anyone, so no
wallet can reach it, and its miner cannot produce a block without a stacks-core
peer willing to share.

## Tasks

- [x] Hold submitted transactions, keyed so a replacement by fee is possible.
- [x] Admit on the checks stacks-core admits on: signature, nonce, fee, size and
      the current chain tip.
- [x] Drop what a block confirmed and what has become invalid.
- [x] Order candidates for block assembly by fee rate under the epoch-4 limits.
- [x] Keep the peer mempool as a source that feeds the local one, not as the
      only one.

## Acceptance Criteria

- A transaction submitted to nano is mined into a nano tenure without a peer.
- A transaction the chain has confirmed or invalidated leaves the mempool.
- Admission rejects what stacks-core rejects, with the same reason.

## Known limits

- The refusals that need a VM over the tip — `NoSuchContract`,
  `NoSuchPublicFunction`, `BadFunctionArgument`, `ContractAlreadyExists` — are
  deferred to block assembly, where `admit_candidates` runs the transaction
  anyway. A test pins the divergence rather than hiding it.
- A tenure-start block still carries no user transactions: that path has
  seconds to publish a proposal and the mempool fill costs two round trips.
  Continuation blocks carry them.
- `fill_mempool` reads one account per principal per poll. That wants batching,
  or the local account index below, before mainnet.
- The miner still asks a peer for nonces and balances. `ChainState` now answers
  `account_balance` and the VM answers `account_nonce`, so a `ChainTip` over
  nano's own executed state would close both this and the deferred refusals
  above.
