---
id: "140"
title: "Extract the Epoch 4.0 consensus firewall"
status: blocked
priority: high
effort: large
dependencies: ["135", "136", "142"]
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
- [x] Extract an `epoch4-consensus` API with no Tokio, HTTP, sockets, wall clock,
      environment reads or arbitrary filesystem access in its decision path.
- [x] Accept only authenticated Bitcoin views, typed block candidates and parent
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

## Dependency corrected, 2026-08-24

This task depended on **138**, which was cancelled on 2026-08-23 because the
project has one operator. A dependency on a cancelled task can never be satisfied,
so taskmd would have held this blocked however much of the work landed — not a
judgement about readiness, just a broken edge.

Re-pointed to **142**, which is where 138's substance actually went: 142 inherited
the "one continuous interval spanning a prepare phase, the rollover and the
complete following cycle" requirement verbatim, and its acceptance criteria name
the single-operator assumption as a residual rather than pretending to independent
corroboration. That is the interval this task's shadow-mode comparison needs, so
the edge now points at the thing that supplies it.

The intent is unchanged: this remains sequenced *after* the follower qualifies, as
its own objective says.

## Measured status, 2026-08-23

The checkboxes above understated what exists, so this records what is in the tree
rather than what was planned. `crates/epoch4-consensus` is 758 lines across
`lib.rs`, `request.rs`, `host.rs` and `src/bin/epoch4-executor.rs`.

**The executor process and its protocol exist.** `epoch4-executor` is a 48-line
shell over `host::serve` that takes a state directory and a network and hands over
stdin and stdout: "one process, one state directory, no listener and no client
capability." The protocol is versioned by schema strings (`STAND_SCHEMA`,
`READY_SCHEMA`, `PROTOCOL_ERROR_SCHEMA`), bounded by `MAX_LINE_BYTES` with a
`Line::TooLong` refusal answered as `a protocol line exceeds the bounded
maximum`, and authenticated by construction rather than by a token, since a pipe
has exactly one writer and the process opens no socket. Commit `35dad1cd` proves
the decision boundary equal in and out of process.

**Authority has not moved, which is why the fourth box stays unticked.** Nothing
in `nano-node` spawns `epoch4-executor`; the only caller is
`nano-conformance/src/bin/epoch4-shadow-executor.rs`. The production node is still
its own chainstate writer, so "the sole chainstate writer" describes the binary's
design and not the deployment. What remains is unchanged: move write authority
behind the boundary, run P2P/RPC/optional roles without write permission, compare
every decision in shadow mode first, and measure throughput, catch-up latency and
restart behaviour before removing the in-process path.

**A dependency that could never be satisfied**, since corrected above: this task
listed `138`, which is cancelled, so taskmd would have held it blocked no matter
how much of the work landed. The full-cycle shadow comparison it needs is the same
interval 142 inherited from 138, so the edge now points there.

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
