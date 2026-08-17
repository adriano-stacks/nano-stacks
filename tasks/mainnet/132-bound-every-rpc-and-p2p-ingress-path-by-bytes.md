---
id: "132"
title: "Bound every RPC and P2P ingress path by bytes"
status: in-progress
priority: critical
effort: large
dependencies: ["063"]
tags: ["mainnet", "rpc", "p2p", "security", "liveness"]
created_at: 2026-08-14
parent: 053
type: improvement
---

# Bound every RPC and P2P ingress path by bytes

## Objective

Give every peer- or client-controlled byte a local memory, concurrency and time
budget before mainnet. Valid but excessive traffic must produce backpressure or
load shedding instead of an unbounded queue, allocation or task population.

## Tasks

- [x] Inventory RPC bodies, P2P frames, decoded messages, proposal/block/chunk/
      transaction channels, peer push buffers, event observers and spawned
      per-request work. Record their present count and byte bounds.
- [x] Replace externally fed unbounded channels with bounded, byte-accounted
      queues. Share one small queue abstraction and expose current bytes, age,
      drops and saturation.
- [x] Add route-specific body, concurrency and rate limits. Return an explicit
      overload response without reporting admission or changing consensus state.
- [x] Bound P2P memory per session, per address and globally; enforce the budget
      before allocating or decoding the advertised payload.
- [x] Bound pushed messages by bytes rather than only by message count and test
      the maximum-size-message case across all allowed sessions.
- [x] Limit slow reads, fragmented frames, decomposed/hex-expanded bodies and
      expensive read-only calls independently from consensus execution.
- [ ] Add load and slowloris tests using authenticated valid traffic as well as
      malformed traffic. Exercise recovery after every queue saturates.

## Acceptance Criteria

- No externally reachable production path feeds an unbounded channel or permits
  peer-selected aggregate memory.
- Declared per-route, per-peer and global byte budgets are enforced before large
  allocation and reported through metrics.
- Saturation cannot acknowledge work that was dropped, expose staged data, stall
  block execution indefinitely or alter block validity.
- RSS and task counts plateau under sustained maximum-rate test traffic, and the
  node returns to normal service without restart after the load stops.
- Codec, RPC, P2P, conformance and strict Clippy gates pass.
