---
title: "Prove and pin why mainnet block 8708126 now executes"
id: "086"
status: in-progress
priority: critical
effort: large
type: bug
group: mainnet
dependencies: []
tags: ["mainnet", "vm", "clarity", "wasm", "conformance", "replay", "release"]
created_at: "2026-08-07"
---

# Prove and pin why mainnet block 8708126 now executes

## Objective

Prove why the captured mainnet block at Stacks height 8,708,126 now executes
through clarity-wasm, and pin the cause with the same receipts and state root as
the network. An earlier binary failed the fifth transaction with `Unexpected
principal data`; a binary rebuilt from `0f1628aa` later executed 500 blocks from
8,708,125 through 8,708,625 in one batch. Advancing once is important evidence,
but it does not identify the fixing change, prove the exact transaction and root,
or exclude a restart/cache/state-lifecycle effect.

## Reproduction

- Block fixture:
  `crates/nano-conformance/fixtures/mainnet/divergence/block-8708126.bin`.
- Transaction:
  `823f248a092638cbe4e08f30e5d60d872ff35d73a6a9ee98c790720f8ebd0db3`.
- Call:
  `SP1A27KFY4XERQCCRCARCYD1CC5N7M6688BSYADJ7.v0-5-market::supply-collateral-add`.
- Arguments: contract principal `sbtc-token`, two uints and a roughly 2 KiB price
  payload.
- Healthy replay state argument: `/home/aldur/mainnet-restored/state`, whose
  databases are under `state/chainstate`, if still present and coherent. The
  directory above it contains a small accidental store created by the earlier
  wrong-path diagnostic; do not use or delete that store blindly. Treat both as
  operator-owned diagnostic inputs, not committed release evidence.

## Tasks

- [x] Preserve and reconcile both observations: `named.log` failing transaction
      `823f248a…` at 8,708,126 and `run.log` executing 500 blocks from 8,708,125
      through 8,708,625 with root
      `44d76d9ab3592521cc412973677bf380d2c25011f6c772f45f80a6c296088e11`.
      Record binary/compiler identity, parent state and exact commands for each.
- [ ] Replay the exact block from an isolated copy of the 8,708,125 parent with a
      pre-failure binary and the passing binary, then bisect the intervening
      changes. In particular, test whether `be3ec64e`'s `BindingUses` walk under
      allowance lists fixed this call or merely happened to precede the restart.
- [x] Read the deployed `v0-5-market` source and analysis through the same metadata
      path nano-vm uses. Do not infer absence from `state-value` reading only trie
      data; use `check-module`/`Vm::contract_source` against the correct `state/`
      path after its node has stopped, and keep the wrong-path failure recorded.
- [ ] Add a focused harness that runs the captured transaction or its exact called
      function against the restored parent state and reports the malformed version,
      source value, expected type, linear-memory offset and producer expression.
- [x] Reduce from the real function body. The existing large-buffer argument and
      trait-dispatch guesses both pass, so retain them as controls and do not spend
      another iteration inventing nearby shapes without tracing the real source.
- [x] Identify whether the bad offset is produced during argument lowering, trait
      adaptation, nested call return handling, optional/response layout or another
      ABI boundary, and pin the smallest faithful reproducer against the reference
      interpreter.
- [ ] Fix the shared clarity-wasm layout/offset logic without a contract exception,
      block exception, interpreter fallback or healing path, if the bisect shows
      that the existing `BindingUses` fix is not the cause.
- [ ] Assert the captured transaction's result, costs, events and writes and the
      block's final state root against mainnet evidence.
- [ ] Resume the restored replay through the remaining staged blocks and record the
      next first divergence. The first post-restart batch reached 8,708,625; do not
      call that one batch a release run or silently fold a subsequent sortition
      stall into this VM task.
- [ ] Add the reproducer to the mandatory conformance suite and name this task in
      task 060's unchecked pristine-WASM replay item.

## Acceptance Criteria

- The captured block executes through clarity-wasm and seals the network's root.
- The focused regression fails on the actual pre-fix revision, passes on the
  causal change and proves the correct principal, not merely the absence of an
  internal error.
- A clean-process and same-process execution agree, excluding cache or stale
  compiled-module state as the explanation.
- Interpreter and WASM receipts, costs, events and writes agree for the minimized
  source and captured transaction.
- No special case names the contract, transaction, block height or fixture.
- Mainnet replay advances beyond 8,708,126 and the next frontier is recorded.

## Evidence that opened this task

Commits `37bbc1ce` through `0f1628aa` captured the block, identified the failing
transaction and target, and ruled out two plausible reductions. A release binary
built at 19:21 after those commits restarted from 8,708,125 and logged `executed
500 blocks, 8708125 to 8708625`; no source dump was produced and no focused
pre-/post-fix replay was recorded. The living record is
`crates/nano-conformance/fixtures/mainnet/divergence/README.md`. Task 060 owns WASM
conformance broadly and task 037 owns full replay; this task prevents a successful
restart from erasing an unexplained, consensus-critical failure.

## What is proved

**The cause.** The deployed source, fetched from a peer rather than from the
state (`/v2/contracts/source/...`, 77,100 characters, `publish_height`
8,668,585), takes `(price-feeds (optional (list 3 (buff 8192))))` — not the
`(buff 2048)` both earlier reductions minimised, which is why both passed. The
offset was never in the arguments. `ft-address` is a `let`-bound principal read
three times, the third read inside `((with-ft ft-address "*" amount))`;
`BindingUses` did not walk a list whose head is not a word, counted two, and the
second read released the binding's locals into the pool that entering the
allowance immediately borrowed from.

**The bisect, run rather than inferred.** `be3ec64e`'s walk reverted in
`wasm_generator.rs`, nothing else changed:

| revision | `binding_uses_counts_a_principal_read_from_an_allowance` | `allowance_principal::an_allowance_reads_the_principal_its_let_bound` |
|---|---|---|
| pre-fix  | FAILED, `[2, 2]` against `[3, 2]` | FAILED, compiler `Internal(InvariantViolation("Runtime(invalid utf-8 sequence of 1 bytes from index 0)"))` against the interpreter's `(ok (tuple (asset u1) (shares u46413)))` |
| post-fix | passed | passed |

Restart, the compiled-module cache and stale in-memory state are excluded by
construction: each side is a fresh in-process `Vm` with `ModuleCache::default()`,
and the two differ only in the pre-pass.

**Binary and log identity.** The passing binary is
`sha256:fca37025f7946aef81c9f0cf4d0d38b4259c6482b09a9767d631da1b9a4fdf9a`,
preserved with both logs under `/home/aldur/nano-086-evidence/`. It started at
19:15:53 on 2026-08-07, resumed `/home/aldur/mainnet-restored/state/chainstate`
sealed at `cf8bd32ee424cee5fc5fed4997f6a0f0ff9f7858528fc3a9b20936674b105bad`
(height 8,708,125), and logged `executed 500 blocks, 8708125 to 8708625, state
root 44d76d9ab3592521cc412973677bf380d2c25011f6c772f45f80a6c296088e11`. The
failing run in `named.log` resumed the same parent and the same seal, and failed
`823f248a…` of 8,708,126.

## What is not proved, and why

These need the live node stopped, and it is running:

- [ ] Replay the exact block from an isolated copy of the 8,708,125 parent with
      each binary. The parent is still a sealed version inside the MARF, and
      `Vm::open_existing` (task 087) now makes reading it non-mutating, but
      `refuse_uncommitted` correctly refuses while the node holds a 13 MB
      `marf.sqlite-wal`.
- [x] Assert the block's final state root against mainnet evidence. **Done, and
      it needed no state:** the follower path is
      `append_nakamoto_block_with_bitcoin_operations`, which executes under
      `RootPolicy::Verify` and refuses a block whose sealed root is not the one
      its header commits to (`nano-chainstate/src/lib.rs:1233`, `:2218`). So
      `executed 500 blocks, 8708125 to 8708625` already means all five hundred
      headers' roots matched, 8,708,126 among them. Confirmed independently
      against the network rather than taken from nano's own log: the peer-served
      block at 8,708,625 (`/v3/blocks/a4bfccd4795ed0598f447ee302e8407583e8881ba7e6a9c658ec0ed6f058e206`,
      2,038 bytes, `chain_length` 8708625, consensus hash
      `f2b9a8b62b38eaa82a1e570493484690cd09d1e5`) carries `state_index_root`
      `44d76d9ab3592521cc412973677bf380d2c25011f6c772f45f80a6c296088e11` at
      header offset 101 — byte for byte the root nano sealed.
- [ ] Assert the captured transaction's *receipt*: status, cost and events. The
      root match covers every write, because a write that differed would move it,
      but a cost is not in the root and decides block admission, and an event is
      not in the root at all. This needs an event-observer capture for the block
      or a stopped node to re-execute it under one.
- [ ] Resume the restored replay and record the next first divergence. The node
      reached 8,708,625 and then stalled on a local sortition that cannot name the
      peers' burn view; that is task 088, not this one.
- [x] Add the reproducer to the mandatory conformance suite
      (`allowance_principal`, and `clar2wasm`'s pre-pass test).
- [ ] Name this task in task 060's unchecked pristine-WASM replay item.

Until the receipt and root are compared with the network, this task stays open:
the cause is pinned and the regression is in the gate, but "the block executes"
and "the block executes to the chain's answer" are different claims.
