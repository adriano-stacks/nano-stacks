---
id: "053"
title: "Pass the mainnet node release gate"
status: in-progress
priority: critical
effort: medium
type: improvement
group: mainnet
dependencies: ["027", "037", "049", "050", "051", "052", "054", "056", "057", "058", "060"]
tags: ["mainnet", "conformance", "release"]
created_at: 2026-08-02
---

# Pass the mainnet node release gate

## Objective

No component result or peer-facing height is enough to call nano a mainnet node.
Exercise the assembled binary from a fresh, attested checkpoint through catch-up
and steady state, with evidence tied to the durable executed chain.

## Tasks

- [ ] Bootstrap a clean state directory from an attested mainnet checkpoint.
- [ ] Start with clarity-wasm as the production execution engine, with
      interpreter fallback and healing disabled for the whole run.
- [ ] Catch up using a local Bitcoin source and multiple Stacks peers while
      recording every executed height and verified root.
- [ ] Restart during catch-up and at tip, then prove the same durable tip, root
      and tenure accounting are resumed.
- [ ] Inject failure and hard process termination at every block commit boundary
      and prove recovery exposes no partially committed block.
- [ ] Retry a rejected block repeatedly and prove no durable or in-memory state
      changes before the accepted replacement arrives.
- [ ] Remove and lie through one Stacks peer and prove neither event changes the
      canonical executed result.
- [ ] Exercise a Bitcoin reorganization and a Stacks fork switch.
- [ ] Run the stock signer/client-facing RPC and an event observer against the
      same executed chain.
- [ ] Hold mainnet tip for at least 24 hours across tenure and Bitcoin boundaries.
- [ ] Publish the exact commands, versions, checkpoint provenance and resulting
      conformance report.

## Acceptance Criteria

- Offline mainnet replay and receipt gates are green before the live run starts.
- Every required mainnet test reports that it actually ran; a missing fixture or
  environment variable cannot be reported as a passing conformance gate.
- The executed tip, not the followed tip, remains within the documented sync
  bound and survives restart.
- Every accepted block passed local burnchain, signer, miner, VRF and state-root
  validation.
- Peer failure, peer equivocation and ordinary reorganization do not stall or
  fork the node.
- RPC responses and events describe the same durable executed state.
- Synchronization, propagation and consensus inputs do not require Hiro or any
  other hosted Stacks HTTP API.
- The release configuration executes consensus through clarity-wasm and cannot
  hide a compiler divergence by retrying under the interpreter.

## A skipped gate can no longer report itself green

Most mainnet tests need a capture or a node's state directory, and skipping when
those are absent is right for a working tree. It is exactly wrong for a release
gate: **a suite where every mainnet test skipped looks identical to one where
every mainnet test passed**, and that difference is the whole question this task
asks.

`nano_conformance::skip_gate` is now what every one of them calls instead of
printing and returning. It prints the same thing normally, and panics when
`NANO_REQUIRE_MAINNET` is set — so a run that claims the mainnet gates are green
had to actually run them.

Demonstrated both ways, which is the point:

```
NANO_REQUIRE_MAINNET=1                          -> FAILED. 0 passed; 2 failed
NANO_REQUIRE_MAINNET=1 NANO_MAINNET_CAPTURE=... -> ok. 2 passed
(neither set)                                   -> ok, skipped
```

Seven gates across six files route through it. The remaining items on this task
need the live run they describe.
