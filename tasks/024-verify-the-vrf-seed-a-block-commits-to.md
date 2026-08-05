---
id: "024"
title: "Verify the VRF seed a block commits to"
status: in-progress
priority: high
effort: small
type: feature
dependencies: []
tags: ["mainnet", "chainstate", "consensus"]
created_at: 2026-07-30
---

# Verify the VRF seed a block commits to

## Objective

`nano-crypto` proves and verifies VRF, and `nano-sortition` mixes the seed, but
`nano-chainstate` never checks one: the word `vrf` does not appear in the crate.
A nano follower therefore accepts a tenure-start block whose coinbase proof does
not correspond to the winning leader key, and accepts a `new seed` its commitment
did not derive.

stacks-core validates this before it will build on a block. nano has to as well,
or it will follow a chain the network will not.

## Tasks

- [x] Resolve the winning leader key's VRF public key for the tenure being
      started.
- [x] Verify the coinbase proof against that key and the parent's seed.
- [x] Check the seed the commitment carries is the one the proof derives.
- [x] Reject a tenure-start block that fails either check.

## Acceptance Criteria

- Every captured tenure-start block passes verification.
- A block with a tampered proof or seed is rejected with a distinct error.
- The fixture replay still reports depth 600/600.

## Remaining

The rules are `nano_chainstate::{verify_coinbase_vrf_proof,
verify_committed_vrf_seed}` and every captured tenure is checked against both.
Nothing calls them on the follow path yet.

Wiring needs the tenure's sortition hash, and taking that from a peer would
mean trusting the peer for a validation input. It has to come from nano's own
`SnapshotChain`, which carries `sortition_hash` already, so this lands once
[[026-survive-a-bitcoin-reorganization]] settles the snapshot chain's shape.

Checking the committed seed also needs the parent tenure's coinbase proof,
which means the validator has to retain the proof of each tenure it accepts.

While proving the rules against the capture: several miners commit the same
block header hash in one Bitcoin block, so a sortition winner is identified by
its transaction, never by what it committed to. `nano-miner`'s
`hacknet_sortition_hash_verifies_the_winning_vrf_proof` still matches on the
committed hash and should move to the txid.

## What the wiring needs, having looked

`026-survive-a-bitcoin-reorganization` has settled, so the snapshot chain's shape
is no longer the blocker. Three inputs are, and only one of them exists today:

- **The tenure's sortition hash.** `SortitionSnapshot::sortition_hash` carries it
  already, from nano's own burnchain. Nothing has to be asked of a peer.
- **The winning leader key's VRF public key.** Not carried anywhere yet.
  `SortitionSnapshot` records `winner_txid` but not the key, and resolving one
  means following the winning block-commit's `key_block_ptr`/`key_vtxindex` to
  its leader-key registration — which the local sortition derivation already
  reads, so it is a matter of retaining it rather than of finding it.
- **The parent tenure's coinbase proof.** The validator has to keep the proof of
  every tenure it accepts, and a node starting from a checkpoint has no proof for
  the tenure before its first — so the checkpoint has to carry that one, or the
  first tenure's seed check has to be explicitly skipped once and said out loud.
  Skipping it quietly is the failure mode this whole group of tasks is about.

That points at `BitcoinBlockContext` gaining `sortition_hash` and
`winner_vrf_public_key`, and `ChainState` retaining the accepted proof. Both are
validation-only inputs — Clarity reads none of them — so neither moves a state
root, which makes this safe to land against a running replay. Six construction
sites for the context, plus the checkpoint field.

Also still true from the note above: `nano-miner`'s
`hacknet_sortition_hash_verifies_the_winning_vrf_proof` matches on the committed
block header hash, and several miners commit the same hash in one Bitcoin block,
so it should match on the txid.

## The check is on the follow path now

`ChainState::check_tenure_vrf` runs before anything executes, on every block that
starts a tenure, and a failure rejects the block rather than warning about it.
`tenure_vrf_enforcement` goes through
`append_nakamoto_block_with_bitcoin_operations` and asks what the chainstate
*does*: the tenure the network accepted is accepted with its own leader key, a
proof from another miner's key is rejected, a key that is not a curve point is
rejected, and an unknown key accepts the block while saying on stderr that it
could not be checked.

The two inputs are carried as validation-only fields — `sortition_hash` and
`winner_vrf_public_key` on `BitcoinBlockContext` — so no Clarity word reads them
and neither moves a state root. `SortitionTracker` now keeps a `LeaderKeys` and
registers every `LeaderKeyRegistration` it walks past, registrations before
commitments so a commitment naming a key from its own burn block resolves. The
accepted tenure's coinbase proof is retained in `ChainState` and rolled back with
everything else if the block is not accepted.

**Two inputs can be absent, and they are reported rather than skipped.** A leader
key registered before the burnchain window this node holds is unresolvable, which
is ordinary for the first tenures after a checkpoint; and the parent tenure's
proof is unknown for the very first tenure. Both print what they could not check
and why. That is the honest state, not a closed hole: a node in that window is
accepting proofs it has not verified, and it says so.

## The sortition hash is local now; the leader key is not

The plumbing landed with
[[049-derive-canonical-sortitions-from-the-local-burncha]]. `SortitionTracker` now
keeps pace with the block being executed — it walks every burn block between its
tip and that block's burn view, bounded at a day of Bitcoin a round — and
`nano-node`'s `CheckpointExecutor::local_sortition` fills
`BitcoinBlockContext::sortition_hash` from the snapshot it derived rather than from
the peer. The sortition hash derives exactly for all fourteen blocks of the
captured mainnet window and matched at every burn block of a live follow.

`winner_vrf_public_key` is still usually `None`, and the reason is now a precise
one rather than "the registration predates the window". Every eligible commitment
in a Nakamoto burn block carries the same `new_seed` and a *different* leader key —
burn 960,230 has five commitments, five keys, one seed — so naming the key means
naming which commitment won, which is the burn distribution's answer, and that
derives 12 of the captured 14. The node therefore publishes the key only where the
burn block leaves no choice (one eligible commitment) and otherwise says how many
competed. Publishing a 12-in-14 answer here would reject one valid tenure in
seven, since `check_tenure_vrf` rejects rather than warns.

So the remaining work on this task is not plumbing any more, it is
`make_burn_sample`'s min-median weighting — see 049's "What the winner still
needs". When that closes, the key follows with no further wiring.

Two smaller things this turned up:

- `check_tenure_vrf`'s message for an absent key still says the registration
  predates the burnchain window, which is no longer the usual reason. It should say
  the winner is unnamed when that is what happened.
- `context.vrf_seed` is still the peer's, and it is a validation input as well as a
  Clarity-visible one: `verify_committed_vrf_seed` reads it. The locally derived
  `winner_vrf_seed` matched the peer's at all fourteen captured blocks and at every
  block of the live follow, so switching is a small step — but it moves a state
  root, so it wants its own change rather than riding along with a
  validation-only one.

## Correction: the parent proof is lost on every restart, not only at a checkpoint

The note above says the parent tenure's proof "is unknown only for the very first
tenure after a checkpoint, because every later one is retained here". That is
wrong. `parent_tenure_proof` lives only in memory — like `tenure_start_heights`
and the executed-block list, it is never written to disk. So **every restart**
loses it, and the committed-seed half of the check silently cannot run for the
first tenure after each one, not merely after an import.

Found while making a rejected block's rollback structural
([[057-commit-and-recover-accepted-block-state-atomically]]), which is where the
fix belongs: persist the whole ledger with the seal rather than beside it. Until
then a restarting node reports that it cannot check the seed, which is at least
audible — but the window is far wider than this task claimed.

## Fixed: the parent proof is committed with the block that produced it

`ChainLedger` is now written down in the same transaction that commits the seal
([[057]]), so `parent_tenure_proof` comes back with the tip and the committed-seed
half of the check runs from the first tenure after a restart. The window is again
what this task originally claimed — the first tenure after a *checkpoint* — plus
one more, honestly named: the first tenure after resuming a state directory
written before ledgers were committed, which has none to read. Both messages now
say so.

The other message was wrong too and is corrected. It said an absent leader key
meant the registration predated the burnchain window this node holds; the real
reason, since [[049]], is that the node could not name which of the burn block's
commitments won, which needs the burn distribution's min-median weighting. That
remains the last thing this task needs, and it is 049's work rather than this
one's.

Still true and untouched: `nano-miner`'s
`hacknet_sortition_hash_verifies_the_winning_vrf_proof` matches on the committed
block header hash where it should match on the txid.

## The other half of the leader key: what it signs with

A leader-key registration binds two things — the VRF public key this task resolves
and a block-signing `Hash160`. The first says who may produce the tenure's
coinbase proof; the second says who may sign its blocks. Both come out of the same
registration, and resolving one is resolving the other.

So [[050]] added the second rule beside these:
`nano_chainstate::verify_miner_signature` against
`registered_signing_key_hash(operations, winner_vrf_public_key)`, keyed by VRF key
because the burnchain refuses a VRF key that is already registered, so one key
names one registration. It runs on the follow path, on every tenure-start block,
and rejects rather than warns.

The capture supplies an oracle that makes it more than self-consistent:
`tenure_vrf_enforcement::the_winning_registration_names_the_key_that_signed_the_tenure`
asserts that **the block-signing hash the winner registered on Bitcoin is the
`Hash160` of the key that signed the first block of the tenure it won**. One side
comes out of a Bitcoin transaction and the other out of a header signature; nano
is asked for neither. That is the chain stating what the rule may assume.

Two more things fell out of the same reading:

- A tenure change has to name the miner that signed the header it travels in
  (`check_tenure_tx`). That needs no burnchain input at all, so it is enforced
  unconditionally — and it is what stops a tenure change being lifted out of a
  competing miner's block.
- The registration a sortition resolves through is usually *not* in the burn block
  the tenure sits in: a leader key is registered once and reused. So the signing
  hash has to travel with the sortition, the way `sortition_hash` and
  `winner_vrf_public_key` already do — one 20-byte field on `SortitionSnapshot`
  and one on `BitcoinBlockContext`, plus a line in `nano-node`'s
  `local_sortition`, which resolves the registration already. Until then the node
  reports, once a tenure, that it knows which key won and not what that key signs
  with.

Still true and still outside this task's files: `nano-node` keeps the
`(candidates == 1)` hedge around `winner_vrf_public_key` that [[049]] made
obsolete, so in production the winner is usually `None` and both the proof check
and the signature check report rather than run. The hedge is a one-line removal.

And still true from the note above: `nano-miner`'s
`hacknet_sortition_hash_verifies_the_winning_vrf_proof` matches on the committed
block header hash where it should match on the txid.
