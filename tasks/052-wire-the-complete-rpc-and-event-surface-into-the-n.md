---
id: "052"
title: "Wire the complete RPC and event surface into the node"
status: in-progress
priority: high
effort: large
type: feature
group: mainnet
dependencies: ["029", "046", "047", "050"]
tags: ["mainnet", "rpc", "events"]
created_at: 2026-08-02
---

# Wire the complete RPC and event surface into the node

## Objective

[[029-serve-the-rest-of-the-node-rpc]] implemented routes and event builders,
but runtime constructs `RpcState` with only `with_chain`. There is no mempool,
block sink, proposal token or published reward set, and followed blocks do not
dispatch `new_block`. The available endpoints are consequently unavailable,
empty, or backed by different tips.

Wire the implemented pieces into one node after consensus validation and
execution.

## Tasks

- [x] Construct and share the mempool in runtime.
- [x] Construct and share the block/proposal channel, proposal token, reward sets
      and StackerDB configuration in runtime.
- [x] Admit uploaded blocks and proposals through the same validator as followed
      blocks.
- [x] Publish `new_block` only after execution, with nano's actual receipts,
      costs and events.
- [x] Dispatch burn-block events from the transition that produces them.
- [x] Dispatch signer, proposal-response and mined-block events from their
      production transitions.
- [x] Serve every route from the coherent executed snapshot established by
      [[046-distinguish-followed-and-executed-chain-tips]].
- [x] Exercise an event observer against the binary and retain delivered
      `new_block`, burn-block and proposal-response payloads.
- [ ] Run a stock `stacks-signer` against the binary without a compatibility
      shim on a chain where nano derives the active PoX-5 signer set.
- [ ] Submit a valid transaction through the public RPC and observe the same
      transaction admitted, mined, executed and emitted in `new_block`.
- [ ] Finish `/v3/stacker_set`: preserve `stacked_amt`, serve the current
      Waterfall shape and derive its sBTC address instead of returning V0/zero
      placeholders.
- [ ] Serve `/v3/blocks/:id` and `/v3/tenures/:id` from the durable executed
      chain, not only the currently followed/recent view.
- [ ] Populate matured rewards, reward set and miner transaction id in
      `new_block`, then compare receipts, costs and events with an independent
      stacks-core observer for the same executed blocks.

## Acceptance Criteria

- A stock signer runs against nano without an RPC compatibility shim.
- Transaction submission, block proposal/upload, reward-set and StackerDB routes
  are live rather than `Unavailable` or empty defaults.
- An observer receives receipt-equivalent payloads for the same executed blocks
  as stacks-core.
- No RPC endpoint advertises or mutates state newer than the executed tip.

## `new_block` now leaves the node

The dispatcher was built from the configuration and then handed to the miner
alone, so a node that only follows executed every block in silence. It is now
given to the **executor**, which is the only part that knows a block was
executed rather than merely downloaded, and every applied block is announced
from there.

The payload is built synchronously and dispatched with owned values: holding the
chainstate across the await makes the future non-`Send`, since a `ChainState`
carries `RefCell`s and a sqlite connection.

Only the fields a follower can answer are filled in — the parent, the burn block
and its height, and the unlock heights — and the rest are left at their defaults
rather than invented. An observer comparing nano against stacks-core is better
served by a field that is plainly absent than by one that is confidently wrong.
Filling in the matured rewards, the reward set and the miner's winning txid is
the remaining work on this item.

`tests/event_observer.rs` already checks nano builds the same payload stacks-core
published, which is the harder half and says nothing about whether anything sends
it. `tests/event_delivery.rs` is the other half: a listener, a dispatch, and the
body arriving.

## One mempool, shared

The miner built its own `Mempool` and the RPC had none, so `/v2/transactions`
answered `Unavailable` and there was nowhere for a submitted transaction to go.
Worse would have been giving the RPC a second pool: a node that admits
transactions the miner cannot see accepts them and never mines them, which reads
as acceptance and behaves as a black hole.

So the runtime builds one and hands it to both. The miner takes the lock only
while it touches the pool — it awaits a peer between those points, and the RPC
admits into the same pool meanwhile.

`/v2/transactions` now decodes and rejects (`400`, "failed to decode
transaction") rather than reporting itself unavailable, which is the route being
live.

## `/v2/info` answered `503` with a perfectly good tip

It asked a *peer view* for one field — the chain identifier — and refused the
whole request when no peer had been heard from, even with an executed tip sitting
right there, which `/nano/sync_status` reported happily.

The chain a node is on is its own configuration; no peer needs to be asked. With
it taken from there, `/v2/info` answers from the executed tip alone:

```json
{"burn_block_height":960248,"stacks_tip_height":8666422,
 "stacks_tip":"3b0e826e…","stacks_tip_consensus_hash":"61f3f614…"}
```

That is also the acceptance criterion about no route advertising state newer than
the executed tip: this one now cannot, because the tip is the only thing it
reads.

## Burn blocks are news exactly once

A burn block becomes news when the tenure it elected begins, which the follow
loop sees as the consensus hash changing between one executed block and the next.
That is where `new_burn_block` is dispatched from.

The one field a follower could otherwise not answer is `burn_amount`. The burn a
block spends is the burn *distribution's* total, which nano cannot derive
([[049-derive-sortitions-locally]]) — but `bitcoin_spent` in the header is a
running total under threshold signer weight, so the difference between
consecutive headers is exactly this burn block's. Nothing is invented.

The reward recipients, slot holders and PoX transactions are still empty rather
than wrong, for the same reason as `new_block`'s matured rewards.

## One executed snapshot, and every route reads it

`RpcState` held two things that could disagree: the peer's `NodeView` and a
`SealedTip`. `/v2/info` read the second and every other Stacks-compatible route
read the first, so a caller reading the tip and a caller reading a block were
told about two different chains — the exact confusion
[[046-distinguish-followed-and-executed-chain-tips]] is about, moved one route
along rather than fixed.

They are now one value, written once. `publish_executed` takes the sealed tip,
bounds the latest followed view at it, and stores both together:

```
Executed { tip, chain: Vec<FollowedTenure>, pox: Option<PoxInfo> }
```

`executed_chain` walks back from the tip through parent links and keeps only what
it reaches, so a peer's block above this node's tip is not served and a tenure
whose newest blocks were dropped stops advertising them — its `tip_block_id` and
`tip_height` come down with it. `/v2/pox`, `/v3/tenures/info`,
`/v3/tenures/:id`, `/v3/sortitions/consensus/:hash`, `/v3/blocks/:id` and the
"do I already hold this block" check all read that one snapshot.

`/v2/pox` keeps its cycle constants — first height, phase lengths, reward slots,
the pox-5 activation — from the peer's view, because those are *configuration*
that no tip invalidates, and it reports `current_burnchain_block_height` from the
executed tip. Same reasoning as `/v2/info` taking the chain identifier from
configuration rather than from a peer.

**A tip the followed view does not reach leaves nothing**, and that is the honest
answer, not a bug: 36,876 blocks behind mainnet, this node has the headers of
what it executed but not the blocks — nothing stores them once staging drops them
— so it cannot serve `/v3/blocks/:id` for its own tip and says `404`, and
`/v3/tenures/info` says `503`. It would rather answer nothing than answer with
the peer's chain. Serving those two from executed state instead of from a
followed view needs the node to keep the blocks it executed, which nothing asks
of it yet.

### `blocks_behind` was `null` for the only node that has one

The live run found this immediately. A node far behind never walks the peer's
tenure — the walk fails every round from thousands of blocks back — so it
published no view, so `/nano/sync_status` reported `followed_stacks_height: null`
and `blocks_behind: null` for a node 36,876 blocks behind. The height the
catching-up branch already asks the peer for is now published on its own, apart
from the view:

```json
{"followed_stacks_height":8708333,"executed_stacks_height":8671457,
 "executed_stacks_tip":"db9daa91…","blocks_behind":36876}
```

That is the second half of 046's acceptance criterion — a peer at N and an
executor at N−100 visible as two facts — which until now only held once the node
was already near tip.

## Admission: routed to the validator, not a second one

`/v3/blocks/upload` and `/v3/block_proposal` used to hand a decoded block
straight to a channel. A node that admits over its own API what it would refuse
from a peer is forkable through its own API, so both now pass
`ChainState::authenticate_block` — the boundary
[[050-authenticate-every-followed-nakamoto-block]] put in front of execution —
and nothing is reimplemented to do it. `nano_rpc::BlockAdmission` is a
one-method trait whose only implementation, in `runtime.rs`, is
`self.chainstate.authenticate_block(block)`.

The same `Arc<Mutex<CheckpointExecutor>>` is coerced to both `dyn ChainAccess`
and `dyn BlockAdmission`, so it is one mutex: an account read, a block
admission and a round of execution are serialized against each other, and the
RPC can never authenticate against a chainstate a round is halfway through
moving.

Admitted blocks go into the **same staging store** the peer's blocks land in,
drained from the channel at the top of each follow round. From there an upload
and a followed block are the same thing: the executor checks the state root of
both. Live, against mainnet:

```
$ curl --data-binary @08665700-afb74536….bin :20470/v3/blocks/upload
{"stacks_block_id":"0x60099cc7…","accepted":true}        [200]
$ # the same block with header version 3
block refused: block header version 3 is not epoch 4.0's [400]
```

and in the log, `admitted block 60099cc7… at height 8665700 over the public API`.

Ruled out: authenticating in the channel consumer instead. It is one line
shorter and it makes `/v3/blocks/upload` unable to say `accepted: false` and
`/v3/block_proposal` unable to name a reason — which is most of what those two
routes are for.

## `/v3/block_proposal` answers, and does not overclaim

Shape is stacks-core's, because a stock signer reads the verdict from the event
rather than from the response body: `202` with
`{"result":"Accepted","message":"Block proposal is processing, …"}` as soon as the
request parses, and the verdict as a `proposal_response` event. A node with no
observer registered answers `400` — stacks-core's own behaviour, and the right
one: a proposal whose result cannot be reported is a request nobody can act on.

What nano can say, it says with the code a signer branches on. Live, all three
verdicts arriving at a real observer:

```json
{"result":"Reject","reason_code":"NetworkChainMismatch",
 "reason":"proposal names chain 0x80000000, this node is on 0x00000001"}
{"result":"Reject","reason_code":"InvalidBlock",
 "reason":"block header version 3 is not epoch 4.0's"}
{"result":"Reject","reason_code":"UnknownParent",
 "reason":"this node has not executed the parent 645ffeda… this block builds on"}
```

**What it will not say is `Ok` for a block it has not executed.** nano validates
a state root by executing the block, and it cannot execute a candidate off its
tip without leaving that candidate's state behind
([[056-make-rejected-block-execution-leave-no-state]]). So a well-formed
extension of the tip is *admitted* for execution and answered `Reject` with
`ChainstateError`, naming exactly that. A block the node has already executed is
answered `Ok` with a zero cost, which is not a placeholder: stacks-core reports
zero for a block it did not have to execute and its signer reads it that way
(`stacks-signer/src/v0/signer.rs:1569`).

Considered and rejected: answering `Ok` on authentication alone. Authentication
does not look at a state root, and a signer that signs on it would be signing
whatever a proposer computed. A route that lies is worse than one that refuses.
Making it truthful means either a proposal validator with its own chainstate —
which `nano-signer`'s `ChainstateProposalValidator` already is, but owns by value
inside `LiveSigner` — or 056, so that the node's own executor can try a candidate
and roll it back. Either closes this; neither is a few lines.

`chain_id` and `replay_txs` are read from the request. A proposal for another
chain is refused, and a transaction replay set is refused rather than ignored,
because ignoring it would validate a different block than the one asked about.

## The events a route produces

Three dispatch sites were missing and are now wired to the transition that
produces them, not to a poll:

| Event | Dispatched from |
|---|---|
| `stackerdb_chunks` | `POST /v2/stackerdb/…/chunks`, only when a slot **took** the chunk |
| `proposal_response` | `POST /v3/block_proposal`, once the verdict is reached |
| `mined_nakamoto_block` | already: `miner::mine` and `miner::continue_tenure` |

A chunk a slot refuses changed nothing, so nothing is said about it — the test
asserts the refused chunk produces no event, which is the half that a "does it
dispatch" test misses.

The live run's observer received `new_block` ×160, `new_burn_block` ×2 and
`proposal_response` ×4 for one node over a few minutes.

`event_observer.rs` now checks both hand-written payload shapes against
**stacks-core's own readers**, which is the cheapest rung of the oracle ladder
and the right one for a shape maintained by hand: `BlockValidateResponse`
deserializes nano's verdict and reads back the tag, the hash, the cost and each
of the thirteen `ValidateRejectCode` names; `RewardSet` deserializes nano's
`/v3/stacker_set` document. Both found real defects — see below.

## Reward sets, derived rather than relayed

After each catch-up round the follower asks the reward cycle at the executed
tip's burn height, and once per cycle derives the set from **this node's own
pox-5 state** (`signers::active_signer_set`, the linked-list walk the network
derives it from). The document goes to `/v3/stacker_set/:cycle`, and the signers'
hash160s configure the three `StackerDB` message contracts of that cycle
(`signers-{parity}-{1,2,3}`: responses, state machine updates, pre-commits).

Against mainnet, it says exactly what [[050-authenticate-every-followed-nakamoto-block]]
predicted it would:

```
this node cannot derive the reward set for cycle 140 from its own state, so
/v3/stacker_set will not answer for it and its signers' StackerDB contracts
stay unconfigured: reward cycle 140 has no signer set: nothing stacked for it
```

Nothing is stacked in pox-5 for cycle 140 because that reward cycle was prepared
under pox-4. Epoch 4.0 and the pox-5 contract are active, but pox-5's first
reward cycle is 141. Reported once per cycle rather than once per round, and the route answers
stacks-core's own `not_available_try_again` rather than an empty set.

Two shape defects the stacks-core reader caught, both of which would have made a
served set unreadable:

- `signing_key` was `0x`-prefixed. stacks-core writes the key type straight out
  and its reader is not prefix-tolerant; nano's own `SyncClient` refused the
  document with `InvalidHash`.
- `start_cycle_state` was missing, and `RewardSetV0` requires it.

The document is `RewardSetV0`-shaped, not 4.0's `Waterfall`. `WaterfallCycleSet`
requires `sbtc_address`, which comes from the sBTC registry's aggregate public
key through the taproot derivation, and nothing in nano reads it yet — a version
1 document without it does not deserialize at all, so nano serves the version
every reader accepts rather than claiming one it cannot fill. Still open.

`stacked_amt` is served as `0`. `SignerSet` keeps only the weight it apportioned
from the amount, and the weight is what decides whether a block is attested;
reconstructing an amount from a weight would give the threshold back, not the
amount. A stock signer reads `stacked_amt`, so this is on the list below.

### `.miners`: configured only where the answer cannot be got wrong

The two `.miners` slots belong to the last two sortition winners, and which
winner owns which is `num_sortitions % 2` — a count over the whole burnchain that
a checkpointed node has never made and that no snapshot nano holds carries
(`SortitionSnapshot` has no such field). A `.miners` replica with its two slots
swapped refuses the very chunks it exists for, so it is configured only when the
last two winners are the same key — every chain with one miner, which is hacknet
— and otherwise says so once and replicates neither.

Reconfiguring a contract clears every chunk in it, so this is done only when the
writer changes. Doing it per round, which the first version did, would have
dropped the proposal a signer was reading, once a second.

## Changes made outside this task's files

Reported rather than hidden, as the brief asks:

- `crates/nano-chainstate/src/lib.rs`: `mod signers;` → `pub mod signers;`.
  Deriving a reward set from executed state is the item, and the derivation lives
  there. Visibility only.
- `crates/nano-chainstate/src/signers.rs`: `active_signer_set` returns
  `(SignerSet, u128)` instead of `SignerSet`, giving back the per-slot threshold
  `SignerSet::from_reward_slots` already computed and discarded. It is
  `pox_ustx_threshold` in the served document and nothing else can recompute it —
  the weights sum to the reward slots by construction and say nothing about the
  total stacked. One signature line, plus `Ok(set)` → the tuple and `Ok(set)` →
  `Ok((set, _))` at its one internal caller.
- `crates/nano-node/src/config.rs`: `node.block_proposal_token`. `deny_unknown_fields`
  means the token cannot be configured without it. No default: unauthenticated,
  `/v3/block_proposal` lets anyone make a node execute a block of their choosing,
  so a node not given a token answers `503`.
- `crates/nano-conformance/tests/conformance/main.rs`: `mod event_queue;`, and
  `axum` added to `nano-conformance`'s dev-dependencies. `event_queue.rs` was an
  orphan file — never declared, so never compiled and never run. It passes.

## The live proof

A reflink copy of `/home/aldur/mainnet-wasm/state` (btrfs, instant), same
checkpoint and peers, `rpc_bind = 127.0.0.1:20470`, a recording event observer on
`:20471`. Resumed at 8,671,317 and executed forward.

```
route                                            answer
────────────────────────────────────────────────────────
/v2/info                                         200  height 8671457, burn 960335
/nano/sync_status                                200  blocks_behind 36876
/v2/accounts/SP2C2YFP…                           200  nonce 4833, balance 0x…3b0b2539c7
/v2/transactions          (garbage)              400  failed to decode transaction
/v3/blocks/upload         (real captured block)  200  accepted true
/v3/blocks/upload         (header version 3)     400  block refused: …not epoch 4.0's
/v3/block_proposal        (no token)             401
/v3/block_proposal        (token)                202  + proposal_response event
/v3/stacker_set/140                              400  not_available_try_again
/v2/stackerdb/…/signers-0-1                      404  no set for cycle 140 to configure it
/v2/pox                                          503  no followed view 36k blocks back
/v3/tenures/info                                 503  same
/v3/blocks/<executed tip>                        404  the node keeps the header, not the block
```

Every one of those is the route answering from real state or refusing for a
reason it can name. None of them is `Unavailable` because a builder was never
called, which is what the whole task was about.

## Still open

- **A stock `stacks-signer` has not been run against the binary.** The binaries
  are on this machine (`/home/aldur/stacks-core/target/debug/stacks-signer`), but
  a signer needs to be *in the reward set the node derives*, and nano derives
  reward sets the waterfall way from pox-5. Mainnet's current cycle 140 was
  prepared under pox-4, so it has no pox-5 set; pox-5's first mainnet reward
  cycle is 141. The same blocker as 050's signer-weight check is therefore
  time-bounded rather than an absence of Epoch 4 on mainnet. A signer would also need `.miners`
  chunks, which reach a nano node only by a miner POSTing to it — nano does not
  pull `StackerDB` chunks from its peer, and its own miner publishes to the
  peer's `.miners`, not to nano's.
- `/v3/block_proposal` cannot answer `Ok` for a block it has not executed. Needs
  056, or the signer's own proposal validator shared with the RPC.
- `/v3/stacker_set` serves `stacked_amt: 0` and the V0 shape. Needs the stake
  entries kept alongside the derived weights, and the sBTC registry's aggregate
  key read for `sbtc_address`.
- `/v3/blocks/:id` and `/v3/tenures/:id` answer only for blocks still in the
  followed view. A node that kept the blocks it executed could serve its whole
  executed chain.
- `new_block`'s matured rewards, reward set and miner txid are still defaults, as
  they were before this task.

## The last item, split into its three halves

"Exercise a stock `stacks-signer`, transaction submitter and event observer
against the binary" is three claims, and they are not in the same state.

- **Event observer: done.** The live run above received `new_block` ×160,
  `new_burn_block` ×2 and `proposal_response` ×4 at a real listener, and
  `event_delivery.rs` is the offline half. `event_observer.rs` checks the payload
  shapes against stacks-core's *own readers*, which is what caught the two
  reward-set defects.
- **Transaction submitter: the route is live, a real submitter has not driven
  it.** `/v2/transactions` decodes, refuses garbage with `400 failed to decode
  transaction`, and admits into the same `Mempool` the miner takes the lock on.
  What has not happened is a wallet or `stacks-cli` posting a *valid* mainnet
  transaction and it appearing in a mined block, which needs a chain nano can
  mine on.
- **Stock signer: waiting on the applicable mainnet reward cycle and nano's
  remaining signer-facing fields.** Cycle 140 was prepared under pox-4 even
  though Epoch 4.0 and pox-5 are active; pox-5's first mainnet reward cycle is
  141. Run the gate there rather than treating mainnet as indefinitely pox-4.

[[053-pass-the-mainnet-node-release-gate]] carries the same split for the release
gate as a whole, under "what is proved, what is staged, and what needs
wall-clock".
