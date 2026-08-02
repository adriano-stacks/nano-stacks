---
id: "054"
title: "Join and synchronize over the Stacks P2P network"
status: pending
priority: critical
effort: large
type: feature
group: mainnet
dependencies: ["027"]
tags: ["mainnet", "p2p", "sync", "networking"]
created_at: 2026-08-02
---

# Join and synchronize over the Stacks P2P network

## Objective

The production runtime currently selects the first HTTP peer that answers and
uses its Stacks RPC for every block, tenure, sortition, mempool and signer data
request. Pointing that client at `api.mainnet.hiro.so` makes a hosted service's
rate limit, availability and view of the chain part of nano's liveness.

Join the canonical Stacks peer network directly. Nano must discover multiple
independent peers, exchange and serve protocol data, and feed locally validated
candidates into the fork-choice and execution paths from
[[027-choose-a-fork-instead-of-following-a-peer]]. HTTP may remain as an
operator-selected bootstrap or diagnostic source, but neither catch-up nor
steady-state operation may require a hosted Stacks API.

## Tasks

- [ ] Reuse a vetted Stacks wire codec and message definitions where possible;
      document every protocol/version compatibility boundary that remains in
      nano.
- [ ] Implement the mainnet handshake, framing, network and chain checks,
      liveness messages, neighbor discovery and persistent peer database.
- [ ] Maintain bounded outbound and inbound peer sets with connection limits,
      retry backoff, scoring and isolation of malformed or dishonest peers.
- [ ] Exchange inventories and acquire Nakamoto blocks, tenures and required
      sortition data from multiple peers without making any one peer a
      consensus input.
- [ ] Persist enough authenticated canonical block data to answer peer inventory
      and block requests after a restart.
- [ ] Relay locally accepted transactions and blocks, and carry the signer and
      StackerDB messages required by the enabled node roles.
- [ ] Feed all received data through the local burnchain, signer, miner, VRF,
      transaction and state-root checks before fork choice or relay.
- [ ] Make peer disconnects, slow peers, duplicate inventory, invalid messages,
      ordinary forks and bounded network queues non-fatal and observable.
- [ ] Interoperate with stock `stacks-node` peers in deterministic integration
      tests, including restart, reorganization and one malicious peer.
- [ ] Document seed configuration, advertised/listen addresses, resource
      limits, peer database recovery and the optional HTTP fallback.

## Acceptance Criteria

- Starting from the attested checkpoint, nano catches up to and holds mainnet
  tip with all hosted Stacks HTTP APIs disabled.
- Removing, stalling or lying through one peer does not stall synchronization,
  select a different canonical chain or corrupt durable state.
- A stock `stacks-node` can complete the handshake with nano, exchange
  inventory and blocks in both directions, and receive a relayed transaction.
- Restarting preserves the validated chain and usable peer knowledge without
  redownloading sealed blocks.
- The P2P implementation has bounded memory, disk and connection use, rejects
  wrong-network and malformed messages, and passes `clippy` without warnings.
- The mainnet release gate records no dependency on Hiro or another hosted
  Stacks API for synchronization, propagation or consensus inputs.
