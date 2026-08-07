---
id: "081"
title: "Emit the as-contract sender-restore prologue only for functions that use as-contract"
status: pending
priority: medium
effort: small
dependencies: []
tags: ["mainnet", "vm", "clarity"]
created_at: 2026-08-07
---

# Emit the as-contract sender-restore prologue only for functions that use as-contract

## Objective

<!-- Describe the goal of this task -->

## Tasks

- [ ] TODO

## Acceptance Criteria

- TODO

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
