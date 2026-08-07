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
- [~] Run a stock `stacks-signer` against the binary without a compatibility
      shim on a chain where nano derives the active PoX-5 signer set, and have
      it accept and sign a proposal nano validated with checkpointed leader-key
      history under [[070-carry-leader-key-history-into-proposal-validation]].

      **A stock signer registers against the signer set nano derived.** Run on the
      host -- the binary copied out of the hacknet's own `stacks-signer-1`, version
      `4.0.1 (026bcbc)`, no shim -- pointed at `127.0.0.1:24443`:

      ```
      Signer #2 (ST24VB7FBXCBV6P0SRDSPSW0Y2J9XHDXNHW9Q8S7H) is registered for
        reward cycle 19.
      Cycle #19 Signer #2 Signer is registered for reward cycle 19 as signer #2.
        Initialized signer state.
      ```

      That is the half of this item about *nano deriving the active PoX-5 signer
      set*: the stock signer read `/v3/stacker_set` from nano, found its own key in
      the set nano walked out of pox-5, and took its slot. No compatibility shim, and
      no stacks-core node in the path.

      Two answers it got back, both of which look right:

      * `reward cycle 20 has no reward set yet` (400). Correct on this chain, and for
        the reason [[069-resolve-the-pox-5-follower-state-root-divergence]] settled:
        its sortitions stopped at burn 393, so no block's *tenure* ever reached
        cycle 19's prepare phase and cycle 20's set was never written. stacks-core
        says the same -- `last-set-cycle` there is still 19.
      * One `503` initializing the signer's local state machine, and pulling that
        thread found a **nano defect**, not an environment one. `/v3/tenures/info`
        and `/v3/sortitions` both answer `503` on a node that is executing normally
        at height 14,516:

        ```
        /v2/pox           200      /v3/stacker_set/19   200
        /v2/info          200      /v3/tenures/info     503
                                   /v3/sortitions       503
        ```

        `tenure_info` reads `executed(&state).chain.last()` and answers
        `Unavailable` when that chain is empty. The cause is one line up:
        `publish_executed` builds that chain out of `self.followed` -- the view the
        *follow* loop keeps -- and the comment immediately below it says why that is
        often absent: "Catching up, the follower asks the peer only for its height:
        the tenure walk a full view needs fails every round from thousands of blocks
        back."

        So a node that catches up by height, executes cleanly and seals a fresh tip
        publishes a snapshot with **no chain**, and the two endpoints a stock signer
        asks for first answer `503`. That is what stands between "registered against
        the set nano derived" and "watching for proposals".

        The fix belongs to this task's own rule -- *the chain this node executed,
        never the chain its peer advertised*. `tenure_info` should be answered from
        the sealed tip the snapshot already carries rather than from a peer-derived
        tenure list that a catching-up node never has.

        Measured, so the next session starts at the work and not at the survey.
        `SealedTip` covers `consensus_hash`, `tip_block_id` and `tip_height`
        directly, and its `bitcoin_height` gives `reward_cycle` through the pox
        calendar. Three have no source in it — `tenure_start_block_id`,
        `parent_consensus_hash` and `parent_tenure_start_block_id` — and zeroing them
        is not open: this task's own rule is that a field plainly absent beats one
        confidently wrong. They are answerable from the executed-block archive
        [[046-distinguish-followed-and-executed-chain-tips]] added, which already
        keeps `tenure_start_heights` and resolves a block by height.

        And the shape of that work is settled too, because the obvious version does
        not work. `ExecutedBlocks` exposes only `block(id)` and
        `tenure(start, stop)`, so answering from it alone means walking the tip's
        parents until the consensus hash changes — which is fine for a short tenure
        and unusable here: the pox-5 chain this is being tested on has been extending
        a *single* tenure for thirteen thousand blocks, so one request would decode
        thirteen thousand blocks.

        **Done.** `ExecutedBlocks` gained a `tenure_start` lookup, defaulted to
        `None` so a store with no tenure index still compiles and a route says it
        cannot answer rather than guessing; `Archive` implements it as one query for
        the lowest height of the tip's consensus hash; and `tenure_info` answers from
        the followed view where there is one and from the sealed tip plus that lookup
        where there is not. Live on the hacknet node:

        ```
        /v3/tenures/info  200
        {"consensus_hash":"0xa06c505c…","tenure_start_block_id":"0x4779701c…",
         "tip_block_id":"0x9ae3ee97…","tip_height":14843,"reward_cycle":19}
        ```

        `0x4779701c…` is block **931** — the divergence frontier
        [[069-resolve-the-pox-5-follower-state-root-divergence]] was named for, now
        simply the start of a tenure extended to 14,843.

        `parent_consensus_hash` and `parent_tenure_start_block_id` come back empty on
        this node because the parent tenure predates what its bounded archive holds.
        Empty and not invented, per this task's own rule. `/v3/sortitions` still answers `503`, and it is **not** the same
        defect -- which is worth saying, because it looks like one.
        `SortitionInfoWire` wants the burn block's hash, timestamp, sortition and
        parent-sortition identifiers, the winner's key hash and the seed. A sealed
        tip carries none of that; it lives in the sortition chain, and *this* node
        derives none because its checkpoint carries no sortition history to seed one
        from -- the same absence that made
        [[069-resolve-the-pox-5-follower-state-root-divergence]] reachable here.

        So `503` is the honest answer for this node: it has nothing to say about
        sortitions, and inventing a snapshot would be worse than declining. The fix
        is not in the route but in the rig -- and giving that checkpoint a
        `sortition` directory turned up a **capture-tooling gap** behind it.

        `capture-fixtures` writes `sortition/snapshots.json` and
        `sortition/leader-keys.json` and **not** `sortition/consensus-hashes.json`,
        which is the file `SortitionTracker::history_from` reads. The mainnet capture
        has all three; a capture this tool produced has two, so a node pointed at one
        answers:

        ```
        cannot derive sortitions locally: sortition seed: neither the saved
        sortitions (No such file or directory) nor the capture (No such file or
        directory) can seed a chain
        ```

        That history is the one part a node cannot re-derive from its own snapshots,
        because `ConsensusHash::from_ops` mixes hashes at power-of-two offsets. So
        **every** capture this tool has produced yields a `sortition/` directory no
        node can use -- which is why this rig derives no sortitions, and why
        [[069-resolve-the-pox-5-follower-state-root-divergence]] was reachable on it.

        **Fixed, and it corrects the paragraph above.** `write_capture` writes the
        history now, ended at the last burn block that *elected* somebody rather than
        simply the last one captured -- a chain is seeded by the snapshot its history
        ends at, and the tracker refuses a seed whose own block won nothing, because
        the sampling of the next block mixes the most recent winner's seed. The
        snapshots and Bitcoin blocks still run to the full span, since the replay needs
        the burn blocks above the seed. Re-captured and restarted, the rig says:

        ```
        deriving sortitions locally from burn 393 on PoX history 1111…
        ```

        So `503` was *not* the honest answer after all: this node has a sortition chain
        now and `latest_sortition` still cannot answer, because it reads
        `executed().chain.last()` -- the **followed** view -- exactly as `tenure_info`
        did before it was fixed. The route needs the same treatment: answer from the
        node's own sortition chain, which holds every field `SortitionInfoWire` wants
        and which the executor already carries. That is the next action, and it is a
        route fix rather than a rig fix.

        **Done, and it was a route fix.** The three sortition routes read a derived
        view published with the executed tip, anchored at the burn view the node has
        *executed* under rather than at the derived chain's own tip, which runs ahead
        naming views for staged blocks. Two fields had no source and are now carried
        off the winning commitment, which already parses both: `committed_block_hash`,
        and the parent burn height that resolves `stacks_parent_ch` through this
        node's own consensus-hash history.

        Serving a *peer's* sortition was never only a gap — it would hand a stranger
        the burn view and through it the fork. So the regression publishes a peer view
        and a derived one that disagree on every field, and asserts the peer's own
        burn view comes back `404`.

        Live on mainnet, `/v3/sortitions` answers burn 961,377 and **all twelve
        fields match what a stock stacks-core node answers** for the same consensus
        hash, derived from nano's own Bitcoin blocks with nothing taken from that
        peer. `latest_and_last` returns the pair.

        One thing had to be fixed before any of it could be *seen*: a catch-up round
        executes up to 500 blocks with no await between them, so the node's whole HTTP
        surface went dead for minutes at a time — `/v2/info` included, socket holding
        connections nobody accepted. A node that cannot answer while catching up
        cannot host a signer, and that is exactly when a signer asks.

      **The rig now exports a sortition history, and the next defect is named.**
      `cargo xtask export-sortition` writes the three files a chain seeds from, and
      `signer-checkpoint.sh` calls it, so the rig is reproducible rather than
      hand-built. The stock signer moved `RequestFailure(503)` →
      `UnexpectedSortitionInfo` → routes answering `200`: it reads the content now
      instead of having the request refused.

      What stops it there is a real gap, stated exactly. **A derived sortition chain
      only advances when execution asks it to.** The chain is walked forward to find
      the burn view a staged block stands on — so a node sitting at the chain tip
      with nothing to execute never derives its own tip's burn view, `snapshot_at`
      answers nothing for it, and `/v3/sortitions` is `503` on a node that is
      perfectly healthy and simply idle. That is exactly the condition a signer waits
      in.

      **Fixed.** A round walks toward Bitcoin's tip whether or not it executed
      anything, bounded by the same limit and quiet when nothing moved. On the rig
      `/v3/sortitions/latest_and_last` went from `503` to the full pair — burn 392
      and 391, each naming its winner's key and its predecessor.

      **And the signer moved one step further, onto the same defect a third time.**
      It now asks for the last block of the parent tenure and gets `404`:

      ```
      Signer State: Failed to fetch last block in parent tenure from stacks-node,
        parent_tenure_id: bdb04d52…, err: RequestFailure(404)
      Failed to initialize local state machine: NoParentTenureInfo(bdb04d52…)
      ```

      `tenure_tip_metadata` reads `executed().chain` — the peer-derived followed
      tenures — which is exactly what `tenure_info` did before it was fixed and what
      the sortition routes did before they were. A node whose followed view is empty,
      which is every catching-up node and every idle one, cannot answer.

      The fix is the same shape and the archive already holds the answer: it keeps
      `tenure_start_heights` and resolves a block by height, so a `tenure_tip`
      lookup beside the `tenure_start` one added for `tenure_info` answers it without
      walking the tenure's blocks — which matters here, where one tenure has extended
      for thirteen thousand of them.

      **Done, and it moved the blocker off nano.** The route falls back to the
      archive by one query keyed by consensus hash. The signer still gets `404`, and
      that answer is now the honest one: nano's archive holds 17,476 blocks and every
      one is in tenure `a06c505c`. The sortitions at burns 390-392 elected miners
      whose tenures never produced a block, because the chain kept extending the one
      before them -- so there is no last block of `bdb04d52` for anybody to serve.

      **What is left is the rig's chain shape, not a nano route.** All three routes a
      stock signer asks for on the way in now answer from what this node executed.
      This chain has been extending a single tenure since block 931 and is stalled
      outright, because participant 3 is down and Hacknet needs all three signatures.
      No proposal arrives on a stalled chain. The next action is to get it electing
      again -- restore participant 3, let the three stock signers carry it through
      fresh sortitions, and only then hand the signer half to nano.

      What is still owed is the acceptance itself: a proposal arriving while the
      signer watches. This chain's miner is extending one tenure rather than starting
      new ones, so proposals are sparse; a chain still electing miners would produce
      them promptly.

      The earlier container attempt and why it failed, kept because it names the
      trap: run the signer *on the host*, not inside the compose network.
      **Container-to-host reachability, not nano.**
      A stock `stacks-signer` -- the binary out of the hacknet's own
      `stacks-signer-1`, no shim -- was configured against nano's RPC with a key from
      the active reward set and the `auth_password` matching nano's
      `block_proposal_token`, and it started:

      ```
      Signer spawned successfully. Waiting for messages to process...
      INFO [libsigner/src/runloop.rs:65] Signer runloop begin
      ```

      It then times out reaching `10.0.0.1:24443` from inside the container. Nano
      binds `0.0.0.0:24443` and answers on the host (`/v2/info` returns tip 14,516),
      so what is missing is a route from the compose network to the host -- run the
      signer on the host against `127.0.0.1:24443`, or put nano on the compose
      network, and the run proceeds.

      The rest of the preconditions are now met, which they were not before: nano
      follows this chain to its tip (14,516, zero state root mismatches) after the
      tenure-height fix in [[069-resolve-the-pox-5-follower-state-root-divergence]],
      so proposals are reachable and validated where the earlier hosted-signer run
      froze at height 931.
- [x] Submit a valid transaction through the public RPC and observe the same
      transaction admitted, mined, executed and emitted in `new_block`.
      `submitted_transaction.rs` walks the whole journey offline and deterministically:
      a **captured** mainnet-accepted transaction is posted to `/v2/transactions`
      over a real socket to a served listener; it lands in the pool the miner reads;
      the pool *offers* it against this node's own tip accounts; nano assembles its
      own block in the place of the one that was dropped and that block carries it;
      the receipt says `Success` where the network's receipt said `success`; and the
      `new_block` payload names it with the same `status`, `raw_result` and
      `execution_cost` stacks-core published for the same execution.

      A captured transaction rather than a forged one, because a valid one needs a
      funded account at the right nonce and a fixture cannot forge that without a key
      it does not carry — one the network itself accepted is valid by construction
      and arrives with its own oracle. The block is also asserted to be refused when
      the pool offers nothing, so "mined" cannot be satisfied by an empty block.

      `event_delivery.rs::an_observer_receives_what_the_node_dispatches` is the other
      half — a listener, a dispatch, and the body arriving — so the payload is not
      only built correctly but sent.
- [x] Finish `/v3/stacker_set`: preserve `stacked_amt`, serve the current
      Waterfall shape and derive its sBTC address instead of returning V0/zero
      placeholders. `DerivedRewardSet` keeps `stacked` and `pox_ustx_threshold`
      through the pox-5 walk and `stacker_set_payload` emits the waterfall shape,
      falling back to V0 only when the payout address cannot be derived; checked
      field by field against `stackslib`'s own `RewardSet` serde.
- [x] Serve `/v3/blocks/:id` and `/v3/tenures/:id` from the durable executed
      chain, not only the currently followed/recent view. `nano-node/archive.rs`
      is a bounded sqlite store written at seal and retracted on a fork switch; the
      handlers ask it first. The invariant holds because it only ever holds blocks
      the executor sealed.
- [~] Populate matured rewards, reward set and miner transaction id in
      `new_block`, then compare receipts, costs and events with an independent
      stacks-core observer for the same executed blocks.
      `miner_address`, `from_stacks_block_hash` and `from_index_consensus_hash` are
      read back out of the executed-block archive, from the start block of the
      tenure that matured. They stay absent for the first 100 tenures after a
      checkpoint and for a node with no archive: a checkpoint carries what is owed,
      not where it was earned, so they are empty rather than guessed. The
      independent-observer comparison over the same *mainnet* blocks is the part
      still open.
- [x] Exercise a stock `stacks-signer`, transaction submitter and event observer
      against the binary far enough to validate RPC shapes, signer registration,
      StackerDB writes, transaction admission and observer payloads. This does
      not claim the signer accepted a block or the transaction was mined.
- [x] Fail StackerDB replication over between discovered peers under
      [[071-fail-over-signer-role-replication-across-peers]]; one initially
      selected HTTP client must not remain load-bearing for the hosted signer.
      Six loops held one endpoint and none does now; `replication_failover.rs`
      breaks the first peer six ways over real HTTP. The live half — removing a
      configured peer from a running node — stays open there.

## Acceptance Criteria

- A stock signer runs against nano without an RPC compatibility shim.
- The signer accepts and signs a valid proposal through nano; registration and
  accepted StackerDB chunks alone are not sufficient.
- A submitted transaction appears in an accepted, executed block and its
  `new_block` event; a `200` admission response alone is not sufficient.
- Transaction submission, block proposal/upload, reward-set and StackerDB routes
  are live rather than `Unavailable` or empty defaults.
- Signer-role replication survives loss or rate limiting of its initially
  selected peer.
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

## The three halves, on a pox-5 chain

Hacknet on epoch 4.0 removes the blocker above: a waterfall reward set exists, so a
stock signer can hold a slot. `hacknet/harness.sh host 3` runs nano as the *node*
half of a participant and a stock `stacks-signer` 4.0.1 as the signer half, with
nano's RPC as its only node. That is the hard direction — the signer keeps no chain
state and knows no peers, so every proposal it reads, every verdict it acts on and
every chunk it writes has to come out of nano.

nano runs no signer of its own in that configuration. The proposal validator and
the embedded signer are the same chain state, and opening it twice is the recorded
panic; so a node does one or the other, and `start_hosting` says which.

### What the stock signer actually did

It registered, held its slot, and wrote through nano:

```
Signer #0 (ST1J9R0VMA5GQTW65QVHW1KVSKD7MCGT27X37A551) is registered for reward cycle 19.
Cycle #19 Signer #0 Signer is registered for reward cycle 19 as signer #0. Initialized signer state.
Cycle #19 Signer #0: Received state machine update from signer 02007311430123d4cad97f4f7e86e023b28143130a18…
```

The reward cycle it registered for is the one **nano derived from its own pox-5
state**, not one it relayed:

```
derived the reward set for cycle 19 from this node's own state: 3 signers, 30 of weight,
  replicating their StackerDB contracts
replicating .miners for the miners that hold its slots, in order: 8585d3e4…, 8585d3e4…
```

`NANO_TRACE_RPC=1` makes every request name itself, which is the only record of
*which* routes a client used — a signer's own log does not keep one. The routes the
stock signer drove, verbatim from nano:

```
GET  /v2/info                                                        200
GET  /v2/pox                                                         200
GET  /v2/accounts/:principal                                         200
GET  /v3/stacker_set/:cycle                                          200
GET  /v3/sortitions/latest_and_last                                  200 / 503
GET  /v3/tenures/tip_metadata/:consensus_hash                        200
GET  /v3/tenures/fork_info/:start/:stop                              200
GET  /v2/stackerdb/…/signers-1-2/:slot                               200
POST /v2/stackerdb/…/signers-{0,1}-2/chunks                          200
POST /v2/contracts/call-read/…/signers/get-last-set-cycle            200
POST /v2/contracts/call-read/…/signers/stackerdb-get-signer-slots-page 200
```

**32 chunks were taken from it over that POST route**, all `StateMachineUpdate`,
each verified against the writer nano assigned the slot. `tests/conformance/hosted_signer.rs`
asserts them from the `stackerdb_chunks` events nano dispatched and *not* from
nano's replica, because nano also pulls its peer's chunks into the same replica and
the hosted signer shares its key with the stock signer it replaced — a chunk read
out of the replica could be either one's. An event is dispatched only where a chunk
was POSTed, and nothing but the hosted signer POSTs to nano. Getting that wrong is
how the first version of this test reported a *replicated* acceptance as the hosted
signer's work.

### Five things a stock signer could not read, and now can

Every one of these was found by pointing the real binary at nano and reading what
it refused. None of them needed a shim to work around; they were nano answering
with a shape stacks-core's own reader rejects, which is a defect rather than a
difference.

| Route | What was wrong |
|---|---|
| `/v2/info` | no `pox_consensus` and no `server_version`; `PeerInfo` requires both. `stacks_tip` was the block *identifier* where the reader wants the block hash. |
| `/v2/pox` | seven fields of `RPCPoxInfoData` absent — `current_cycle`, `next_cycle`, `epochs`, `current_epoch`, `reward_cycle_id`, `contract_versions[].first_reward_cycle_id`, and the sBTC contracts. serde refuses a document missing a field, so answering with a useful subset is answering with nothing. |
| `/v3/sortitions`, `/v3/sortitions/latest_and_last` | not served at all. The signer builds its whole view of who may mine from the second, and refuses to build one if the pair is not returned together. |
| `/v3/tenures/fork_info/:start/:stop`, `/v3/tenures/tip_metadata/:ch` | not served at all. The first is the signer's reorganization check; the second it asks on every tenure it evaluates. |
| `/v3/sortitions/consensus/:ch` | `vrf_seed` was absent, and `prefix_opt_hex` deserializes a *field* — a missing one is an error and not a `None`, so every sortition document nano served was unreadable. |
| `POST /v2/stackerdb/…/chunks` | a refusal carried no `metadata`, so a writer was told "wrong version" without being told which. A stock signer was seen walking its version number up one request at a time, 1643, 1644, 1645… |
| `stackerdb_chunks` event | named its contract with the `address.name` string the route is keyed by; the reader wants Clarity's `QualifiedContractIdentifier`, and a signer's event listener drops the whole event. |

`event_observer.rs` now runs `StackerDBChunksEvent` and `BurnBlockEvent` — the
readers a hosted signer's listener uses — over nano's hand-written payloads, which
is the offline half of the last row and would have caught it.

### StackerDB replication, which is what makes hosting possible at all

A node that serves only its own replica hosts a signer that can see no proposals
and whose answers reach nobody: the miner counting a response reads it from *its*
replica, and nothing carried it there. nano's own signer never needed this because
it reads and writes the peer's StackerDB directly.

So `hosting::replicate` pulls each contract's chunks from the peer and pushes back
what nano took, over the same `/v2/stackerdb` routes a signer uses. Every pulled
chunk goes through `StackerDbStore::put`, which verifies it against the writer nano
assigned the slot — replication, not trust; a peer serving a forged chunk gets it
refused, and the log says so.

`.miners` no longer refuses to replicate when the last two sortitions went to
different miners. Which winner owns which slot is `num_sortitions % 2`, a count a
checkpointed node has never made — but **every slot's metadata is signed by the
writer that owns it**, so recovering the peer's listing says who that is, checked
against the miners this node saw win. Both slots have to resolve or nothing is
configured: a slot assigned to the wrong writer refuses the very chunks it exists
for, and the first version of this guessed the second slot from the first, which
left a hosted signer with nothing to answer while the log said `.miners` was
replicated.

### The transaction submitter

`/v2/transactions` was a black hole with a `200`: it admitted into the pool the
node's *own miner* reads and told nobody, so a transaction posted to a following
node could never be mined by anybody. It is now relayed the way a block admitted
over the API already was — announced to the p2p relay by the follow loop, from the
same place and for the same reason.

Live, against nano on hacknet:

```
nano admitted 43f6862ebbc1a47cc8db4490a40d39bfc55dcbaf9154c14b7f72ffebd40a6c35 into the
  mempool its miner reads, answering 200 OK
the network reports 43f6862e… as not mined: nano admitted it, and this configuration
  does not mine
```

Admission is nano's own answer and is asserted. Whether it is *mined* is the
network's, and the test reports it rather than asserting it: this node follows and
does not mine, so being mined depends on the relay reaching a miner, and conflating
the two would be claiming the second on the strength of the first.

### The event observer

A listener in nano's `event_observers`, recording every event to a file
(`hacknet/event-sink.py` now keeps all of them, not only `new_block`):

```
nano's observer received 632 new_block events
nano's observer received 109 new_burn_block events
nano's observer received 32 stackerdb_chunks events
the receipts agree for all 632 blocks both observers were told about
```

The last line is the one that matters. The same sink recorded what the *stock*
nodes' observer was told for the same blocks, and for every block both saw, nano's
per-transaction receipts — `txid`, `status`, `raw_result`, `execution_cost` — are
identical to stacks-core's, along with the block height and the parent identifier.
A payload that merely arrives says nothing about whether it describes the same
execution; this says it does, 632 times.

### What this does not prove

- **The hosted signer has not accepted a block through nano.** It answers `Reject`
  because nano does, and nano does because its proposal validator cannot execute a
  candidate: `proposal execution failed: invalid transaction: committed seed is not
  the hash of the parent tenure's VRF proof`. The cause is named exactly — the
  validator has no leader-key registry, because `[checkpoint] sortition` is what
  carries one and `signer-checkpoint.sh` exports no sortition history. A
  registration is named for years after it is made, so it is far below any burn
  window a follower holds. This is the same hole as before, localized: it is not
  "056 or a shared validator" but *the checkpoint not carrying `leader-keys.json`
  and the snapshots that seed a tracker*, plus wiring that tracker into the
  proposal validator the way it is wired into the executor. It is tracked in
  [[070-carry-leader-key-history-into-proposal-validation]].
- **nano's follower stopped on a state-root mismatch** at height 931 of that chain
  (`expected f90f06c9…, got e939a724…`, two transfers, not a tenure start), so its
  executed tip froze and `/v3/sortitions/latest_and_last` then answered `503` — a
  stale executed chain cannot name the sortition before its tip. A divergence on a
  pox-5 hacknet chain is a replay finding of its own and is tracked in
  [[069-resolve-the-pox-5-follower-state-root-divergence]].
- The chain was **not** kept running on nano's signature alone. Hacknet needs all
  three, and a signer that rejects stalls it; the runs above therefore kept the
  stock signer available and switched it off only in bounded windows. What
  `hacknet_replacement` shows for nano's own signer is not shown here for a hosted
  one.

### One thing learned the expensive way

**A Hacknet signer missing for a whole prepare phase kills the chain.** No blocks
in the prepare phase means no PoX anchor block for the cycle after it, and the
coordinator then repeats `Missing canonical anchor block` forever. Two chains were
lost to it — one to a previous run's node dying mid-prepare-phase, one to failed
starts of `host` while the participant it replaces was already stopped. `host` now
starts nano *first*, waits until it answers `/v2/pox` and `/v3/stacker_set`, and
only then stops the stock pair; a failed start stops nothing.

`check_maturity_window` also refused every checkpoint a fresh network can produce.
The earliest payout any block can ask for is tenure 1, because a tenure below the
maturity horizon matures nothing — so earnings reaching back to the chain's first
tenure are complete however few they are, and demanding a hundred of them made nano
unable to start from any chain less than a hundred tenures old.
