---
id: "071"
title: "Fail over signer-role replication across peers"
status: in-progress
priority: high
effort: medium
dependencies: []
tags: ["mainnet", "p2p", "signer", "reliability"]
created_at: 2026-08-06
type: improvement
---

# Fail over signer-role replication across peers

## Objective

Remove the remaining single-peer liveness dependency from signer-facing roles.
Chain synchronization now has a bounded peer pool, but hosted StackerDB
replication clones one initially selected `SyncClient` and loops on it forever;
proposal-validator recovery has the same shape. In the observed mainnet run the
selected replication client was Hiro, so chain sync could survive Hiro loss
while the hosted signer silently could not.

## Tasks

- [x] Inventory every long-lived signer, miner, StackerDB and proposal-recovery
      client and identify which retain one endpoint after discovery starts.
      See *What was holding one endpoint* below.
- [x] Route StackerDB pull/push replication through the bounded scored peer pool
      or an equivalent discovery handle, with bounded retry and rotation.
- [x] Route proposal-validator recovery through the same authenticated pool
      rather than a separately pinned HTTP client.
- [x] Preserve chunk-signature, slot-writer and canonical-chain validation when
      changing serving peers; failover must not turn availability into trust.
- [x] Add deterministic tests for disconnect, timeout, 429, malformed chunks,
      stale replicas and an equivocating first peer.
      `conformance/replication_failover.rs`. "Timeout" is served as a peer that
      refuses the connection and as one that answers `500`: neither client sets a
      request timeout, so a test that waited for one would wait forever and prove
      nothing about the rotation.
- [x] Record which peer served each replication/recovery operation and expose a
      bounded failure counter so the no-hosted-API run can prove distribution.
      `Replicas::distribution()` answers `(peers that served, rounds that failed)`
      and the loop now *says* it whenever either number moves -- a pool that was
      never asked twice reads identically to one peer doing all the work, so the
      counters have to leave the process to be evidence. `TenureSource::last_served()`
      names the peer behind each pooled request.
- [~] Run the hosted signer with no configured Hiro endpoint, remove its active
      peer, and show proposals and signed chunks continue through another
      discovered peer.
      **The no-hosted-API half is done and measured.** A mainnet follower ran with
      `peers = []` and no hosted endpoint anywhere in its configuration, joined over
      p2p alone, and replicated StackerDB over a pool that grew from three peers to
      five -- `108.130.44.244`, `117.52.250.3`, `152.53.22.28`, `172.96.141.17`,
      `172.96.141.52` -- with zero rounds unanswered over the run.
      What is left is the *removal*: no peer failed while it was watched, so the
      rotation was never exercised in the field. Forcing it -- dropping the serving
      peer and showing the next round go elsewhere -- is the remaining evidence, and
      `replication_failover.rs` already pins the same behaviour offline six ways.

## What was holding one endpoint

Measured by reading every long-lived client the runtime hands out, not by
grepping for the symptom:

| Role | Held | Now |
|---|---|---|
| `hosting::replicate` | one `StackerDbClient` built from the startup peer, forever | `Replicas`: one client per discovered endpoint, the turn moved by a round that was not answered |
| `hosting::validate_proposals` | `TenureSource::only(peer)` — a pool of one, by construction | the same pool the chain is followed over, refreshed from `Discovered` |
| `signer::catch_up` | `&SyncClient` | `&mut TenureSource` |
| `signer::run` → `binding` | the startup peer's `/v3/tenures/info` and `/v3/stacker_set` | the pool |
| `signer::run` → `SignerService`, `StateAnnouncer`, `LiveSigner` | one `StackerDbClient`/`SyncClient` each, built once | retargeted from `Replicas` whenever the turn moves |
| `miner::run` | the startup peer | **unchanged**, and out of scope here: a miner that cannot reach a peer mines nothing, which is a liveness cost to itself and not to the signers of the network |
| follower / bulk history | already a `TenureSource` rebuilt from discovery | unchanged |

## Why a round and not a request

Chain history is fetched a tenure at a time and every answer is
content-addressed, so `TenureSource` spreads it per *request*. StackerDB
replication is a conversation — the slot listing, then the chunks it says are
newer — and switching peers between those two would compare one peer's listing
against another's chunks. So the peer is chosen per round, and a round that goes
unanswered anywhere moves the turn on.

Rotation is not trust, and the distinction is asserted rather than asserted-to:
a chunk this node *refuses* leaves the turn where it is. Rotating on a refusal
would let one peer serving forgeries walk a node off every honest peer it has,
one round each, which would make the pool a liability. A chunk the hosted signer
wrote also survives the round that failed — it is a signature the network is
counting, and the peer whose turn it happened to be going away is no reason to
lose it.

## Acceptance Criteria

- No signer-facing background loop is permanently bound to the first
  `SyncClient` selected at startup.
- Removing or rate-limiting that peer does not stop StackerDB convergence or
  proposal validation while another honest peer is available.
- Invalid chunks and consensus context remain rejected across failover.
- The release configuration contains no hosted API, and the retained log names
  more than one serving peer over the run.
- Peer loss uses bounded memory, queues and backoff and passes `clippy` without
  warnings.

## Evidence that opened this task

The mainnet log says `replicating StackerDB chunks with
https://api.mainnet.hiro.so/` even though seven P2P-discovered peers were
available to the synchronizer. This does not block ordinary following today,
but it makes the embedded/hosted signer role depend on the service the P2P work
is meant to make optional.
