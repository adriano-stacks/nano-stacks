---
id: "154"
title: "Make native code reuse survive a call frame"
status: pending
priority: high
effort: medium
dependencies: []
tags: ["mainnet", "vm", "performance", "clarity-wasm"]
created_at: 2026-08-27
type: improvement
---

# Make native code reuse survive a call frame

## Objective

Every Clarity call frame asks for its contract's `InstancePre`:

```rust
// vendor/clarity-wasm/clar2wasm/src/initialize.rs:754
let instance_pre = crate::phases::time(crate::phases::Phase::LinkerSetup, || {
    module.instance_pre(module_cache)
})?;
```

`CompiledContract::instance_pre` (`vendor/clarity-wasm/clar2wasm/src/lib.rs:132`)
memoizes in a `OnceCell`, so a hit is nearly free. A miss calls
`self.native(cache)` — fetching native code, which means deserializing an
on-disk entry or compiling the contract — then builds the host linker and
`instantiate_pre`s.

Measured on 2026-08-27 that phase cost **2,724 ms over 631 frames, 4.3 ms
mean, with a single frame reaching 77 ms**, for 48 outermost contract calls.
A hit cannot cost 4.3 ms, so the memo is missing far more often than "once per
contract per process" would predict. Meanwhile `Instantiate` — the step people
assume is expensive — is **0.024 ms/call**, and `contract_load` is 0.83 ms.

So the question is not how to make instantiation cheaper. It is why a
per-contract memo does not hold across the ~13 frames of a single call, let
alone across blocks.

## Measurement

Same run as [[153]]: 66 blocks at 8,851,9xx–8,852,0xx, instrumented node on a
reflink clone of the port-20492 tip state. Full phase tables are in task 153;
the two lines that matter here:

| vm phase | share of `txs_run` | calls | ms/call |
|---|---|---|---|
| `linker_setup` (`instance_pre`) | 27.0% | 631 | 4.3 |
| `instantiate` | 0.1% | 631 | 0.024 |

Per-block spread, which is the interesting part:

- h8852044: `linker_setup` 606.8 ms over 20 frames — **30 ms per frame**
- h8852039: 926.5 ms over 12 frames — **77 ms per frame**
- h8852009: 233.0 ms over 365 frames — **0.64 ms per frame**

That spread says the cost is misses, not a fixed per-frame overhead, and that
some misses are very expensive.

Taken with 153, roughly **80% of VM wall time goes to obtaining compiled native
code rather than running it.** That is the headline, and it is the cost an
interpreter structurally never pays — which is the most likely reason a nano
block seals slower than a stacks-core one.

## Tasks

- [ ] Instrument the miss: count `OnceCell` misses separately from hits, and
      split a miss into on-disk deserialize versus full compile. Without that
      split the 4.3 ms mean cannot be acted on.
- [ ] Find out how long a `CompiledContract` lives. If a frame gets a fresh
      object rather than the cached one, the memo is defeated by construction
      and the fix is ownership, not caching.
- [ ] Check the `ModuleCache` eviction policy against mainnet's working set. The
      20492 node holds 539 modules in 268 MB; if the set of contracts a tenure
      touches exceeds the cap, misses recur forever and a bigger cap is the
      cheapest fix available.
- [ ] Confirm the on-disk cache is actually being read on a warm start. A key
      derived from compiled wasm bytes plus `ENGINE_CONFIG_ID`
      (`crates/nano-wasm-cache/src/lib.rs:103`) is correct but unforgiving: any
      compiler change orphans every entry, and a node that silently recompiles
      everything looks exactly like a node with a small cache.
- [ ] Fix whichever of the above the measurement indicts, then re-measure with
      the harness in 153.

## Acceptance Criteria

- Hit and miss counts for `instance_pre` are reported, with misses split into
  deserialize and compile.
- `linker_setup`'s share of VM time is reported before and after on the same
  harness with a warm cache in both cases.
- Receipts, costs, events and state roots unchanged across a replay slice.

## Sequencing

Unlike [[153]], a fix here probably lands in `vendor/clarity-wasm` — which moves
`COMPILER_IDENTITY`, invalidates the checkpoint attestation and forces a fresh
import rather than a restart. Batch it with any other compiler edit immediately
before a re-issue ceremony, never during a hold or replay run. See [[151]] for
the same constraint on a much smaller change.

If the indicted code turns out to be the `ModuleCache` cap or the on-disk cache
in `crates/`, none of that applies and it can land on its own.
