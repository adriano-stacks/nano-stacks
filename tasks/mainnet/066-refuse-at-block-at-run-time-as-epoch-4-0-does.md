---
id: "066"
title: "Refuse at-block at run time, as epoch 4.0 does"
status: completed
priority: high
effort: medium
type: bug
group: mainnet
tags: ["mainnet", "vm", "clarity", "consensus"]
created_at: 2026-08-06
completed_at: 2026-08-06
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

- [x] Gate `at-block` on the *executing* epoch rather than the compiled one, so a
      module built under 2.4 semantics still refuses the word in 4.0. The
      executing epoch is not the module's, so this cannot be a compile-time
      decision.
- [x] Map the refusal to the error identity stacks-core produces —
      `RuntimeCheckErrorKind::AtBlockUnavailable` — not a generic runtime failure,
      because the error text is in the receipt.
- [x] Check the same shape for every other epoch-gated *runtime* predicate, not
      just this one. `supports_call_with_constant()` is checked at
      `functions/database.rs:113` and is the same pattern one word across.
- [x] Crosscheck a contract deployed with `at-block` and called in 4.0 against
      the interpreter, asserting the error and the cost dimensions.
- [x] Split the non-epoch-gated constant-target contract-call disagreement found
      by the audit into
      [[067-reject-contract-call-through-a-constant-while-depl]] instead of
      keeping this completed `at-block` fix open for a different bug.

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

## The gate is in, and the crosscheck cannot express the case

`enter_at_block` refuses on `!epoch.supports_at_block()` before the argument count
and before the cost, in that order because stacks-core's `special_at_block` does the
same and the refusal therefore charges no `AtBlock` cost — which is in the receipt.
The error is `RuntimeCheckErrorKind::AtBlockUnavailable`, its identity and not a
generic runtime failure.

**clar2wasm's own crosscheck harness cannot pin it, and it is worth writing down
why.** The case needs a contract analysed under a Clarity version where `at-block`
resolved and *executed* under an epoch where it does not. `TestEnvironment::new`
refuses that pairing by construction — `epoch_and_clarity_match` replaces a
mismatched version with `default_for_epoch(epoch)` — so a snippet asking for it is
silently run as Clarity 6, where both engines refuse `at-block` at *analysis* with
"use of unresolved function". That is a true fact about a new contract and not this
defect at all. The three `at_block_*` tests already in `words/blockinfo.rs` carry
`#[ignore = "test system needs to be improved relative to versioning and epochs"]`
for the same reason.

Where it *can* be expressed is nano's own harness, which deploys a Clarity 2
contract into an epoch-4.0 state already (`conformance/block_info_tenure_height.rs`
does exactly that) — but a contract *containing* `at-block` cannot be deployed under
epoch 4.0 at all, since analysis refuses the word whatever the version. So the pin
needs a planted stored analysis, which is the shape [[064]]'s deploy-epoch fixture
already builds. That is the remaining item and it is a test-fixture problem rather
than a production one.

## The pin exists, and it found that the gate was in the wrong place

`crates/nano-conformance/tests/conformance/at_block_refusal.rs`, three tests, both
engines, no infrastructure. The state is planted the way [[064]]'s fixture plants
one — a contract whose stored analysis names **Epoch 3.3 / Clarity 2** and whose
source uses `at-block`, called at 4.0 — because a contract *containing* the word
cannot be deployed at 4.0 by either engine. Both the analysis and the contract
definition are produced by the reference implementation itself, in throwaway
in-memory stores (`run_analysis` at 3.3, `initialize_versioned_contract` at 3.3), so
nothing planted is a hand-written stand-in for what the chain holds.

Writing it falsified two of this task's own closed items. The gate was in
`enter_at_block`, and a host function runs **after its arguments**:

- **The `AtBlock` cost was charged.** `AtBlock::traverse` called `self.charge(…)`
  before emitting anything, so the refusal cost what the word costs.
  `special_at_block` refuses *before* `runtime_cost(AtBlock)`.
- **`reserve-v1`'s own shape did not refuse at all.** Its call site is
  `(at-block (unwrap! (get-block-info? id-header-hash block) (err ERR_BLOCK_INFO)) …)`
  — an argument that can return from the function on its own. Evaluating it first
  meant the answer was the argument's, not a refusal. Measured, with the refusal
  removed again: `Internal(… "Could not pop at_block")`, because the early return
  leaves the host's at-block stack pushed. Either way it is not what the chain says.
- **The error identity was wrong**, which the item claimed was done.
  `error_mapping::clone_runtime_check_error` handled two variants and turned every
  other into `WasmError::Expect(text)` — so the receipt read
  `Internal(InvariantViolation("Expect(\"AtBlockUnavailable\")"))`. That is not
  `is_acceptable_runtime_failure`, so nano would have **refused the block** where
  stacks-core fails the transaction and accepts it (`AtBlockUnavailable` is not
  `rejectable()`). A gate that stops the node is not the same conformance as a gate
  that writes a receipt.

The fix is `AtBlock::traverse` emitting the refusal *in place of* the whole
expression when the module's **executing** epoch withdraws the word — the charging
epoch `with_cost_code_for_epoch` was given, which is the epoch the chain is running
now, exposed as `WasmGenerator::executing_epoch()`. Nothing of the expression is
emitted: no cost, no argument, no body. It still raises only where control reaches
it, which `a_branch_that_never_reaches_at_block_still_answers` holds: an `at-block`
under an untaken `if` answers, and the taken branch refuses. Widening a refusal to
the whole contract is 8,668,096's mistake, and this is not that.

### The cost oracle is stacks-core's own snapshot, and it matches exactly

`stackslib/src/chainstate/tests/runtime_analysis_tests.rs`'s
`runtime_check_error_kind_at_block_unavailable_ccall` deploys a contract in 3.3 and
calls it in 3.4. Its recorded snapshot says `vm_error: Some(AtBlockUnavailable)`,
`(err none)`, block accepted, and

```
ExecutionCost { write_length: 0, write_count: 0, read_length: 159, read_count: 3, runtime: 275 }
```

nano charges **that**, in both engines, for that contract's source copied byte for
byte (`read_length` is the contract's size, which is why it is copied rather than
paraphrased). All five dimensions are asserted rather than only the writes, because
they were measured before being claimed. With the refusal removed the runtime is the
`AtBlock` charge higher and the test fails on that dimension.

The write dimensions are the part that matters for how this was ever invisible: a
refusal writes nothing, so a block whose only difference is a refused read seals the
root an untouched block seals.

## Every other epoch-gated runtime predicate in `clarity/src/vm/functions`

Read exhaustively out of the pinned revision (`efc34a0`), excluding test modules.
nano executes only at epoch 4.0, so the question for each is: does the wasm path
produce **4.0's** answer, and can the other answer be reached at all?

| predicate | site | 4.0's answer | wasm path |
|---|---|---|---|
| `supports_at_block()` | `database.rs:562` | refuse | **this task**, `at_block_refusal.rs` |
| `epoch_id >= Epoch30` (tenure-height reading) | `database.rs:983` | a tenure height | fixed at 8,706,194; `conformance/block_info_tenure_height.rs` |
| `supports_call_with_constant()` | `database.rs:113` | allow | allowed unconditionally, which is 4.0's answer. Its neighbouring `!is_deploying` conjunct has **no** counterpart — see below |
| `value_sanitizing()`, `treats_unexpected_serialization_as_none()` | `conversions.rs:358,362` | sanitize; unexpected ⇒ `none` | decided in wasm, epoch-independently, and `words/consensus_buff.rs`'s crosschecks run at `StacksEpochId::latest()` — `from_consensus_buff_tuple_extra_pair` and `_invalid_extra` are those two answers |
| `fixes_map_off_by_one()` | `sequences.rs:204` | stop at the shortest sequence | `map_stops_at_the_shortest_sequence`, added here |
| `fixes_tuple_merge_size_check()` | `tuples.rs:134` | `ValueTooLarge` at the merge | none, and **unreachable**: at 4.0 `check_special_merge` refuses an oversized merge at analysis, so only a pre-4.0 analysis can carry one, and stacks-core's own note on the predicate says such a contract "deployed and became uncallable" |
| `handles_with_stx_combined_check()` | `post_conditions.rs:684` | the combined check | `linker.rs:215`, keyed on `global_context.epoch_id` — already a runtime counterpart |
| `switch_on_global_epoch!` ×10 | `database.rs`, `assets.rs` | the ≥2.05 variant | one implementation, which is that variant; the `Epoch20` branch is unreachable |
| `match exec_state.epoch()` in `special_concat` | `sequences.rs:415` | `v600`, variadic | variadic `concat`, which is what W6.2 built |
| `Value::sanitize_value(epoch, …)` on a contract-call return | `database.rs:267` | sanitize | by construction: a wasm value is laid out from its analysed type, so an over-wide value has nowhere to exist. The residue of that is the tuple-supertype asymmetry already accounted for on [[060]] |
| `admits(epoch, …)`, `cons_list(epoch)`, `least_supertype(epoch)` | `assets.rs`, `sequences.rs`, `conversions.rs` | 4.0's | the host functions pass `global_context.epoch_id` |

**One predicate has no counterpart and it is not epoch-gated**, which is why the item
would have missed it and why it is recorded here rather than quietly:
`supports_call_with_constant()`'s third conjunct is `!contract_context.is_deploying`.
Measured at the executing epoch:

```
(define-constant target .callee) (contract-call? target foo)   ;; at a deploying top level
compiled     Ok(Some(Response { committed: true, data: UInt(1) }))
interpreted  Err(RuntimeCheck(ContractCallExpectName))
```

A deployment whose top level calls a contract through a constant succeeds here and
fails on the chain — a **state root** divergence, since the reference's failed deploy
writes nothing. It is `#[ignore]`d as
`a_constant_contract_call_while_deploying_agrees` in `words/contract.rs` with that
reason, and tracked in
[[067-reject-contract-call-through-a-constant-while-depl]]: the fix is a runtime branch inside the module
on a flag the host would have to publish, which is more than a mapping change and
wants its own measurement against mainnet.

`ContractCallExpectName` is also the other unit variant a host function raises that
`clone_runtime_check_error` still mangles into `Expect(…)`. Left alone deliberately,
and said so in the code: correcting it turns a refused block into a failed
transaction, which is consensus-visible and wants an oracle rather than a guess made
while passing.

## What none of this proves

No mainnet block in the replayed window calls an `at-block` contract, so this is
conformance ahead of a divergence rather than a fix confirmed against the chain — the
881 contracts are a population, not a hit. And the planted fixture is nano's own
state-writing, not stacks-core's: what it reproduces is a contract whose stored
analysis predates 3.4, which is checked against the mainnet capture's real
`metadata_table` in [[064]] rather than here.
