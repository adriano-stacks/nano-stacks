---
id: "071"
title: "Fail over signer-role replication across peers"
status: pending
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

- [ ] Inventory every long-lived signer, miner, StackerDB and proposal-recovery
      client and identify which retain one endpoint after discovery starts.
- [ ] Route StackerDB pull/push replication through the bounded scored peer pool
      or an equivalent discovery handle, with bounded retry and rotation.
- [ ] Route proposal-validator recovery through the same authenticated pool
      rather than a separately pinned HTTP client.
- [ ] Preserve chunk-signature, slot-writer and canonical-chain validation when
      changing serving peers; failover must not turn availability into trust.
- [ ] Add deterministic tests for disconnect, timeout, 429, malformed chunks,
      stale replicas and an equivocating first peer.
- [ ] Record which peer served each replication/recovery operation and expose a
      bounded failure counter so the no-hosted-API run can prove distribution.
- [ ] Run the hosted signer with no configured Hiro endpoint, remove its active
      peer, and show proposals and signed chunks continue through another
      discovered peer.

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
