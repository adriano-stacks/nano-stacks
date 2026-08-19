---
id: "140"
title: "Extract the Epoch 4.0 consensus firewall"
status: in-progress
priority: high
effort: large
dependencies: ["135", "136", "138"]
tags: ["mainnet", "consensus", "architecture", "isolation"]
created_at: 2026-08-14
type: improvement
---

# Extract the Epoch 4.0 consensus firewall

## Objective

After the hardened follower qualifies, isolate deterministic Epoch 4.0 decisions
and chainstate write authority from network, RPC and optional-role failures. This
is a follow-on architecture change, not permission to rewrite the qualified
consensus rules in place.

## Tasks

- [x] Write an architecture decision describing the process/crate boundary,
      deterministic inputs and outputs, failure semantics and migration plan.
- [ ] Extract an `epoch4-consensus` API with no Tokio, HTTP, sockets, wall clock,
      environment reads or arbitrary filesystem access in its decision path.
- [ ] Accept only authenticated Bitcoin views, typed block candidates and parent
      state references; return a canonical decision record containing verdict,
      writes, root, receipts, costs and effects.
- [ ] Host the API in a separately supervised executor process that is the sole
      chainstate writer. Use a versioned, bounded and authenticated local protocol.
- [ ] Run P2P, public RPC and optional roles without chainstate write permissions;
      prove their crash, compromise or restart cannot mutate committed state.
- [ ] Introduce the boundary first in shadow mode and compare every decision with
      the qualified in-process follower before switching authority.
- [ ] Measure throughput, catch-up latency and restart behavior; retain no
      fallback to the old executor after migration.

## Acceptance Criteria

- The consensus decision API is deterministic from serialized inputs and passes
  the complete Epoch 4.0 corpus in and out of process.
- Only the executor process can write chainstate, and it has no network listener
  or client capability.
- Killing or saturating every edge/optional process neither corrupts state nor
  changes the next accepted decision after recovery.
- Shadow and authoritative modes produce identical decision records throughout a
  full reward cycle before the old path is removed.
- Strict Clippy, fault injection, conformance and performance bounds pass.
