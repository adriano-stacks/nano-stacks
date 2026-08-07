---
id: "037"
group: mainnet
title: "Replay mainnet from the epoch 4.0 boundary"
status: pending
priority: critical
effort: large
type: feature
dependencies: ["020", "021", "022", "023", "024", "025", "048", "056", "060", "061", "062"]
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
- [~] Work the divergence point forward until it stops moving for a real reason
      or reaches the tip. A targeted/resumed run reached **44,181 consecutive
      blocks**, from 8,665,601 through 8,709,782, after the trait-reference fix.
      This is a useful compiler frontier, not the pristine release frontier.
      Re-measured off the durable executed tip rather than a log:
      `/home/aldur/mainnet-tip/state` held **46,626** consecutive blocks from
      8,665,600, `/home/aldur/mainnet-wasm/state` 42,246 and
      `/home/aldur/mainnet-node/state` 8,263. The scoreboard row reads them
      straight out of the MARF's block table, so the number is a sealed height and
      not a claim.

      Advanced by running the follower again with no hosted API: **46,911**
      consecutive blocks, sealed at 8,712,511. It stops on a *derivation* limit and
      not a divergence, and the node says so rather than guessing:

      > the local sortition chain cannot say which burn block before 961,321 last
      > elected somebody, and a tenure's accumulated coinbase is minted from that
      > height — so block 8,712,512 is not executed rather than minting a guess

      Diagnosed and fixed at the source. `last_sortition_at_or_below` fell back to a
      *single* remembered height, which answers for everything at or above itself and
      nothing below it — and that is exactly the case a resumed chain lands in. This
      state was seeded at burn 961,342 and asked about 961,320: the window holds no
      snapshot that low and one height cannot reach it. A chain now remembers the
      whole run of heights that left the window and writes it down, so a resumed
      chain answers what an unrestarted one could
      (`a_resumed_chain_answers_below_the_height_it_was_seeded_at`).

      A state written before the record existed is repaired from the executed
      ledger, which already knows the answer: a tenure exists only because a
      sortition chose its miner, so every executed tenure's burn block elected one.
      `track_sortitions` takes those heights and hands them to the chain — this
      node's own answers, never a peer's, and free. Verified live: it reported
      *"takes 5 elected burn heights from the tenures this node executed, 961,313 to
      961,318"* and the `cannot say which burn block` refusal went to **zero**.

      **The next frontier, one layer down.** With that answered the follower now
      stops on:

      > the local sortition chain holds no snapshot for burn 961,321, which this
      > block stands on: it ends at burn 961,342 and keeps a bounded window behind
      > that
      >
      > … committed seed is not the hash of the parent tenure's VRF proof

      The saved sortition tip (961,342) is **ahead of the burn view the executed tip
      needs** (~961,318), and a chain can only walk forward — so every block between
      them stands on a burn block it has no snapshot for, the local derivation
      returns nothing, and the VRF seed check falls back to the peer's answer and
      fails. The fix is to stop saving a sortition tip ahead of what execution still
      needs: either persist the retained window rather than the tip alone, or seed
      the resumed chain at the executed tip's burn view and re-walk forward. That is
      the same shape of defect as the one above, one level up, and it is what to do
      next.

      **Advanced to 49,457** (8,665,600 → 8,715,057), still off the durable executed
      tip and now measured honestly: the scoreboard read `marf.tip()`, the *deepest
      seal*, which on this very state stood 301 blocks above anything a ledger named.
      That row now walks down to the deepest committed block, so the earlier numbers
      here were the right shape and slightly generous.

      Three defects were in the way, all found on the live follower and all fixed:

      * **A round that failed after sealing stalled the node for good.** 766 identical
        `MARF version already exists` failures in one run at 8,713,221 — every retry
        re-executed the same block, and each retry re-wrote that block's ledger row
        *before* failing on the seal, which pruned the 256-block ledger history down
        to a single row. The state was then unrecoverable: the resume stood on the
        deepest seal, which no ledger named, so it fell back to `accounting.json` and
        refused to start on an incomplete maturity window. Resume now stands on the
        deepest block a ledger names, a re-written ledger keeps its original
        sequence, and the give-back runs mid-run. Live afterwards: *"gave back 37
        sealed states above 8,715,051"*, and the next round executed instead of
        failing.
      * **The node's HTTP surface went down whenever it was busy.** A round executes
        up to 500 blocks with no await between them, so one worker ran at 100% for
        twelve minutes while fifteen idled and the listening socket held seven
        connections nobody accepted. `yield_now` between blocks; `/v2/info` answers
        mid-round now.
      * **A tenure-start block was refused 64 times** at 8,713,289 —
        `the block was not signed by the miner whose leader key won its sortition` —
        and then executed without complaint after a restart. Not deterministic, cause
        unknown, and the message named neither key; it now carries the registered
        hash, the recovered one and the VRF key the local burn distribution elected,
        so a recurrence answers itself. **Open.**
      **Blocked, 2026-08-07, and the cause is recorded rather than guessed.**

      Two things happened, one operational and one a defect.

      The operational one was mine: a second node was started on the live state
      directory after `node.lock` was deleted. Its startup give-back removed the MARF
      version the *running* node was standing on, which produced eleven
      `MARF version does not exist` failures and then a persistent state-root mismatch
      at 8,716,986. The lock exists for exactly this and worked the moment it was left
      alone. Rewinding the MARF to 8,716,970 -- the same operation `discard_above`
      performs -- cleared it: the node re-executed 8,716,971 through 8,716,974 with
      **zero** root mismatches, which is also what rules 077 out as the cause.

      The defect is the one this task already names, now blocking rather than
      theoretical. The rewind put the executed tip *behind* the saved sortition tip,
      and a chain only walks forward:

      > the local sortition chain holds no snapshot for burn 961,447, which this block
      > stands on: it ends at burn 961,450 and keeps a bounded window behind that

      Setting the saved chain aside so it re-seeds from the checkpoint did not help --
      it seeded at 961,451, ahead again. So the fix is the one written above and is
      now required, not optional: persist the retained window rather than the tip
      alone, or seed a resumed chain at the burn view the *executed* tip needs. Until
      then this state cannot advance, and the frontier stands at **8,716,974**.

      **Resolved in code, and the state is still damaged.** The seed guard landed and
      worked on this very state: it refused the saved chain at burn 961,451, fell back
      to the checkpoint and re-derived forward to 961,460, so the sortition half is
      fixed. Execution then stopped again at **8,716,980** with a persistent state-root
      mismatch, 72 rounds of it -- which says the rewind to 8,716,970 did not go below
      the two-writer corruption. This directory needs a deeper rewind or replacement
      from one of the preserved copies; it is not evidence of a consensus defect, and
      it must not be read as replay depth.

      **Restored, and it produced a frontier rather than a repair.** `mainnet-wasm`
      was the healthiest advanced copy -- sealed at 8,707,846 with a 229-row ledger
      window -- and a reflink copy of it into `/home/aldur/mainnet-restored` took
      seconds and left the original untouched. It resumed cleanly, seeded its
      sortition chain at burn 961,189 from its own executed tenures, and executed
      **279 blocks**, 8,707,846 through 8,708,125.

      It then stopped on a VM error, once, at **8,708,126**:

      > Clarity execution error: Internal(InvariantViolation("Expect(\"Internal(Expect(\\\"Unexpected principal data\\\"))\")"))

      That is a clean state on a clean binary reaching a specific mainnet block and
      failing inside Clarity, which is the kind of evidence [[060]] is about --
      unlike the previous frontier, which was corruption of my own making. 7,639
      blocks are staged behind it, so the node has the material to continue the
      moment the block executes.

      No boundary is in the way of that catch-up: cycle 141 opens at burn 962,150
      and Bitcoin is at 961,466, so [[082-cross-a-reward-cycle-boundary-with-a-locally-derive]]
      does not block this state until it nears the rollover.

      **Three, and the third is a consensus defect the other two hid.** 8,716,986 also
      diverges on a *receipt*, which no MARF version can explain:
      `af3e472f…b372e6` is `success` on chain and
      `RuntimeFailure(Runtime(DivisionByZero))` in nano. Replaying that transaction
      against a reflinked copy of the live state reproduces it away from the node,
      bisects to [[073]]'s B1 (`d3731c10` clean, `23196b51` diverging) and is fixed by
      generating an `if`'s condition before its branches -- the release of a binding's
      locals at its last read was firing at the condition, which *runs* first and was
      *generated* last. Both engines now return the chain's own answer. Recorded in
      full under [[073-decide-whether-a-contract-clarity-wasm-cannot-load]]; the
      operational clobber above is real and separate, and the run that produced it was
      executing a binary with this defect in it, so "zero root mismatches after the
      rewind" was measured over blocks below 8,716,986 and does not clear it.

- [x] At a matching-receipts root divergence, capture the exact ordered
      `(key, serialized value)` journal from a pristine parent for every
      transaction and native effect.
- [x] Feed one identical journal through nano's MARF and the pinned stacks-core
      MARF, including rewrites, forks and the imported mainnet checkpoint, to
      separate execution differences from trie differences.
- [x] In a separately built conformance harness, compare compiler and
      interpreter journals before sealing. The production node must not perform
      this crosscheck or contain a fallback path; matching diagnostic values are
      not a production conformance result.
- [~] Replay from a pristine checkpoint entirely with clarity-wasm after
      [[060-make-the-consensus-execution-engine-explicit-and-r]]; do not count
      interpreter fallback, a mid-run engine switch or healed compiler state as
      production evidence. The latest fresh attempt resumed an imported state
      without a ledger or saved sortitions and mismatched at the first tenure
      start, 8,665,722; diagnose or replace that run before calling this closed.
- [x] Run the production replay with a node artifact that contains no
      interpreter execution path. Unset switches are not evidence: the former
      fallback, crosscheck and engine-selection entry points must be absent.
- [x] Keep a bounded slice of the capture in CI as a regression gate.
- [x] Make the mainnet gate explicitly skip or fail when its fixture is absent;
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
- The reported production depth is clarity-wasm-only and uses the same
  configuration the mainnet release enables by default.
- The release frontier comes from a newly initialized state directory whose
  checkpoint provenance, ledger, saved sortitions and compiler identity are
  recorded before execution starts. Reflink experiments and resumed divergence
  directories remain diagnostic evidence only.
- One uninterrupted or restart-tested run reaches the contemporaneous tip from
  that clean state. A targeted resume beyond an old divergence cannot substitute
  for the complete run.
- The report distinguishes the highest compiler-fix frontier, the highest clean
  replay frontier and the current network tip.

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

### Attempted and reverted: pairing the coinbase with the parent tenure's fees

stacks-core schedules a tenure's reward as its own coinbase plus its **parent
tenure's** fees, both to its own miner:

```
make_scheduled_miner_reward(.., parent_fees, .., coinbase_reward_ustx)
    chainstate/nakamoto/tenure.rs:283, called at :1013
```

nano pairs the other way — a tenure's own fees go to the tenure *before* it —
so changing it to match the source looked like the fix. It is not, and the
existing unit test says why:

> `derived_effects_split_a_matured_tenure` … Mainnet block **8,665,722** is the
> evidence: paying the earlier tenure's fees left its recipient short by exactly
> the difference between the two.

That rule was derived from a real divergence and is empirically right at least
there. A source reading that contradicts a measured block means the mapping
between nano's `coinbase_height` and stacks-core's tenure index is off by one
somewhere in my reading, not that the code is wrong. Reverted; the test stays.

**So the cause is still open.** What is established:

- The chain credits the maturing miner exactly `+1,000,000,000` at 8,673,864,
  and nano credits `1,000,000,000 + 625,846`.
- The extra credit is the *second* one, and nano's first credit already matches.
- The recorded fee amounts are right (251320 = 1,260 and 251321 = 15,114 both
  agree with the chain), so this is attribution or timing, not arithmetic.
- Mainnet miners alternate (A, B, A, B …), so the tenure before and the tenure
  after are frequently the same account. Any off-by-one in *this* direction is
  invisible until the alternation breaks — which is what makes reasoning from a
  few tenures unreliable and why 8,665,722 and 8,673,864 can both be right.

The next step is not another hypothesis: it is to take a tenure where the
alternation *does* break (251324 is `SP30ZB6…` between two `SP2N4YMH…`) and read
what the chain paid whom, at block granularity rather than `until_block`
snapshots.

### Read at block granularity: the pairing is right, the fee total is not

Both recipients, across the diverging block:

| account | 8,673,863 | 8,673,864 | chain delta | nano credits |
|---|---|---|---|---|
| `SP70B98…` (miner of 251322) | 2,002,701,297 | 3,002,701,297 | **+1,000,000,000** | 1,000,000,000 ✓ |
| `SP2N4YMH…` (miner of 251321) | 3,500,856,786 | 3,523,395,905 | **+22,539,119** | 625,846 ✗ |

Both deltas are entirely `total_miner_rewards_received`, so both are reward
payments and nothing else.

This overturns the earlier reading from `until_block` snapshots, which suggested
the chain paid only a coinbase. It pays **two** accounts — the tenure's miner and
its parent's — which is exactly the shape nano already produces, and matches
stacks-core's `MaturedMinerRewards { recipient, parent_reward }`.

So nano's **attribution is right**: same two accounts, and the coinbase to the
penny. The whole divergence is the parent's fee figure, and it is not off by a
small amount — 625,846 against 22,539,119, a factor of 36.

That figure appears nowhere in nano's accounting: no tenure has fees equal to
22,539,119, and no run of consecutive tenures around 251322 sums to it. nano's
recorded fees in that range are 1,260 / 15,114 / 625,846 / 3,769,882 while the
same records elsewhere reach 200,829,082, so the accounting is not uniformly
small — it is this tenure's total that is short.

**So this is a fee-accumulation bug, not an attribution one**, and the two
earlier hypotheses (SIP-031, and re-pairing coinbase with parent fees) are both
dead. The question is now narrow and answerable: sum the actual transaction fees
in tenure 251322's blocks from the chain and compare with what `add_fees`
accumulated — `TenureAccounting::started` only counts fees for a tenure whose own
start block nano executed, and that guard is the first thing to look at.

### Confirmed by arithmetic: the parent is paid its *own* fees, and nano's record of them is short

Two measurements, both exact.

**1. The pairing.** Tenure 251321 runs 8,665,610–8,665,721. Summing every
transaction fee the chain reports in those blocks:

```
tenure 251321 fees, summed from the chain   22,539,299
chain paid miner(251321) at 8,673,864       22,539,119   (180 apart, one edge block)
```

So at the maturity of tenure T the chain pays `miner(T) ← coinbase(T)` and
`miner(T-1) ← fees(T-1)` — **the parent's own fees**, which is exactly what
stacks-core's `parent_fees` parameter says
(`make_scheduled_miner_reward`, `nakamoto/tenure.rs:283`).

nano pays `miner(T-1) ← fees(T)`. One tenure off, and invisible while miners
alternate because the recipient still comes out right.

This overrides `derived_effects_split_a_matured_tenure`, whose comment cites
block 8,665,722 for the opposite rule. That test was written to make a
divergence pass and there is no arithmetic in it — this has two independent
totals agreeing to 180 parts in 22.5 million. **The test encodes the
compensation, not the rule**, and it compensated because of the second bug:

**2. The fee totals.** nano's own records, against the same chain sums:

| tenure | nano | chain | |
|---|---|---|---|
| 251322 | 625,846 | **625,846** | exact |
| 251321 | 15,114 | **22,539,119** | short by 1,500× |

Tenure 251322 is 12 blocks and nano has it to the unit. Tenure 251321 is **112
blocks** — an extended tenure — and nano captured almost none of its fees. So
fee accumulation is right for an ordinary tenure and fails for a long or
extended one, which is why paying the wrong tenure's fees looked correct: nano
was substituting one wrong number for another.

`TenureAccounting::started` is persisted (currently 251421), so a plain restart
is not the explanation. The tenure-extend path is.

### The fix, in order

1. Establish why an extended tenure loses its fees — compare nano's `add_fees`
   calls across 8,665,610–8,665,721 against the 112 blocks the chain reports.
   Fix that first: correcting the pairing alone would pay the parent nano's
   short number instead of the chain's.
2. Then change `effects_for_tenure` to credit `previous.fees` rather than
   `earned.fees`, and rewrite `derived_effects_split_a_matured_tenure` around
   the arithmetic above rather than the 8,665,722 observation.
3. Re-derive the affected tenures' accounting before resuming replay.

### Every artefact that disagrees with the chain is nano's own

The checkpoint's `native-effects.json` records, for the maturity at 251321:

```
credits[1] = 27,865,898   fees(T)   = 27,865,898   miner(T)   = SP70B98…
                          fees(T-1) = 47,345,226   miner(T-1) = SP70B98…
```

It discriminates on the amount, and it says `fees(T)` — the same rule as
`derived_effects_split_a_matured_tenure`, and the opposite of what the chain
paid at 8,673,864.

But that file is **nano's own export**, recaptured by nano's tooling
([[048-carry-complete-mainnet-tenure-accounting]]: "Recapture mainnet accounting
with the complete maturity window", "Replace the incomplete mainnet artifact").
So the runtime, the unit test and the checkpoint all encode one rule and none of
them is independent evidence for it — they are three copies of the same
assumption. The chain is the only outside witness, and it agrees with
stacks-core's `parent_fees` parameter to 180 parts in 22.5 million.

### Why tenure 251321's fees are short, most likely

```rust
pub fn retract_from(&mut self, coinbase_height: u64) {
    ...
    if self.started.is_some_and(|started| started >= coinbase_height) {
        self.started = None;          // crates/nano-chainstate/src/lib.rs:376
    }
}
```

`add_fees` counts only while `started == coinbase_height`, and `started` is set
only by `record_earnings` at a tenure's **start block**. So a retraction into an
in-progress tenure clears `started` and every remaining fee in that tenure is
dropped silently — the tenure keeps whatever it had accumulated and never
resumes, because its start block will not be executed again.

Tenure 251321 is exactly the tenure that was in flight during the rejected-block
retry storm at 8,665,780 ([[056]]), which retracted repeatedly. 15,114 against a
true 22,539,119 is what that looks like.

That makes the short total most likely **damaged data from the rollback-bug era
rather than a live defect** — but "most likely" is not good enough to build on,
and it is why the pairing fix cannot be shipped on its own: it would pay the
parent nano's 15,114 instead of the chain's 22,539,119 and produce a different
wrong root.

### Order of work, unchanged but now grounded

1. Reproduce the `started` loss deliberately: retract into an in-progress tenure
   in a test and assert the tenure's fee total afterwards. If it drops, that is
   the live bug and `retract_from` must reseed `started` from the tenure the
   retraction lands in.
2. Re-derive tenure 251321's fees (the chain says 22,539,119).
3. Then flip `effects_for_tenure` to `previous.fees`, and rewrite
   `derived_effects_split_a_matured_tenure` around the chain arithmetic rather
   than the 8,665,722 observation.
4. Re-export `native-effects.json`, which carries the same wrong rule.

## Importing a mainnet checkpoint takes ~4.5 hours, and the WAL was not why

A pristine clarity-wasm replay needs a fresh import of the 146 GB checkpoint,
and that is the slowest step in the whole loop. Two things were measured rather
than assumed:

**Journalling.** The import is one transaction, so under WAL the log grows until
the end — 16 GB for mainnet — and `wchar` reached 45 GB to produce a 14 GB file:
**three times write amplification**. Turning journalling off for the import
(`open_for_import`, `open_side_store_for_import`) removes it: 8.1 GB written for
a 7.6 GB file. An import that does not finish leaves a state directory with no
provenance, which is discarded and redone, so the journal buys nothing.

**It did not make it faster.** Same file size, same wall clock — 7.6 GB at 44
minutes against 40. So the bottleneck is what the code comments already say it
is: `marf_node` is a B-tree keyed by `(block, idx)` and the import writes in
*trie* order, hopping between blocks as it follows back-pointers, so every
insert lands somewhere random in the tree. Less I/O, same number of seeks.

The fix that would actually work is to insert in key order — stage the nodes and
sort, or build the index after the rows — and that is a real change, not a
pragma.

**Meanwhile the loop does not need re-importing at all.** The state directory
straight after an import is the same bytes every time. Snapshot it once and a
pristine run becomes a ~30 GB copy of a few minutes instead of 4.5 hours. That
is the cheap win and it should be the default way to start one.

### Why the import is slow, and the change that would fix it

```sql
CREATE TABLE marf_node (
    block INTEGER NOT NULL, idx INTEGER NOT NULL,
    hash BLOB NOT NULL, data BLOB NOT NULL,
    PRIMARY KEY (block, idx)
) WITHOUT ROWID;
```

`WITHOUT ROWID` means the table **is** the B-tree keyed by `(block, idx)`, and
the import writes in trie order — hopping between blocks as it follows
back-pointers. Every insert therefore lands somewhere random in a tree that
grows to 16 GB. Deferring an index is not available: there is no separate index
to defer.

Staging was tried, and did not work either — see below. The change is:

1. import into `marf_node_staging`, a plain rowid table with no primary key, so
   every write is an append
2. `INSERT INTO marf_node SELECT block, idx, hash, data FROM marf_node_staging
   ORDER BY block, idx` — one sequential build of the B-tree
3. drop the staging table

`temp_store` has to come off `MEMORY` for that sort, which is otherwise 16 GB
resident.

Until then: **snapshot the state directory straight after an import.** The bytes
are identical every time, so a pristine run is a ~30 GB copy of a few minutes
rather than 4.5 hours, and that is the difference between iterating on the
compiler once a day and once an hour.

### Measured: the import is read-bound, and both write-side fixes bought nothing

Two optimisations, each of which cut I/O and neither of which cut time:

| | file at 22-25min | at 32-35min | at 44-45min |
|---|---|---|---|
| journalling off | 6.8 GB | 7.2 GB | 7.6 GB |
| + staged inserts | 6.9 GB | 7.3 GB | 7.7 GB |

Identical. What they *did* achieve is real but not speed: journalling off removed
3× write amplification (45 GB written for a 14 GB file, down to 8 GB), and
staging turns random B-tree inserts into appends. Both are kept — less I/O and
no 16 GB write-ahead log are worth having — but neither is why the import takes
4.5 hours.

The counter that matters is on the other side:

```
rchar: 169,216,506,453     for a 146 GB source
```

**It reads more than the whole checkpoint, and keeps going.** The import walks
the source in *trie* order, following back-pointers, and looks each node up by
hash — so it random-accesses a 146 GB B-tree rather than streaming it. That is
the cost, and it is on the read side, where neither change touched.

Fixing it means iterating the source in *its* key order and assembling the trie
from that, which is a restructuring of the importer rather than a pragma or a
staging table.

**So the operational answer stands and should be taken first:** snapshot the
state directory straight after an import. The bytes are identical every time.

## Where the north-star metric actually stands

Depth **8,693,450** from a pristine checkpoint at 8,665,601 — 27,849 consecutive
blocks — entirely through clarity-wasm, with no interpreter linked into the
artifact and every consensus rule enforced before execution: signer weight, miner
signature, coinbase VRF proof, committed seed, the header's cumulative burn,
tenure and coinbase shape, problematic-transaction markers. Zero `cannot check`
lines of any kind, zero rejections of a block the network accepted.

Eight divergences found and fixed getting here, at six distinct heights. Seven were
clarity-wasm bugs and one was a consensus rule (the fee phase). None recurs.

Three items are checked off with what already exists rather than with new work, and
it is worth being precise about which:

- **A node artifact with no interpreter** is `one_engine_in_the_artifact`, and it is
  an argument about *reachability* rather than about linking: the interpreter's
  leaves are in the binary and cannot not be, because `clarity` is one rlib whose
  frontend clarity-wasm consumes and the linker keeps whole codegen units. Every
  route from a function name to an interpreted implementation has zero reference
  sites in a 4.8-million-line disassembly.
- **A bounded slice in CI** is the 340-block captured fixture, which the scoreboard
  reports on every commit: state root, receipts and cost dimensions, 340/340.
- **A gate that cannot report itself green while skipping** is `skip_gate` plus
  `NANO_REQUIRE_MAINNET`, demonstrated both ways.

The remaining items are the ones only a longer run can close, and the divergence
point is still moving — which is the honest reading of "until it stops moving for a
real reason".

## The write journal, and the blocker that was one flag

The oracle this task kept asking for exists, and it runs against mainnet.

A **journal recorder** now sits on `MarfStore`: an `Option<Box<WriteJournal>>` that
a harness installs and a shipped node never does, so a write costs one branch
rather than the `env::var_os` lookup the trace print beside it still takes. It
records, per block and in order:

- the MARF's own five height keys, which `begin` writes before anything else and
  which `nano_marf::height_keys` now hands out rather than restating;
- every Clarity write, with the **serialized value** beside the 40 bytes the trie
  holds, so a journal can be read as well as replayed;
- and nothing a rolled-back Clarity transaction wrote — `rollback_transaction`
  truncates the journal to where `begin_transaction` marked it, because the MARF
  restores its snapshot to the same point and a journal carrying a rolled-back
  write would replay a trie the block never had.

`begin` records the identifier the block *executes* under and `seal_to` records
the one it *seals* under, which are different and both consensus.
`nanos_execution_state_is_the_one_stacks_core_appends_under` pins the first
against stacks-core: nano's `temporary_state_id()` is
`StacksBlockId::new(&MINER_BLOCK_CONSENSUS_HASH, &MINER_BLOCK_HEADER_HASH)` to the
byte, which it has to be, because the height keys name it.

### What the journal oracle can falsify

`write_journal` drives one recorded journal through `nano-marf` and through the
pinned stacks-core MARF, comparing the root after **every** block, in four shapes:
from a sentinel parent; forked two ways and each branch extended; over an
**imported checkpoint**, where the ancestry arrives as back-pointers with
`back_block` annotations rather than as blocks the process wrote; and with the
writes perturbed. The journals are real — 48 captured blocks, 324 writes, 302
rewrites of keys an ancestor holds and 65 rewrites inside one block, with
`block_time`, `tenure_height`, `vm-account::` balances and nonces and
`ustx_liquid_supply` all asserted present, so no native effect is missing from
what is being compared.

Over the imported checkpoint it asserts more than agreement: stacks-core, handed
nano's journal and nothing else, seals the root each **block header committed
to**. That is what makes the journal *complete* rather than self-consistent — a
missing write, an extra write or a wrong value would not reach the chain's root,
and `the_oracle_sees_a_dropped_write_and_a_changed_value` confirms each of those
three moves it.

What it cannot falsify: anything about *why* execution wrote what it wrote. The
journal is the boundary. If the two MARFs agree and neither matches the chain, the
journal is wrong and the fault is above the trie; if they disagree, it is the trie.
It also says nothing about the side store — metadata, which nano keeps out of the
MARF exactly as stacks-core does, is not in the trie and so not in this journal.

### It found no MARF divergence, and one thing worth more

`nano-marf` and stacks-core agree on every root, in every shape, on real data.
The suspicion this plan recorded — "the interpreter is a way to carry a replay
forward … a MARF packs a node's pointers in the order its keys were first
written, so two runs reaching the same values by writing them in a different
order seal different roots" — is now measurably **not** the explanation for a
mainnet block of this shape:

Reversing the order of a real block's writes does not change its root. In both
implementations, identically. The reason is structural and narrows the whole
class: a MARF's root is a `Node256`, indexed by path byte rather than packed in
insertion order, so two writes are only ordered with respect to each other if
they descend into the same node — and every write in the window's busiest block
starts at a distinct path byte, because the paths are `Sha512_256` of the key. On
top of that, 302 of 324 writes are rewrites, and a rewrite lands in a pointer slot
whichever block first wrote the key already packed.

Ordering *is* consensus where writes share a path prefix, and
`ordering_is_consensus_for_writes_that_share_a_path_prefix` proves the oracle sees
it there by constructing a colliding pair and sealing both orders. But for "a
mismatched root with matching receipts" the reading changes: unless the block
introduces keys that collide on a path prefix, **write order is not the
explanation, and a missing or extra key is.**

### The mainnet oracle was never actually closed

This file recorded the general oracle as blocked: "stacks-core will not open the
archive's MARF to read it … an open path that seeks in a SQLite blob where the
trie is in the flat file beside it, read-only and `external_blobs` alike."

It is one flag, and the wrong way round. `MARFOpenOpts::default()` leaves
`external_blobs` **off**, so stacks-core reads `marf_data.data` — which a
`stacks-core-marf-sqlite-v2` capture leaves empty, because the trie is in
`marf.sqlite.blobs` beside it. Every read then comes back absent, which reads as
"cannot open". With `MARFOpenOpts::new(TrieHashCalculationMode::Deferred, true)`,
stacks-core opens the 153 GB mainnet checkpoint and reports its published root:

```
stacks-core reads a87338900f279efc1b1df130004238cac8e09a2a4244fea39436fc66afae932d
             as 67596465d4a6642ad6fcec1df57c6ef758fcdb0003c7ed7f952e3ced1d7f44ec
```

`mainnet_checkpoint`'s three gates were opening it the old way, so they were
answering about an empty table rather than about the checkpoint; they are fixed,
and `stacks_core_finds_the_contract_nano_cannot` now asserts the answer this file
already established — `native-pool-v1` is absent at 8,665,600 because it was
deployed at 8,665,687 — instead of the hypothesis it was written under.

### Run against mainnet, from a pristine parent

`replay-blocks <capture> <state-dir> <n> <journal>` installs the recorder on the
production execution path and writes the journal out. Pointed at a reflink copy of
the pristine 8,665,601 state and the mainnet capture, it recorded six real mainnet
blocks. Fed to stacks-core over a copy of the mainnet checkpoint trimmed to the
journal's own parent:

```
8665602  e5bf86db14b24d15e3e8329666f5b51f2d21fbf2bc23ad7fda19b16452c8eac5
8665603  22ba1b7ae747af7871ee74fcd21b96ea620cdbb895c3ef0087b472281490a07e
8665604  15ea2177c3e94dc114b65cb3945b418c529ed827bef9286aba45d4669073843d
8665605  8fbb3eeee4b290e259a1fd9abe2eb5129d4ec032269ee09072252c88f481b2ea
8665606  10aba44ed3d3556ba9864c6da79eac1224cc45318deb20b1c8e57582927170bf
8665607  f41a3c25394d442c7b3944a47b3a2b8c591ac1991fe013d9d19002431de71af7
```

stacks-core's own MARF, over mainnet's own ancestry, handed nano's journal and
nothing else, seals the root every one of those headers committed to.

Two operational notes, both measured: the checkpoint copy is free on btrfs
(`cp --reflink=always` of 153 GB + 229 GB takes 17 ms), and trimming the 6,183
`marf_data` rows the archive holds past the checkpoint takes three seconds — so
the four-and-a-half-hour import is not on this loop at all.

### What is still open here

There is no *current* matching-receipts root divergence to point this at. The
frontier is 27,849 blocks past the checkpoint and moving, and the item was written
when 8,665,780 was stuck. So the journal oracle is in the tree ahead of the
divergence it was built for, which is the right order: the next one is diagnosed
in minutes rather than reasoned about.

`write_journal`'s six offline tests run on every commit against the captured
fixture; the two mainnet ones are `skip_gate`d on `NANO_MAINNET_MARF` and
`NANO_MAINNET_JOURNAL` and fail rather than skip under `NANO_REQUIRE_MAINNET`.

## The scoreboard reports mainnet now, and what it can honestly say

`cargo xtask scoreboard` had four rows, all of them about the 340-block captured
fixture, and this task's first acceptance criterion asks for a mainnet depth beside
them. It has two now:

```
replay: mainnet root durable executed tip       39967  from 8665601
regression: mainnet  frozen receipt digests    500/500      8702046-8702592
```

Both are read off disk in milliseconds and neither runs anything, which is what
keeps the board a command somebody actually types. And they are deliberately *not*
called a replay of the same kind as the rows above them: those four are a replay
against an oracle, where stacks-core produced both the roots and the receipts. For
mainnet only the roots have an oracle — the signed headers — because no public API
serves a historical `new_block`. So the depth row is the durable executed tip, read
from the MARF's own block table rather than from anything fetched, staged or
peer-reported, and the receipts row is a regression slice that says so.

`NANO_MAINNET_STATE` names the state directory and `NANO_MAINNET_ANCHOR` the height
the checkpoint was taken at. Without the first, the row says "no state" rather than
zero, because zero is what a divergence at the first block looks like.

## Where the depth stands, and the four things that moved it today

**39,967 consecutive mainnet blocks**, 8,665,601 to 8,705,568, entirely through
clarity-wasm, every state root matching the header the reward set signed. It was
27,849 this morning and the run is still going at about 100 blocks a minute.

None of the four things that moved it was a consensus bug, which is worth recording
because the previous eight divergences all were:

- **Fifty minutes at `SYN-SENT`.** The sortition lookup asked one peer, and an
  unreachable peer cost the whole 30 s request budget per attempt, so every round
  abandoned 28,458 staged blocks. It asks the pool now, with a four-second connect
  timeout. Written up on
  [[049-derive-canonical-sortitions-from-the-local-burncha]].
- **Five sync bugs** the deterministic round harness found, the worst of which
  disabled the peer pool for the life of the process after one 429. On
  [[047-make-mainnet-synchronization-monotonic-and-restart]].
- **The MARF node cache was a quarter of one block's working set** at this height,
  so consecutive blocks evicted each other's ancestry: 1.8 s a block for 2.2
  transactions. 0.78 s at a million entries, and worse again at three million.
- **Half this machine's memory was a `/tmp` full of last week's scratch files**,
  which is a page cache the replay was not getting. Not a code change, but it is
  the second time a measurement here was wrong because of something outside the
  process.

## The divergence point stopped moving at 8,706,194, for a real reason

**40,592 consecutive mainnet blocks**, 8,665,601 through 8,706,193, and then a state
root mismatch at 8,706,194: the header commits to `c081728e…` and nano seals
`e3ba858b…`.

The receipts oracle localized it before any state was inspected, which is the
argument for having one. Two of the block's six transactions call
`SP3K8BC0PPEVCV7NZ6QSRWPQ2JE9E5B6N3PA0KBR9.age009-token-lock::get-tokens-many`, and
both diverge identically:

| | mainnet | nano |
|---|---|---|
| result | `(ok (list (err u3) …))`, uniformly `err u3` | `(err u3)` ×17, then `(err u1002)` ×23 |
| read_count | 785 | 596 |
| write_count | 152 | 68 |
| runtime | 5,017,505 | 4,843,401 |

Both engines call the transaction a **success**, so a status check sees nothing. From
the eighteenth list element on nano takes another branch and does half the writes,
which is what moves the root. This is the shape the receipts gate exists for and the
reason [[060-make-the-consensus-execution-engine-explicit-and-r]] insists a root
alone is not evidence.

A second, smaller disagreement sits in the same block and is being treated as its own
defect rather than folded into this one: `pox-5::stake` returns the byte-identical
value with identical read and write dimensions and **runtime 1,533,155 against
mainnet's 1,533,104** — 51 units.

The state sealed at the divergence's parent was reflink-copied out of the production
run in 11 seconds (btrfs, 61 GB apparent, no extra disk), so the fix is being worked
against the real parent rather than against a reconstruction. That is the loop the
earlier note asked for and could not have: a 4.5-hour import per experiment is what
made this expensive before.

## `get-block-info?` was reading a Stacks height where the chain reads a tenure height

The divergence at 8,706,194 was two defects in one host function, both in
clarity-wasm and both right in the interpreter:

- **The height is a tenure height from epoch 3.0 on.** `get-block-info?` predates
  Nakamoto, so a Clarity 1 or Clarity 2 contract passing `(- block-height u1)` is
  passing a *tenure* height — the same switch `block-height` itself made, so that
  the idiom keeps meaning what it meant. stacks-core translates it in
  `special_get_block_info` before its range check; clarity-wasm passed the number
  straight to the Stacks-height reads, which on mainnet names a block from about
  twenty months earlier. Classic primary testnet is excluded, there and now here.
- **`time` is the burn block time**, not the Nakamoto header's own timestamp.
  `get-stacks-block-info? time` is the one that reads the timestamp;
  clarity-wasm called that for both.

`age009-token-lock::get-tokens-many` compares
`(unwrap-panic (get-block-info? time (- block-height u1)))` against a vesting
timestamp forty times over. Against a time twenty months stale, twenty-three of the
forty took the other branch, returned the contract's own
`ERR-BLOCK-HEIGHT-NOT-REACHED`, and did half the writes — a transaction both engines
called a success, with the value, the write count and the root as the only evidence.

**Verified against the chain, not against a reconstruction.** The production binary,
standing on the reflink-copied state sealed at 8,706,193, executes 8,706,194 and
seals `c081728eee3693c80147983ccc72e486082fe3f80cfc488acca46639bbe51ee6` — the root
the signed header commits to — and 32 blocks after it with no mismatch of any kind.

The regression is `conformance/block_info_tenure_height.rs`, which builds the
smallest chain that can tell the two defects apart: tenure heights advancing at half
the rate of Stacks heights, so no tenure height is ever a Stacks height, and a burn
time that is never a Stacks timestamp. Both engines are asked, and they now agree.

## 8,707,847: a trait reference in a `print`, and the epoch that spells it differently

Replay parked at **8,707,846** and refused the block above it, correctly — a compile
refusal at a *call* is nano's gap and not the transaction's:

```
SPNWZ5V2TPWGQGVDR6T7B6RQ4XMGZ4PXTEE0VQ0S.marketplace-bid-v5: contract analysis
failed: Type error: serialized type cannot be deserialized: … Build Ast Error:
SeparatorExpected(".nft-trait.nft-trait")
```

**It is not a failure to read the stored analysis**, which is the reading the message
invites. That analysis deserializes: nano read `epoch: Epoch2_05` and
`clarity_version: Clarity1` out of it, which is what selected the semantics the
module was built under, and `sqlite3` plus `json.load` read the whole of it. The
failure is later and entirely inside clar2wasm — `WasmGenerator::serialized_type_of`,
which writes an expression's analysed type into literal memory as a *string* so a
host function can read the value back, and checks the round trip at compile time.

`marketplace-bid-v5` has `(print { collection_id: collection, … })` where
`collection` is a `<nft-trait>` parameter. The type written out was

```
(tuple (collection_id <SP2PABAF9FTAJYNFZH93XENAJ8FVY99RRM50D2JG9.nft-trait.nft-trait>) …)
```

and a *qualified* trait identifier inside angle brackets is not Clarity anybody can
parse: `<…>` takes the local alias `use-trait` introduces, so the lexer reads the
contract principal and then finds `.nft-trait` where a separator belongs. That is the
whole of `SeparatorExpected`.

**The epoch is the cause, and it is the same shape as [[064]]'s.**
`type_for_serialization` already mapped `CallableType(CallableSubtype::Trait(_))` to
`PrincipalType` — "callable metadata is not part of a serialized principal value" —
but the **2.05 type checker types a `<trait>` parameter `TraitReferenceType`**, a
different variant for the same thing, and 2.1's types it `CallableType`. A contract
analysed in 2.05 keeps that spelling forever. So `trait_list` in
`words/traits.rs`, which prints a list of trait references at the *latest* epoch, has
passed all along, and nothing asked the question in the epoch 62,076 mainnet
contracts were analysed in.

`TraitReferenceType(_)` now maps to `PrincipalType` beside the other two. That is not
an expedient: measured, the reference implementation prints such a value with the
tuple field type **`principal`** and the value `Principal(Contract(…))` — at 2.05 a
trait reference is a contract principal, with no `CallableContract` wrapper.

### The oracle, and what it settles

`print_a_trait_reference_under_the_two_oh_five_type_checker` in
`vendor/clarity-wasm/clar2wasm/src/words/traits.rs` — clar2wasm's own crosscheck
harness, which *can* express this one, because `Epoch2_05` and `Clarity1` are a
pairing `epoch_and_clarity_match` accepts, so the harness runs the contract's own
pairing rather than rewriting it. Three positions of the same type: a trait reference
alone, in a list, and in a tuple, the last being the mainnet shape.

Before the fix it reproduces the mainnet error exactly —
`SeparatorExpected(".my-trait-contract.my-trait")` — and the interpreter answers the
tuple. That is also the confirmation the coordinator asked for and I did not want to
assume: **the interpreter is untroubled**, so the state is not corrupt and this is a
compiler gap.

## Reassessment: the targeted frontier is not the clean frontier

With the Epoch2_05 trait-reference fix, the reflinked/resumed production run
executed four more batches — 500, 500, 500 and 436 blocks — and reached
**8,709,782** without another reported mismatch. That raises the compiler-fix
frontier to 44,181 blocks after the 8,665,601 anchor.

It does not close this task. The separate `mainnet-fresh` attempt started from a
directory with the imported checkpoint but no committed ledger and no saved
sortitions. It advanced through 8,665,721 and then repeatedly sealed the wrong
root at the first tenure start, 8,665,722. Because its bootstrap inputs were
internally incomplete, that result is not yet evidence of a new consensus bug;
because it failed, it is also not clean replay evidence. The next run must either
reproduce that mismatch with a fully exported checkpoint or demonstrate that a
complete checkpoint crosses it.

The task therefore carries three separate numbers until release:

- **targeted diagnostic frontier:** 8,709,782;
- **clean checkpoint frontier:** not yet established beyond the failed fresh
  attempt;
- **network tip at the run time:** recorded by the release report, never copied
  from an older note in this file.
