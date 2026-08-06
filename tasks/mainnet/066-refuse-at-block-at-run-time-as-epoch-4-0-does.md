---
id: "066"
title: "Refuse at-block at run time, as epoch 4.0 does"
status: pending
priority: high
effort: medium
type: bug
group: mainnet
dependencies: ["064"]
tags: ["mainnet", "vm", "clarity", "consensus"]
created_at: 2026-08-06
---

# Refuse at-block at run time, as epoch 4.0 does

## Objective

stacks-core checks `supports_at_block()` **twice**, against two different epochs,
and nano implements only the first.

- At analysis time, against the epoch the contract was deployed in
  (`clarity/src/vm/analysis/type_checker/v2_1/natives/mod.rs:138`). A contract
  published before 3.4 was accepted with `at-block` and its stored analysis keeps
  it accepted forever.
- At run time, against the epoch the chain is executing *now*
  (`clarity/src/vm/functions/database.rs:562`):

```rust
if !exec_state.epoch().supports_at_block() {
    return Err(RuntimeCheckErrorKind::AtBlockUnavailable.into());
}
```

`supports_at_block()` is `< Epoch34`, so on mainnet today the first check passes
and the second fails: calling `at-block` in an old contract **errors**.

clar2wasm's `AtBlock` word (`words/blockinfo.rs:170`) carries no such gate. Its
`enter_at_block`/`exit_at_block` host functions do the work unconditionally, so a
module built under a pre-3.4 semantic epoch — which after [[064]] is exactly what
those contracts get, correctly — evaluates `at-block` and returns a value where
mainnet returns an error.

**881** contracts in the mainnet checkpoint at height 8,665,600 have an
`(at-block` call site. Any call reaching one of those lines diverges, and it
diverges in a way a state root can hide: the error path writes nothing, so a block
whose only difference is a refused read seals the root an untouched block seals.

## Tasks

- [ ] Gate `at-block` on the *executing* epoch rather than the compiled one, so a
      module built under 2.4 semantics still refuses the word in 4.0. The
      executing epoch is not the module's, so this cannot be a compile-time
      decision.
- [ ] Map the refusal to the error identity stacks-core produces —
      `RuntimeCheckErrorKind::AtBlockUnavailable` — not a generic runtime failure,
      because the error text is in the receipt.
- [ ] Check the same shape for every other epoch-gated *runtime* predicate, not
      just this one. `supports_call_with_constant()` is checked at
      `functions/database.rs:113` and is the same pattern one word across.
- [ ] Crosscheck a contract deployed with `at-block` and called in 4.0 against
      the interpreter, asserting the error and the cost dimensions.

## Acceptance Criteria

- An old contract's `at-block` call returns `AtBlockUnavailable` in epoch 4.0 and
  evaluates normally in an epoch that supports it.
- Every epoch-gated runtime predicate in `clarity/src/vm/functions` has a
  counterpart in the wasm path or a note saying why it cannot be reached.
- A crosscheck covers the refusal's error identity and its cost, since neither is
  visible in a state root.

## Where this came from

Found while closing [[064]], which made the semantic epoch a function of chain
state and in doing so made this the remaining divergence on the same 881
contracts. [[064]] does not cause it — the old guessing path built those modules
under 3.3, where `at-block` also evaluates — it only makes it the whole of what
is left. Its last open item, the receipt pin, is this task's crosscheck.
