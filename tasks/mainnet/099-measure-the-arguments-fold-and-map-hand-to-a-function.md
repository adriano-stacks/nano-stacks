---
id: "099"
group: mainnet
title: "Measure the arguments fold and map hand to a function"
status: completed
priority: critical
effort: small
dependencies: ["097"]
tags: ["mainnet", "vm", "clarity-wasm", "consensus", "liveness", "release"]
created_at: 2026-08-09
completed_at: 2026-08-09
type: bug
---

# Measure the arguments fold and map hand to a function

## Objective

A contract that folds or maps a public or read-only function of its own cannot be
compiled at all, so every block calling it is refused. Not a wrong answer — a
refusal to load, which fails the block rather than the transaction.

## Evidence

`/home/aldur/mainnet-tip`, blocked at 8724864 on the same transaction that
[[097-cast-a-trait-argument-the-callee-declares-differently]] unblocked:

```
transaction 24d63204…c061558 of 8724865 failed the block rather than its own receipt:
  clarity-wasm cannot run a contract this call names, and the network ran it:
  Internal(Expect("Unreachable(\"SP2VCQJGH7PHP2DJK7Z0V48AGBHQAW3R3ZW1QF4N.pool-0-reserve-v2-0:
    contract analysis failed: Internal error: read-only call is missing argument sizes\")"))
```

## Cause

`edcb6bd3` gave a public or read-only function's prologue its argument sizes
through the `argument-sizes` region: the callee reads them there instead of
measuring the values itself (`wasm_generator.rs`, the `external_entry` branch of
the parameter loop). `traverse_call_user_defined` writes them, and it is the only
thing that does.

`fold` and `map` also apply a *named* function, and reach `local_call_public` /
`local_call_read_only` by the same path — but passed `None` for the sizes. Both
answer `GeneratorError::InternalError` rather than compiling. A private function
was unaffected, which is why the gap survived: the same `fold` over a private
helper compiles and runs.

## Fix

The sizes are measured where the value is: as each argument reaches the top of
the stack, before the next one covers it. `map` measures inside its argument
loop; `fold` measures the element as it is loaded and the accumulator as the copy
is pushed, because by the call the accumulator sits on top of the element.

Measured only when the callee is one a transaction could also enter
(`is_external_entry`), which is the same condition the prologue reads them under.

## Acceptance

- `fold` and `map` over a read-only function compile, run, and agree with the
  interpreter — including an argument whose size is not a constant.
- Mainnet block 8724865 executes. **Done**: the node executed 8724864 → 8725364
  in one round, state root `00d1e347…`, and went on catching up.
