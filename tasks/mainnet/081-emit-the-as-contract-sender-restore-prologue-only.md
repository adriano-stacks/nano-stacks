---
id: "081"
title: "Emit the as-contract sender-restore prologue only for functions that use as-contract"
status: in-progress
priority: medium
effort: small
dependencies: []
tags: ["mainnet", "vm", "clarity"]
created_at: 2026-08-07
---

# Emit the as-contract sender-restore prologue only for functions that use as-contract

## Objective

Emit the `as-contract` sender-restore prologue only for functions whose body can
switch the sender, instead of for every generated function.

## Tasks

- [x] Decide from the function's own body whether it can switch the sender, and
      emit the two `i32` locals and the two stdlib calls only where it can.
- [x] Keep the postlude and the prologue together: a function with no prologue has
      nothing to unwind, and one with a prologue must always unwind.
- [x] Keep every sender-leak regression green, including the mainnet
      8,668,161 shape — a function that `asserts!` its way out of `as-contract`,
      called twice by `map`.
- [x] Re-measure the prologue locals across the mainnet contract sweep recorded in
      [[073-decide-whether-a-contract-clarity-wasm-cannot-load]] ("Mainnet-state
      margin") and state the headroom this returns.

## Acceptance Criteria

- A function whose body contains no `as-contract` generates no sender/caller depth
  locals and no `principal_depth`/`restore_principal_depth` calls.
- A function whose body contains one still restores the stacks on every path out,
  including an early return from inside the `as-contract`.
- The captured fixture replay stays at 340/340 and the clar2wasm suite stays green.

## What was done

The predicate is deliberately one-sided. A wrong `true` costs two locals and two
calls; a wrong `false` lets a switched sender escape a function and be inherited by
whatever runs next — which is exactly mainnet block 8,668,161. So it asks only
whether the *name* appears anywhere in the body's tree, including inside `let`
bodies, branches and arguments, and does not try to decide reachability.

It deliberately does **not** follow calls into other functions, and that is sound
rather than a shortcut: `as-contract` switches the sender for the dynamic extent of
its own body, which ends before any callee's postlude runs, so a callee cannot leak
into its caller. The `*_no_leak` tests are the ones that would catch this being
wrong, and they pass.

Green afterwards: 1,406 clar2wasm tests, the nine `as_contract`/`wasm_trait_fold`
conformance gates including `returning_out_of_as_contract_restores_the_sender`, and
the captured replay at 340/340.

Split out of [[073-decide-whether-a-contract-clarity-wasm-cannot-load]].

The `as-contract` sender-restore fix (see [[060]]) adds two `i32` locals to
every generated function's prologue whether or not the function's body
contains an `as-contract`. That is a cost every mainnet contract pays:
locals, plus instructions at every call. Emit the save/restore only for
functions whose body contains an `as-contract` (upstream issue
[stx-labs/clarity-wasm#575](https://github.com/stx-labs/clarity-wasm/issues/575)
context), then re-measure the prologue locals across the mainnet contract
sweep recorded in [[073]] ("Mainnet-state margin").

Note the interaction with [[073]]'s B2: the two prologue locals also count
toward a function's local total, so removing them where unneeded slightly
raises headroom under wasmparser's 50,000-locals limit — though after A+B1+B2
no source-level shape reaches that limit anyway.

## Mainnet-state measurement — 2026-08-09

`cargo xtask sweep-contracts /home/aldur/mainnet-8716986/state` measured a
maximum of **17,055 live locals out of 50,000**, leaving **32,945 locals of
headroom**, at
`SP3EGAD1CG4KTM1Z3B2D85WQMC1FQG0FVM3TPAE8D.t-a-1::.top-level`.

The maximum is a top-level expression, so it has no function sender-restore
prologue: this optimization returns **zero additional locals at the measured
worst site**. It still removes exactly two locals from each generated function
that does not contain `as-contract`. The earlier 16,505 maximum is not a valid
before/after delta because other compiler changes landed between the two full
sweeps.

The inventory verdict was red for an orthogonal reason: 64 current contracts
compiled but did not load. That does not hide a locals measurement — their
compile reports were included — and it is recorded under [[084]] and [[093]],
not claimed as success here.
