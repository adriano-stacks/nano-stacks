---
id: "037"
title: "Replay mainnet from the epoch 4.0 boundary"
status: pending
priority: critical
effort: large
type: feature
dependencies: ["020", "021", "022", "023", "024", "025", "048", "056"]
tags: ["mainnet", "replay", "conformance"]
created_at: 2026-07-30
---

# Replay mainnet from the epoch 4.0 boundary

## Objective

The milestone that decides whether any of the rest of it worked. M10 proved nano
computes the same chain state as stacks-core for 340 Hacknet blocks from a
regtest checkpoint. This is the same claim against the chain that matters.

Everything before it is a component check. The oracle is the same as M10's —
`state_index_root` per header and every receipt from the event observer — pointed
at mainnet blocks after the 4.0 boundary instead of captured Hacknet ones.

Replay depth is the metric again. It is measured from the durable executed tip,
never from fetched, staged or peer-reported height.

## Tasks

- [x] Capture a mainnet checkpoint at or after the 4.0 boundary, with the
      blocks and the burn blocks that follow it. Receipts need an observer.
- [x] Teach the fixture tooling and the scoreboard about a mainnet capture.
- [x] Make `import_checkpoint` work in bounded memory, so a mainnet-sized MARF
      can be imported at all.
- [x] Replay forward and report the first divergence with the field that
      diverged.
- [ ] Work the divergence point forward until it stops moving for a real reason
      or reaches the tip.
- [ ] At a matching-receipts root divergence, capture the exact ordered
      `(key, serialized value)` journal from a pristine parent for every
      transaction and native effect.
- [ ] Feed one identical journal through nano's MARF and the pinned stacks-core
      MARF, including rewrites, forks and the imported mainnet checkpoint, to
      separate execution differences from trie differences.
- [ ] Compare compiler and interpreter journals before sealing; a fallback that
      reaches the same values in a different order is diagnostic, not a
      production conformance result.
- [ ] Keep a bounded slice of the capture in CI as a regression gate.
- [ ] Make the mainnet gate explicitly skip or fail when its fixture is absent;
      an environment-variable early return must not appear as conformance.
- [x] Check what mainnet *can* serve without a chainstate — the block envelope
      against the published reward set — and keep that in CI meanwhile.

## Acceptance Criteria

- `cargo xtask scoreboard` reports a mainnet replay depth alongside the Hacknet
  one.
- Every replayed mainnet header has the matching `state_index_root`.
- Every replayed transaction has the matching receipt, including status, costs
  and events.
- The replay runs offline from captured fixtures.

## Audited frontier and next oracle

The checkpoint is 8,665,600 and the durable store is sealed through 8,665,779:
**179 consecutive mainnet roots match**. The sync staging store separately holds
28,458 later blocks through 8,694,237. That staged height is download progress,
not conformance depth.

The first current mismatch is 8,665,780. All five transaction results, costs,
events and inspected values agree with mainnet, while the root does not. The
`as-contract` compiler fix removed a real wasm failure but did not close this
root mismatch. Skipping a zero-valued liquid-supply update changes the root but
still does not produce mainnet's root.

The next useful artifact is therefore the exact write journal from a clean
8,665,779 parent. The in-progress direct MARF lockstep test is the right shape,
but synthetic keys alone cannot decide whether this real block diverges before
or during trie sealing. Runs must use clean accounting after
[[056-make-rejected-block-execution-leave-no-state]]; the current live state has
been altered by rejected retries.

## The boundary is now

Checked against `api.mainnet.hiro.so` on 2026-07-30:

- mainnet runs `stacks-node 4.0.1`
- `.pox-5` activates at burn height **960,230**, and `validate_epochs` requires
  `pox_5_activation_height == Epoch40.start`, so that is the 4.0 boundary
- the burn tip was **960,227** when this was written — three Bitcoin blocks out

So the checkpoint this task needs becomes takeable within the hour, and every
mainnet parameter is now known rather than assumed: reward cycle 2100, prepare
phase 100, first burn block 666,050, boot address
`SP000000000000000000002Q6VF78`, chain id `0x00000001`.

It also means SIP-031 is live and not hypothetical. The mainnet schedule starts
at burn 907,740, long past, so every tenure nano executes mints 475 STX to
`.sip-031`, rising to 1,140 at burn 960,300 — seventy-odd blocks after the fork.
[[025-apply-the-sip-031-emission]] is load-bearing from the first block.

## The chainstate is obtainable after all

Hiro publishes dated archives of a synced mainnet chainstate, and one of them
is a **4.0.1 node dated 2026-07-30** — the same day mainnet crossed the epoch
4.0 boundary at burn 960,230:

```
https://archive.hiro.so/mainnet/stacks-blockchain/mainnet-stacks-blockchain-4.0.1-20260730.tar.zst
```

208 GiB compressed, and it holds exactly the three databases a capture reads —
`chainstate/vm/`, `chainstate/blocks/nakamoto.sqlite` and
`burnchain/sortition/marf.sqlite`. So "this needs a node nobody here has" was
wrong: it needs a large download, which is a different kind of problem.

Two things were needed to use it, one of them now done:

- `cargo xtask capture-fixtures` assumed Hacknet's layout, where a node's
  directories sit under `stacks-miner-1/nakamoto-neon`. **`--node-root` now
  names the directory directly**, which is what an archive extracts to.
- Streaming the archive through `zstd | tar` cannot resume, and a 208 GiB
  stream *will* be interrupted — it was, twice, with `Unexpected EOF in
  archive`. It has to land on disk first, resumably.

## The capture works

The archive downloads and extracts, and **a mainnet capture succeeds**:

```
captured 100 real Nakamoto blocks with a portable MARF checkpoint
chain_id = 1
checkpoint_stacks_height = 8665600
checkpoint_state_index_root = "67596465d4a6642ad6fcec1df57c6ef758fcdb0003c7ed7f952e3ced1d7f44ec"
first_stacks_height = 8665601
stacks_core_rev = "62e03cc"
```

Getting there needed care in five places, each found by running the command
rather than predicting it — see the commit. Two are worth repeating here:
mainnet has a million burn blocks and more than one snapshot per height, so the
capture takes the canonical one across the window; and mainnet runs `62e03cc`
where the in-process oracles pin `efc34a0`, so `--accept-node-revision` takes
the build by name and records it, rather than waving the guard through.

Also learned about the archive itself:

- The host throttles **per connection** — one stream settles to 6 MB/s while a
  second fresh one gets 42 — so twelve parallel ranges pull it at ~110 MB/s.
- `curl -C -` and `-r` are incompatible, and retrying a range while appending
  duplicates it, because curl re-requests the whole range. Parts must be
  written with `-o`.
- The archive dated the day mainnet crossed the boundary stops **27 burn blocks
  short of it**. The next day's reaches burn 960,341.

## Replay depth on mainnet: 118 blocks

The durable executed tip moved from the checkpoint at 8,665,600 to **8,665,718**
— a hundred and eighteen real mainnet blocks whose `state_index_root` matched,
read from the store rather than inferred, and reported through
`/nano/sync_status` as the executed height.

Each divergence was a real bug, found against the network and fixed with a test:

| height | what diverged |
|---|---|
| 8,688,027 | the SIP-040 `MaybeSent` post-condition would not decode |
| — | Bitcoin hashes read in opposite byte orders from RPC and Esplora |
| 8,665,615 | `get-burn-block-info?` answered `none`: no burn header, no tip sortition |
| 8,665,623 | a deployment naming a contract deployed 64 blocks later was fatal, not failed |
| 8,665,685 | a trait nested in a list of tuples was not recovered from its declared type |
| 8,665,694 | `with-all-assets-unsafe` charged a cost with no entry in the 4.0 table |
| 8,665,695 | a call into a contract using `at-block` was fatal, not failed |
| 8,665,699 | every contract read the block height as one more than the network did |

### The first divergence the receipts could not narrow

At 8,665,699 nano and mainnet agreed on everything a receipt can express — the
result `(ok u2)`, all five cost dimensions, and every key written — and the
roots still differed. That is the signature this plan names: a mismatched root
with matching receipts is **MARF or write ordering**.

Tracing the writes settled it in one run. Six keys, in the order stacks-core
writes them, with the same values — except one, and it was a *value*, not an
ordering: `oracle-report-block` held `u8665700` where mainnet held `u8665699`.
Two branches of the current-block height disagreed, and every contract reading
it saw one past the block it was in. A second, smaller write was there too:
incrementing the liquid supply by zero for matured rewards that did not exist,
which stacks-core guards on having payouts at all — and a write the network did
not make is a leaf in the trie it does not have.

## Replay depth 179, and the oracle that got it there

Reading the *accounts back from the network at the diverging block* is what
turns "the root differs" into "this key differs". It found a real consensus bug
at 8,665,722 that no receipt could show: one account short by exactly
26,404,093 uSTX, which is tenure 251,222's fees less tenure 251,221's. A
Nakamoto tenure hands **its own** anchored fees to the tenure before it, and the
derivation was paying the earlier tenure's — the right account, the wrong
amount. The capture's export had the rule right; only the derivation did not,
which is why it survived until a tenure matured that the checkpoint carried no
effect for.

That took replay from 118 to **179 blocks**, through two more compiler failures
on the way: clarity-wasm emitting wasm that will not load for a contract it
compiles under an older epoch, which now fails the call rather than the node.

### Where it stops now: 8,665,780

Same shape, and this time everything comparable agrees. All five receipts match
mainnet's statuses and results; every account balance and nonce written in the
block matches when read back from the network at that block; both pool data
vars match; both `univ2-core` map entries match. And the root still differs.

Then the events, which is as far as the network will describe a block from
outside: **1, 11, 1, 47, 1 per transaction, matching nano exactly**. So every
STX movement, every token event and every `print` agrees, in count.

What that leaves is a key nano does **not** write, that moves no balance and
raises no event. `vm-metadata::` is the shape that fits — a contract's data size
changes without an event — and this plan lists those among the Clarity key
strings the MARF holds, where nano keeps them in the side store.

### Knowing which epoch a height was in

`get_stacks_epoch` answered "epoch 4.0" for every height, which is only right at
the tip, and rebuilding a contract the current epoch rejects jumped to the
oldest epoch its Clarity version allowed — sending a Clarity 1 contract back to
epoch 2.0, the least exercised paths in the compiler. Mainnet's boundaries are
now transcribed from stacks-core and pinned against it, and the rebuild walks
epochs newest first.

That took the contracts needing the interpreter from **forty crosschecks to
one**: `SP2H674PRTZV6YW56K0FMR7GDGZE4ZC5HMYZ3CDEV.flea`, which compiles under
epoch 4.0 and produces a module wasmtime refuses — "expected i64, found i32" at
byte offset 74,624.

It is an adversarial contract: one trait returning
`(response (list 20 (response uint uint)) uint)` and forty near-identical
functions calling it. Neither shape reproduces the fault on its own — both are
kept as regression coverage, since this plan expects divergences around trait
lists — so the trigger is scale, and fixing it is work in the code generator
rather than a word.

**And the fallback itself is the likely answer.** Block 8,665,780 runs through
the interpreter, because a contract in it compiles to wasm that will not load.
A MARF packs a node's pointers in the order its keys were first written, so two
runs reaching the same values by writing them in a different order seal
different roots — which is exactly the shape here: every value equal, every
event equal, the root different. The interpreter is a way to carry a replay
forward and find the next divergence; it is not a way to follow a chain, and
the real fix is the clarity-wasm codegen bug behind it.

Enumerating mainnet's own writes would settle it, and both routes are closed for
now: there is no event observer on a mainnet node, and stacks-core will not open
the archive's MARF to read it. That is not truncation — the blob file is exactly
as long as the index says it should be — but an open path that seeks in a SQLite
blob where the trie is in the flat file beside it, read-only and
`external_blobs` alike. Getting that open to work is the one piece of tooling
that would unlock the general oracle.

### The first tenure start, and what was ruled out there

Replay now reaches **8,665,722**, the first tenure start past the checkpoint,
and every observable at it agrees with mainnet:

| what | nano | mainnet |
|---|---|---|
| transactions | tenure-change and coinbase, both `(ok true)` | the same |
| matured credit | 1,000,000,000 to the tenure's recipient | the archive's own figure |
| SIP-031 mint | 475,000,000 to `SP000000000000000000002Q6VF78.sip-031` | the same amount and recipient |
| liquid supply after | 1,854,643,201.554249 STX | **1,854,643,201.554249 STX** |
| write set | ten keys, in stacks-core's order | — |
| burn operations | five, none writing Clarity state | — |

and the root still differs. The trace that produced this covers `put_data`
only, and `insert_metadata` writes land in the MARF too — so the next thing to
instrument is metadata, not another data write.

### Where it stopped before

At **8,665,719**, and it is structural rather than arithmetic:
[[055-answer-block-info-for-blocks-before-the-checkpoint]]. `HeadersDB` answers
only for blocks nano executed itself, so everything before the checkpoint is
`none`, and a contract reaching `get-stacks-block-info?` for older history gets
an answer the network never gave.

Also carried forward from the wrong turn before it: a reachability check over an
imported checkpoint, which asserts every contract the side store names can still
be walked to in the trie, and diagnostics that separate "the trie has no such
key" from "the side store has no such value".

## The near-tip claim was the followed view, not execution

On 2026-08-01 the node's `/v2/info` stayed within zero to three blocks of the
peer for eighty minutes. That was recorded here as roughly 18,290 mainnet state
roots matching. A read of the durable store on 2026-08-02 disproved it:

```
select count(*), max(height) from marf_block;
8665602|8665601
```

The store still ends at the single anchor applied after the checkpoint. Startup
said the same thing — `sealed at ... height 8665601` — and no `accounting.json`
exists, which means `CheckpointExecutor::follow_to_tip` has never returned one
successful batch. The near-tip heights came from `NodeView.node_info`, copied
from the peer and published before execution. Absence of `StateRootMismatch` in
the log therefore proved that no mismatching block was executed, not that every
peer block was executed successfully.

The live process later fell more than a thousand blocks behind and entered a
stable loop of `peer tenure does not extend the followed chain`, so the follower
improvements are useful but not yet a durable mainnet sync result either. See
[[046-distinguish-followed-and-executed-chain-tips]] and
[[047-make-mainnet-synchronization-monotonic-and-restart]].

The state-root half of this task remains open. Evidence must come from the
offline scoreboard or from the durable executed tip and an explicit count of
roots verified, never from the peer-facing RPC height or from absence of an
execution error.

## Historical blocker: checkpoint import memory

The first import was killed after exceeding 15 GB resident against a 142 GB MARF
and 229 GB blob store. The lazy, reachable-record import work fixed that blocker:
the mainnet checkpoint now imports into the 31 GB machine and the node resumes
from it. This establishes bounded-enough bootstrap behavior, not forward replay.

## The unlock heights the capture needs

Checked against stacks-core rather than transcribed, because a wrong one
changes what execution sees:

| flag | mainnet |
|---|---|
| `--pox-v1-unlock-height` | 781,552 |
| `--pox-v2-unlock-height` | 787,652 |
| `--pox-v3-unlock-height` | 840,361 |
| `--pox-v4-unlock-height` | 960,230 |

The first three are `POX_V{1,2,3}_MAINNET_EARLY_UNLOCK_HEIGHT`, each one past
its epoch's burn height. The fourth is pox-5's activation, which
`validate_epochs` ties to the epoch 4.0 boundary and which
`api.mainnet.hiro.so/v2/pox` reports as the same 960,230.
`mainnet_unlock_heights_match_stacks_core` pins all four.

## What the archive still cannot give

**Receipts.** `events/new_block/*.json` come from an event observer attached to
a running node, and an archive holds no events. So a mainnet capture can carry
`state_index_root` per header — the claim this task is really about — and not
the per-transaction receipts that the Hacknet capture checks alongside it.

Getting those means either running a node against the restored chainstate with
an observer attached, or reading `execution_cost` back from
`/extended/v1/tx/:txid`, which is a weaker oracle than the event stream and
covers costs rather than events.

## What still blocks it

The archive supplied the MARF and sortition snapshots, and the public API
supplied the blocks. The remaining fixture problems are now concrete:

- `native-effects.json` predates the complete maturity-window export and cannot
  carry execution across the next tenure; see
  [[048-carry-complete-mainnet-tenure-accounting]]
- receipts and events still require an observer attached to stacks-core
- the 100-block capture has not produced an offline scoreboard result
- no bounded mainnet slice runs in the repository's CI

## What mainnet already proves

Execution needs a chainstate. **The envelope does not** — the reward set is
published at `/v3/stacker_set/:cycle` and the envelope is self-contained — so
that half is now checked against the chain that matters rather than against
Hacknet.

`cargo xtask verify-block <block.bin> <stacker_set.json>` checks a block
against the set published for its cycle. Twenty consecutive blocks from the
mainnet tip were **accepted, none rejected**, carrying between fourteen and
nineteen signatures of twenty-five signers and between seven and eight tenths
of the weight. nano derives the same signer signature hash mainnet signed,
recovers the same keys from it, orders them the same way, and counts the same
weight against the same threshold.

Five of them and the reward set are kept under `fixtures/mainnet/`, so the test
runs offline, and `verify-block` takes any block a node will serve for a wider
check. The repository has no root CI workflow invoking that gate yet.

This is M9 against mainnet. It says nothing about execution, which is the half
this task is really about and which remains open until the captured chainstate
replays with an explicit depth and first-divergence result.

## Mainnet execution has started, and what it found

The executed tip is durable and moving: **8,665,601 → 8,665,622**, twenty-two
real mainnet blocks whose `state_index_root` matched the header. Reported from
`/nano/sync_status` as the executed height, which is the only number this task
will accept as evidence.

Getting the first block to execute at all took four fixes, each a real
divergence rather than an infrastructure problem:

- **`MaybeSent`** — nano's codec rejected the SIP-040 non-fungible condition
  code, so the block carrying it would not decode and the descent stopped at
  8,688,027 by any route.
- **Bitcoin hash byte order** — a `BitcoinBlock` records the displayed order,
  Bitcoin Core's RPC returns the internal order and is reversed to match, and
  Esplora returns the displayed order and was reversed as well. Every read
  looked like a reorganization.
- **The burn header a block lands on** — the production node never set it, and
  Clarity resolves `get-burn-block-info?` through the *tip sortition* from
  epoch 3 on, which nano answered `none` to. sBTC's withdrawal path compares
  the hash it was signed for against that, so nano returned
  `ERR_INVALID_BURN_HASH` where mainnet returned `(ok true)` and diverged at
  **8,665,615**.
- **Compile on demand** — a contract whose module is not loaded is reported two
  ways, and only one was recognised.

## Where it stops now

A deployment references `SP4SZE494VC2YC5JYG7AYFQ44F5Q4PYV7DVMDPBG.native-pool-v1`
and its analysis cannot be loaded, so `clar2wasm::compile` fails with
`NoSuchContract`. The retry cannot help: `ensure_wasm_module` caches the
*module* but never registers the *analysis*, where the deploy path does both.

The analysis is present in the imported side store under
`clr-meta::…native-pool-v1::analysis`, so this is a lookup that misses, not
state that is absent. `get_metadata` reaches it through
`get_contract_hash` → `get_block_at_height(commitment.block_height)`, so the
next thing to check is whether a pre-checkpoint deploy height still resolves to
the block its metadata was written under.

Inserting the analysis on demand would paper over it and is not obviously
consensus-safe — metadata writes are state — so the lookup is the thing to fix.

### It was neither: the contract did not exist yet

The diagnostics above narrowed it to "the key is not reachable in the imported
trie", and that reading was wrong — the trie is right and so is the lookup.
`native-pool-v1` was **deployed at height 8,665,687**, eighty-five blocks after
the checkpoint and sixty-four after the deployment that names it. It is absent
because it is genuinely absent.

Mainnet recorded that deployment as an ordinary **failed transaction**:
`abort_by_response`, `(err none)`, fee charged and nonce bumped. nano stopped
the chain on it.

The fault is nano's error classification. stacks-core turns a static check that
is not `rejectable_in_epoch` into a receipt and carries on — `NoSuchContract` is
one of those — and only `Unreachable` and a few others stop a block. nano
wrapped *every* `clar2wasm` compile failure as `Unreachable`, which is the
rejectable kind, so a bad contract and a broken VM were indistinguishable.

Compile failures now carry a mark of their own, and a deployment that fails
analysis becomes the same receipt mainnet wrote.

Two things are worth keeping from the wrong turn: the reachability check over an
imported checkpoint is a real gate and stays in the tree, and the diagnostics
that separate "the trie has no such key" from "the side store has no such value"
are what made the wrong answer cheap to disprove.

## Next divergence: 8,673,864, a tenure start with matching receipts

```
state root mismatch at height 8673864: tenure start true, 2 transactions,
2 receipts, Bitcoin height 960382, tenure height 251422, 2 credits,
liquid supply +1000000000
expected 626fd51b107e40ea4f8843aaebcb4160e01133c36002cfab83dac4890e102c4b
got      35a33905186439176ee34887606b3a0fde2353a0c0880c707abd976663a3ec5e
```

The block holds a tenure change and a coinbase and nothing else. Both succeed,
and mainnet's own receipts for the same two transactions are `(ok true)` and
`(ok true)` — so unlike 8,668,161 this really is a root-only divergence with no
contract call anywhere in it to blame.

That points at what a tenure start does in pure Rust rather than at the VM:
`setup_block`'s tenure height, the matured miner rewards `finish_block` pays,
`process_stx_unlocks`, and the SIP-031 mint. `2 credits, liquid supply
+1000000000` says a coinbase of 1,000 STX was minted and two accounts credited,
which is the first thing to check against the chain.

**Ruled out: the burn-time fix.** It changed `burn_block_time` and `vrf_seed`,
which reach only the header side store and `get-tenure-info?`. The MARFed block
time comes from `setup_block_metadata(block.header.timestamp)` — the Stacks
timestamp, untouched. And several hundred blocks with tenure changes in them
executed cleanly after the fix before this one failed.

## Two things the run exposed about observing a replay

- `/nano/sync_status` **blocks while a catch-up round holds the executor lock**,
  so during a long round it does not answer at all. A measurement taken through
  it reads as a stalled node when the node is executing hard. Reading
  `MAX(height)` from `marf.sqlite` is lock-free and was what actually showed
  progress.
- `max_sync_blocks` bounds a round, and it was 100,000. The durable tip and the
  accounting are written **after** a round returns, so that setting also sets
  how long the node runs with neither persisted. Lowered to 500 for this
  deployment; the right default is worth deciding rather than inheriting.

### SIP-031 ruled out

The mismatch line now reports the mint, and it reads:

```
... 2 credits, liquid supply +1000000000, SIP-031 mint 1140000000
```

So the mint happened, at the post-960,300 rate. nano's schedule matches
stacks-core's `SIP031_EMISSION_INTERVALS_MAINNET` interval for interval
(`stacks-common/src/types/mod.rs:345-373`), including the 960,300 step to
1,140 STX that sits 82 burn blocks before this divergence and looked like the
obvious suspect. It is not the cause.

Worth keeping: reading the *old* line — `2 credits, liquid supply +1000000000` —
I concluded the mint had not happened at all, because 1,000,000,000 is exactly
the coinbase. That was wrong, and the line invited it by omitting the one
quantity that would have settled it.

### What is left

`+1000000000` of liquid supply and 2 credits at a tenure start, with no contract
call in the block. That is the matured reward path: `finish_block`'s matured
miner rewards, `process_stx_unlocks` over `.lockup`, and the tenure height
`setup_block` writes. The next check is whose two accounts nano credited and by
how much, against the chain's own balances at 8,673,863 and 8,673,864 — the
first divergence in ~7,200 blocks that is neither the VM nor a missing header.

### The cause: nano pays a tenure's fees with its coinbase; the chain does not

The miner maturing at 8,673,864 is `SP70B98HWSFY2M7JB6V6P563TR3JSBWW3S43GS8M`,
and its balance across that block moves by **exactly the coinbase**:

```
until_block 8673863   balance 2002701297
until_block 8673864   balance 3002701297     +1,000,000,000
```

nano wants to pay `coinbase 1,000,000,000 + fees 625,846 = 1,000,625,846`, from
its record of tenure 251322 (= 251422 - 100, the maturity). Hence the extra
credit, and hence the root.

Walking the same balance backwards shows the chain paying the two apart:

| range | delta | what |
|---|---|---|
| 8,673,800 → 8,673,840 | **+1,000,000,000** | a coinbase, exactly, no fees |
| 8,673,840 → 8,673,855 | **+15,114** | a fee payment on its own |

`15,114` is precisely nano's fee figure for tenure **251321** — a tenure nano
credits to `SP2N4YMH4XNWTD…`, not to this miner. So the chain both **separates
the fee from the coinbase** and attributes it to a different tenure's miner than
nano does.

That is one off-by-one at a tenure boundary, and it is why this survived ~7,200
blocks: it only shows when consecutive tenures have different miners *and* the
fees are large enough to move a root before the two payments cancel out.

**Not the accounting corruption.** That was the first suspicion, since 251322 is
inside the window the rejected-block bug touched ([[056]]) and 251323 was
repaired by hand. But nano's neighbouring figures (251320 = 1,260, 251321 =
15,114) are exactly what the chain pays, so the recorded fees are right. It is
the attribution that is wrong.

Next: read `finish_block`'s matured-reward split in stacks-core
(`MinerPaymentSchedule`) and check which tenure's fees a coinbase maturity
carries, then fix nano's `TenureAccounting::earnings_at` to match. The two
`credits` nano makes at a tenure start are the coinbase and the fee; the chain
makes them in different blocks.
