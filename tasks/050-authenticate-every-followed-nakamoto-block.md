---
id: "050"
title: "Authenticate every followed Nakamoto block"
status: in-progress
priority: critical
effort: large
type: feature
group: mainnet
dependencies: ["024", "049"]
tags: ["mainnet", "chainstate", "consensus"]
created_at: 2026-08-02
---

# Authenticate every followed Nakamoto block

## Objective

The follow path currently checks block decoding, Merkle root, parent, height,
timestamp and eventual state root. It does not verify signer weight, the miner
signature, the winning leader, VRF proof or committed seed. Several decoded
consensus fields and transaction network constraints are also not checked.

Put one validation boundary before execution, supplied only by nano's executed
state and local burn view.

## Tasks

- [x] Resolve the active reward set from executed state and verify ordered signer
      signatures and threshold weight.
- [x] Verify the miner signature against the local sortition winner and leader
      key.
- [x] Finish [[024-verify-the-vrf-seed-a-block-commits-to]] on this path.
- [x] Validate tenure-change and coinbase semantics against the local snapshot.
- [x] Enforce the header version for the active epoch.
- [ ] Enforce `bitcoin_spent`, PoX treatment and problematic transaction rules.
- [x] Enforce transaction version, chain ID, network and anchor-mode constraints
      on followed blocks, not only in the mempool.
- [x] Reject before beginning VM execution and return a distinct consensus error.

## Acceptance Criteria

- Every captured mainnet block passes the complete validator before replay.
- Mutating each authenticated field produces a focused rejection test.
- No signer, miner, VRF or sortition validation input comes from the peer that
  supplied the candidate block.
- A block with a self-consistent state root but invalid consensus authentication
  is never sealed.

## One boundary, before anything runs

`ChainState::authenticate_block` is that boundary, called from
`execute_nakamoto_block` before the VM is touched. It answers only from this
node's own network configuration — nothing is asked of the peer that supplied
the block — and returns `ConsensusError`, which is distinct from an execution
failure so a caller can tell "not our chain" from "did not compute".

It checks the epoch's header version, ignoring the shadow flag above it, and per
transaction: the version byte's network, the chain identifier, and that the
anchor mode is not off-chain, which names microblocks that 4.0 does not have.

None of these is something a state root would catch. A node that executes them
computes a perfectly self-consistent state for a chain nobody else is on, which
is the whole reason they belong before execution rather than after.

`tests/block_authentication.rs` gives each its own rejection, mutating a real
captured block — the transaction cases by changing a byte and decoding again,
which is also what arriving from a peer looks like — and pins that a block the
network accepted still authenticates, with the shadow flag set or not. Mainnet
replay to 8,666,422 raises none of them.

The rules that need more than the block are beside it in `check_before_executing`,
which is the same boundary widened: the signer weight, the tenure change against
the executed chain, the miner signature against the sortition winner, and the VRF
rules of [[024-verify-the-vrf-seed-a-block-commits-to]]. What each of them can
reach today is the rest of this note.

## Correction: the reward set does not have to be derived

The note this section replaces said signer weight was derivable but not provable,
because walking pox-5's positions finds nothing for a cycle that was stacked in
**pox-4** — which is mainnet's cycle 140, the one being replayed:

```
no signer set for the cycle at burn 960248:
  reward cycle 140 has no signer set: nothing stacked for it
```

That is still true of the *walk*, and the coordinator's finding on block
8,673,846 — `StackStx` routed to pox-5 where the chain left the account alone,
because `active_pox_contract_for_cycle` is cycle-keyed and these cycles are still
pox-4's — is more evidence for it.

But the set does not have to be re-derived. `.signers` **records** it: at a
prepare phase the node computes the next cycle's set and writes
`stackerdb-set-signer-slots` and `set-signers`, and those writes are consensus
state, so a set that is wrong fails the state root of the block that wrote it.
Reading it back is therefore not trusting a peer and not trusting nano's own
arithmetic either — it is reading a number the network has already agreed with.

Three things follow, and they are why this is now a rejection:

- **It reaches back before the checkpoint.** Cycle 140's entries were written by
  stacks-core in a pox-4-era prepare phase, below the exported state, and came
  across with it. So mainnet's signer weight is checkable *today*.
- **It is one contract call, not forty-five.** The walk costs three calls per
  staker: 42.85 s to replay the 340-block capture against 1.55 s without it.
  Reading `.signers` costs 1.70 s.
- **The two agree.** `signer_weight_enforcement::the_derived_and_recorded_signer_sets_agree`
  puts nano's own pox-5 derivation beside what the chain recorded, which is what
  makes reading the recorded one instead of walking safe.

`ChainState::recorded_signer_set` is the reader, `SignerWeights` is the set —
signing-key hashes and weights, because `.signers` stores a principal rather than
a public key, and a hash is what both the burnchain and a signature check need.
`SignerSet::verify` goes through the same code: one rule, one implementation, and
the ordering half is the part that would otherwise drift unexercised.

### What it is checked against

Not against itself. Four oracles, in order of how hard they are to fool:

| Oracle | What it says |
|---|---|
| `/v3/stacker_set/:cycle` for the captured chain | the recorded set *is* the published one, signer for signer and weight for weight |
| the pox-5 walk | the derivation and the record agree |
| 340 captured blocks, replayed | every one carries threshold weight from that set |
| an imported **mainnet** state | its cycle-140 set is mainnet's own — 25 signers, 3,712 weight, 2,599 to approve — and 105 real mainnet blocks pass `verify` against it |

The last one is the one that mattered for turning a report into a rejection.
Composing "mainnet's blocks verify against the published set" (`mainnet_envelope`)
with "the recorded set is the published set" would have been an argument;
`mainnet_blocks_pass_the_check_against_mainnet_state` is a measurement, with the
set out of state, the blocks off the wire and the same `verify` the follow path
calls. It needs `NANO_MAINNET_STATE` pointing at a directory a node has imported
into, and skips otherwise.

**A cycle with nothing recorded is reported once a tenure and accepted.** That is
not a check that passed, and the message says so. Once a block rather than once a
tenure would bury everything else a mainnet run prints, which is how a real
message gets missed.

### Two things this turned up

`signing_principal` rendered every signer with the **testnet** version byte, where
`handle_signer_stackerdb_update` uses `p2pkh_from_hash(is_mainnet, ..)`. Invisible
on a testnet chain, and a state root divergence at mainnet's first prepare phase
with nothing else to show for it. Fixed; no current test moves, because the
captured chain is testnet.

And a block being **assembled** has no signatures yet — the miner signs the header
at seal time, after validation, and the signers only see it afterwards — so every
rule that reads one is asked of a followed block and not of a candidate
(`authenticate::Signatures`). The coinbase VRF proof is not exempt: that is the
miner's own work, and a miner that cannot prove it won its own sortition should
hear about it before it publishes.

## Tenure and coinbase semantics: the block, and the chain behind it

Two layers, and only the second needs state.

The first is `stackslib`'s `is_wellformed_tenure_start_block`,
`is_wellformed_tenure_extend_block` and `check_tenure_tx`, read *together* — which
is the only way to read them, because each returns "not one of mine" for the
other's blocks and only their union says which blocks are refused. At most one
coinbase and one tenure change; a coinbase needs a change to authorize it; a
tenure start is the change first and the coinbase second; an extension is owed no
coinbase and claims its own tenure as its previous one, where a block found
cannot; the change ends at the block's own parent and names the block's own
tenure; and the change names the miner that signed the header, without which a
tenure change lifted out of a competing miner's block would carry over intact.
Also: every coinbase carries a VRF proof, so the 2.x wire forms that decode
without one belong to no 4.0 block.

The second is `check_nakamoto_tenure`'s two history claims — which tenure a change
confirms, and how many blocks that tenure ran (`get_nakamoto_tenure_length`, the
parent's own `height_in_tenure`). nano answers both from the blocks it has
executed: a tenure is the run of blocks sharing a consensus hash, since a new
sortition is a new hash and an extension keeps the one it has.

Both are **skipped rather than guessed** when the executed list cannot answer, and
the two cases differ. A parent that is not in the list is the checkpoint's anchor
— reported, because that is a real hole. A count whose tenure began below the
retained 256 blocks cannot be *completed*, and a partial count is lower than the
truth, so enforcing it would refuse an honest block: that one is passed over in
silence, because the tenure tie beside it did run and a mainnet tenure longer than
the window would otherwise say so on every block of it.

`tenure_continuity`'s control is an oracle in its own right: the
`previous_tenure_blocks` the network counted for a tenure is the number of blocks
nano executed in it, so the two mutations beside it reject something real.

**The mainnet capture cannot falsify the count, and that is a capture shape.** Its
hundred blocks hold two tenures — nine blocks and then ninety-one — so there is
one boundary in the sample and the tenure before it began below the span. The
identity half is checkable there and checked; the count half needs a capture
holding two consecutive whole tenures, which at mainnet's tenure lengths means a
few hundred blocks. `mainnet_tenure_changes_agree_with_the_count_over_a_tenures_blocks`
prints how many of each it could check rather than passing on an empty comparison.

So the count is the one rule this task enforces with no mainnet oracle behind it,
and it is worth knowing what it would look like if it were wrong: a rejection
naming both numbers — "the tenure change reports N blocks in the tenure it ends,
where this chain executed M" — on a tenure-start block, on a chain that had been
following fine. The arithmetic cannot drift for the reasons a chain diverges
(nano executes every block of a branch in order, and a retraction splits the list
rather than trimming it), but the sample that would prove it is a longer capture.

The half that is **not** at this boundary: the header's `consensus_hash` against
the local sortition's, and the tenure change's `burn_view_consensus_hash`.
`BitcoinBlockContext` carries the burn header hash but not the consensus hash, so
the comparison lives in `nano-node`'s `report_disagreements`, where it is printed
per burn block. Moving it here needs one field on the context.

## What the miner signature can and cannot reach

Two rules, and they are different questions:

- **The tenure change names the miner that signed the block.** Always checkable,
  always enforced, needs nothing but the block.
- **That miner is the one whose leader key won the sortition.** Needs the winning
  leader key's registered block-signing `Hash160`, which is the second half of the
  registration whose VRF key [[024]] already resolves.

The second is implemented, on the follow path, and rejects — `verify_miner_signature`
against `registered_signing_key_hash(operations, winner_vrf_public_key)`, keyed by
VRF key because the burnchain refuses a VRF key that is already registered, so one
key names one registration. It is proved both ways in
`tenure_vrf_enforcement`, and the capture supplies the oracle that makes it more
than self-consistent: **the block-signing hash the winner registered on Bitcoin is
the `Hash160` of the key that signed the first block of the tenure it won**. One
side comes out of a Bitcoin transaction and the other out of a header signature;
nano is asked nothing.

What it cannot reach yet is its input. A leader key is registered once and reused
across tenures, so the registration sits in a burn block far below the operations
a block is handed — and the winner's signing hash is not carried anywhere.
`SortitionWinner` resolves the registration already to publish
`winner_vrf_public_key`, so this is retention rather than derivation:
`SortitionSnapshot` and `BitcoinBlockContext` each want one 20-byte field, and
`nano-node`'s `local_sortition` one line. Until then the node reports, once a
tenure, that it knows which key won and not what that key signs with.

Also still true from [[049]]: `nano-node` keeps the `(candidates == 1)` hedge
around `winner_vrf_public_key`, so in production the winner is usually `None` and
both this rule and the VRF proof check report rather than run. The hedge is
obsolete — the winner derives for all fourteen captured sortitions — and removing
it is a one-line change in a file this task does not own.

## `bitcoin_spent` is compared, in the wrong place

The header's `bitcoin_spent` is the burn view's running total, and nano does
compare it: `SortitionTracker::agrees_with_header`, from `nano-node`. A
disagreement stops the local derivation and falls back to the peer's sortitions
rather than rejecting the block — which is right for the tracker and wrong for
this boundary, since a block claiming a total the burnchain does not have is not
this chain's block.

It cannot move here as things stand. `BitcoinBlockContext::burn_spend_total` is
the *sortition's* burn, not the cumulative one, and the executed list keeps no
per-block total (adding one means editing `ChainLedger`, which another task owns).
The cumulative number is in `SortitionSnapshot::total_burn` and `local_sortition`
already holds it, so this is one field on the context and three lines here.

## PoX treatment: there is no rule, and that is pinned

Under a waterfall cycle `check_pox_bitvector` returns on its first line:
`rewarded_addresses()` is `None` for anything but a V0 set, and stacks-core's own
comment on `pox_treatment_bitvec_len` says the single bit a 4.0 miner sends "is no
longer used in consensus [and] the miner includes it for deserialization
compatibility".

So nothing is enforced, and
`block_authentication::pox_treatment_is_not_consensus_under_a_waterfall_reward_set`
exists to say why against stacks-core rather than in a comment — it deserializes a
reward set the captured chain published and asserts both facts. A header field
that nothing checks and nothing explains is how a rule the network does not have
gets invented later.

## Problematic transactions

Enforced: the cap (`MAX_PROBLEMATIC_TX_MARKERS`, pinned against `stackslib`'s own
constant rather than against the same arithmetic repeated), strictly increasing
indices, every index in bounds, and no marker on a coinbase or tenure change.
They are in the block hash for header version 1, so a wrong one is a different
block — but a replay *follows* them, so a marker pointing at nothing would have
two nodes execute different transaction sets out of the same bytes.

## Where it stands

Rejecting, each with its own test that mutates a real block: header version;
transaction network, chain and anchor mode; empty block; coinbase without a VRF
proof; the whole tenure/coinbase shape; the tenure change's miner; the tenure it
confirms and the blocks it counts; problematic-transaction markers; signer weight,
ordering and uniqueness; the coinbase VRF proof and the committed seed.

Reporting, with the reason named in the message: the winner's block-signing hash
(not carried yet), a reward cycle with no recorded set (mainnet's pre-4.0 cycles),
a parent below the checkpoint, the first tenure's committed seed.

Elsewhere: `bitcoin_spent` and the sortition's consensus hash, both compared in
`nano-node` and reported.

The whole capture still authenticates — 340 blocks of it, plus the 100 consecutive
mainnet blocks the capture holds — which is the outcome that matters most here: a
rule that refuses a block the network accepted would be worse than a missing one.

`restart.rs` needed a change to go on passing, and it is the interesting kind. Its
competing tenure was the captured block with one second added to its timestamp,
which is in both signature preimages: the captured miner signature over a changed
header recovers to some other key, and the captured signer signatures belong to a
block hash that no longer exists. No edit of somebody else's block can be made
valid, because that is exactly what these rules refuse — so the fork is *mined*
now, with a tenure change naming the miner that signs it. A synthetic block that
could not exist is not a test of a rule about real ones.
