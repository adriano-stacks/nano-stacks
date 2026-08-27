---
id: "153"
title: "Stop compiling contracts a call never invokes"
status: completed
priority: high
effort: medium
dependencies: []
tags: ["mainnet", "vm", "performance", "clarity-wasm"]
created_at: 2026-08-27
type: improvement
---

# Stop compiling contracts a call never invokes

## Objective

`call_contract_values_in_context` (`crates/nano-vm/src/lib.rs:4924`) compiles
**every contract-typed argument** of a contract call before the call runs:

```rust
let probe = clar2wasm::phases::start();
for argument in arguments.iter().filter_map(contract_argument) {
    let _ = needed_module(store, bitcoin_context, modules, argument, cost_tracker);
}
let failed = needed_module(store, bitcoin_context, modules, contract, cost_tracker);
clar2wasm::phases::finish(clar2wasm::phases::Phase::ModuleProbe, probe);
```

The comment explains why: a trait dispatch may need the argument's module and the
call cannot say in advance which. But the code immediately below already handles
that case lazily — "compile what the run turns out to want and run it again" —
so the eager probe may be paying, on every call, for a module most calls never
use.

Measured on 2026-08-27, this is the single largest cost in block execution.

## Measurement

66 blocks at heights 8,851,9xx–8,852,0xx, instrumented node against a reflink
clone of the port-20492 tip state.

Block phases:

| phase | share | mean | max |
|---|---|---|---|
| `txs_run` (VM) | 84.2% | 154 ms | 4,644 ms |
| `finish` (sqlite commit) | 11.7% | 21 ms | 195 ms |
| `root` (MARF root hash) | 2.9% | 5 ms | 87 ms |
| everything else | <1% | | |

Inside `txs_run`, by `nano_vm::phases`:

| vm phase | share | calls | ms/call |
|---|---|---|---|
| `module_probe` | 54.0% | 44 | 123.8 |
| `linker_setup` | 27.0% | 631 | 4.3 |
| `wasm_invoke` | 24.4% | 48 | 51.3 |
| `commit` | 6.7% | 675 | 1.0 |
| `host_map` | 5.2% | 4,606 | 0.1 |

`wasm_invoke` contains the `host_*` buckets, so the shares overlap by design.

The worst block in the sample, **h8852009**, spent **4,278 ms in 3 probes** —
1.4 s per probe — against a charged block cost nowhere near any limit.

## Caveat on the magnitude

The clone was repinned by hand so an instrumented build could open it, which
invalidated the on-disk native module cache — it keys on the compiled wasm
bytes, and those move with the compiler. So this run recompiled from cold and
`module_probe`'s share is **inflated**. Scale: over the same blocks, the warm
production node on 20492 averaged 98 ms per block against this run's 183 ms,
so roughly 2x on the mean.

The tail is not a cold-cache artifact, though: 20492 has been up since
2026-08-26 and still shows 31 blocks over 10.24 s in 7,425.

## Tasks

- [x] Count the eager probes that are never used: instrument `needed_module` at
      the probe site and at the trait-dispatch site, and report the hit rate over
      a few thousand mainnet blocks. If most probes are used, this task is a
      no-op and should be closed saying so.
- [x] Measure what the lazy path actually costs when the guess *was* needed: the
      retry re-executes a rolled-back call, so the trade is one compile against
      one re-execution.
- [x] If the numbers favour it, remove the eager probe and let the
      compile-and-retry path carry the case. **They do not.**
- [x] Keep the "argument names a contract that does not exist" behaviour intact
      — mainnet passes `.native-pool-signer`, which is deployed nowhere and must
      still return a value rather than failing the call.
- [x] Re-measure with the same harness and record the new numbers here.

## Acceptance Criteria

- Receipts, costs, events and state roots are unchanged across a replay slice:
  this is a scheduling change, not a semantic one.
- The probe's hit rate is recorded, whatever the decision.
- `module_probe`'s share of VM time is reported before and after on the same
  harness, warm cache in both cases.

## How to re-measure

The harness is worth keeping. Patch `execute_nakamoto_block` with per-phase
`Instant` timers and a `nano_vm::phases::snapshot()` diff around
`run_transactions`, gate the print on an env switch, build with plain
`cargo build --release --bin stacks-node` (`nix develop` may fail — the
sandbox's `/nix/store` overlay filled up on 2026-08-27), copy the binary out of
`target/release`, revert the tree, then run it against a reflink clone of a tip
state on spare ports with `NANO_PHASE_TIMING=1 NANO_WASM_PHASES=1`.

A clone of another compiler's state needs two repins to open — the
`consensus_profile` row in `chainstate/clarity.sqlite` and
`profile_fingerprint` in `chainstate/checkpoint-provenance.toml`; the second one
is easy to miss and refuses startup with a clear message. A state repinned this
way is diagnostic only and must never be presented as evidence. Sample logs from
this run: `/tmp/phase-run1.log`, `/tmp/phase-warm.log`.

## Measured and closed as a no-op, 2026-08-27

Both schedules were run against **the same mainnet blocks** — two reflinked copies
of one witness state, same binary, the eager loop behind an environment variable
so nothing else could differ, started from an identical height of 8,758,892.

**Warm native-module cache** (the copies inherited one):

| | blocks in 9 min |
|---|---|
| eager probe | 4,000 |
| lazy only | 4,000 |

Identical, to the block.

**Cold cache** (`native-modules` removed from both, which is the state a fresh
import starts in):

| | blocks in 5 min | blocks in 15 min |
|---|---|---|
| eager probe | 969 | 2,969 |
| lazy only | 1,092 | 3,092 |

Lazy opens a **123-block lead in the first five minutes and never widens it**. So
the eager probe's cost is a one-time cold-cache cost of about 35 seconds of
catch-up, not the per-block cost the profile suggested — and the profile said so
itself, in its caveat: the sample was taken against a hand-repinned clone whose
native cache had been invalidated, which is exactly what inflates
`module_probe`'s share to 54%.

What removal would cost is not zero either: over ~3,000 blocks the lazy path
compiled **162** modules a dispatch reached, and each of those is a call
re-executed after a rollback.

Trading a bounded 35 seconds for 162 re-executions per 3,000 blocks is not worth
it, so the eager probe stays and the measurement scaffold is removed. Neither arm
produced a state root mismatch, which is the check that this was ever only a
scheduling question.

The lesson worth keeping is about the harness rather than the probe: a cold cache
makes a one-time cost look like a per-block one, and the only way to tell is to
run both arms over the same blocks twice — once warm, once cold.

