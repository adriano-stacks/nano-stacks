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

Re-read while auditing the rest, there is a **second blocker under that one**, and
it is the one that would survive relaxing `epoch_and_clarity_match`.
`TestEnvironment` compiles through `compile` (`tools.rs:267`), which hands its
single `epoch` argument to `compile_for_cost_epoch` *twice* (`lib.rs:202-212`), once
as the semantic epoch and once as the charging epoch. `executing_epoch()` is the
charging one, so inside that harness it is definitionally equal to the semantic
epoch and the gap this task is about — semantics of 3.3, chain at 4.0 — cannot exist
there at all. Expressing it needs a second epoch threaded through both the compiled
and the interpreted deploy paths *and* the planted contract below, since a contract
containing `at-block` cannot be deployed at 4.0 by either engine. That is the same
fixture built twice, so the pin stays where it is.

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

## Every epoch-gated runtime predicate in the Clarity VM

Read exhaustively out of the pinned revision (`efc34a0`, the one
`nano-conformance/Cargo.toml` names), excluding test modules, over
`clarity/src/vm/functions/**`, `callables.rs`, `contexts.rs`, `vm/mod.rs`,
`database/**`, `costs/`, `types/`, `ast/`, `analysis/**` and `clarity-types`. The
search was for all **24** named `StacksEpochId` predicates
(`stacks-common/src/types/mod.rs:518-873`) *and* for bare `StacksEpochId::Epoch*`
comparisons, because four of the runtime gates are not named predicates at all
(`vm/mod.rs:327`, `functions/mod.rs:869`, `functions/database.rs:983`,
`callables.rs:229,245`).

nano executes only at epoch 4.0, so the question for each is: does the wasm path
produce **4.0's** answer, and can the other answer be reached at all?

**The question is not the same for every site**, which is what makes the list
shorter than its length. nano links the `clarity` crate: `ClarityDatabase`,
`AssetMap`, `LimitedCostTracker`, `build_ast` and `run_analysis` are stacks-core's
own code running in nano's process (`crates/nano-vm/src/lib.rs:11-18,3736`). A
predicate inside them has exactly one implementation and cannot disagree with
itself. What clar2wasm *replaces* is `functions/**` and the call and eval
machinery in `callables.rs` and `vm/mod.rs` — **18 decisions at 29 sites** — and
only those can diverge. The rest, **17 decisions at 45 sites**, are listed below
them so the count is the whole population rather than the interesting part of it.

Epoch *threading* — `admits(epoch, …)`, `cons_list(epoch)`,
`least_supertype(epoch)`, `parse_type_repr(epoch)`, `sanitize_value(epoch, …)` —
is counted once, at the bottom of the first table, rather than at each of its ~40
call sites: the decision is in the callee, which nano links.

### The 18 clar2wasm has to answer itself

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
| `uses_arg_size_for_cost()` | `callables.rs:186` | charge an argument at the size of the value | `wasm_generator.rs:1182` `runtime_size` emits the value's own size; `cost.rs`'s `charges_an_argument_for_what_it_holds`. The refusal path had no counterpart until now — see below |
| `sanitize_in_function_invocation()` | `callables.rs:302` | sanitize the argument against the parameter type | by layout: a wasm argument is written into the callee's declared type, so a wider one has nowhere to go. Where it *refuses* instead, the two engines disagreed until now — see below |
| `< Epoch21` / `>= Epoch21` trait-reference arguments | `callables.rs:229,245` | `CallableType`, not `TraitReferenceType` | `initialize.rs`'s `implicit_contract_cast`, which re-tags nested trait references too; `words/traits.rs`'s `print_a_trait_reference_under_the_two_oh_five_type_checker` |
| `uses_pre_sanitized_variables()` | `vm/mod.rs:220,230` | borrow, charging no clone | a compiled constant is emitted into the module, so nothing is cloned and nothing is charged for cloning, which is 4.0's answer. The load-time half of the same predicate is linked, below |
| `>= Epoch2_05` (`NativeFunction205` cost input) | `vm/mod.rs:327` | the per-word cost input, not `args.len()` | one implementation, the cost tables of `cost/clar*.rs`, which are the ≥2.05 variant |
| `>= Epoch21` (`as-contract` has a cost) | `functions/mod.rs:869` | charge `AsContract` | charged unconditionally, `cost/clar3.rs:656`; `conformance/as_contract_codegen.rs` |
| `Value::sanitize_value(epoch, type_of(value), …)` at the call entry | `contexts.rs:1355` | sanitize | the identity: the expected type *is* the value's own, so it re-canonicalizes and cannot narrow. clar2wasm's entry (`initialize.rs`'s `call_function`) does not run it, and the crosschecks below are what says that agrees |
| `Value::sanitize_value(epoch, …)` on a contract-call return | `database.rs:267` | sanitize | by construction: a wasm value is laid out from its analysed type, so an over-wide value has nowhere to exist. The residue of that is the tuple-supertype asymmetry already accounted for on [[060]] |
| `admits(epoch, …)`, `cons_list(epoch)`, `least_supertype(epoch)`, `parse_type_repr(epoch)` | `assets.rs`, `sequences.rs`, `conversions.rs`, `define.rs`, `functions/mod.rs:637` | 4.0's | the host functions pass `global_context.epoch_id` — 40 of the ~109 mentions of an epoch in `linker.rs` are exactly that |

### The 17 nano links rather than reimplements

One implementation each, stacks-core's own, compiled into nano and reached through
the same call. Listed because "no counterpart" and "cannot disagree" are different
answers and the item asked for the population.

| predicate | sites | what it decides |
|---|---|---|
| `value_sanitizing()` | `database/clarity_db.rs:585`, `database/key_value_wrapper.rs:435`, `clarity-types/src/types/serialization.rs:1260` | whether a value read from or written to the store is sanitized |
| `uses_marfed_block_time()` | `clarity_db.rs:1045` | whether the block time is a MARF key — nano writes it in `setup_block_metadata` |
| `< Epoch30` tenure height | `clarity_db.rs:1124,1146` | tenure height is stored, not the block height |
| `>= Epoch22`, `>= Epoch25`, `>= Epoch40` unlock heights | `clarity_db.rs:1216,1226,1236` | the v2/v3/v4 PoX auto-unlock heights; the 4.0 one is pox-5 activation |
| `clarity_uses_tip_burn_block()` | `clarity_db.rs:1287,1372` | burn-block reads use the tip, not the parent |
| `uses_pre_sanitized_variables()` at load | `contexts.rs:2156`, via `clarity_db.rs:916` | contract variables are sanitized when the contract is loaded |
| `sums_stacking_assetmap()` | `contexts.rs:507,566` | a second stacking entry sums instead of erroring; nano reaches it through `GlobalContext::log_stacking` (`contexts.rs:1900`) from `nano-vm/src/pox.rs:350,379` |
| `supports_cost_voting_contract()` | `costs/mod.rs:958,998` | 4.0 retires cost voting, which is why a cost tracker builds over an empty store |
| `limits_parameter_and_method_count()` | `types/signatures.rs:415,423,449`, `analysis/type_checker/v2_1/mod.rs:1458` | the 3.3 caps on trait methods and function parameters |
| `rejects_parse_depth_errors()` | `ast/errors.rs:210` | whether a parse-depth error invalidates the block or fails the transaction |
| `rejects_supertype_too_large()` | `analysis/errors.rs:694` | same question for `SupertypeTooLarge` |
| `supports_at_block()` (analysis half) | `analysis/.../natives/mod.rs:138` | the deploy-time half of this task's pair |
| `fixes_tuple_merge_size_check()` (analysis half) | `natives/mod.rs:231` | why the runtime half above is unreachable |
| `analysis_memory()` | `natives/mod.rs:329,344`, `natives/options.rs:303,318`, `v2_1/mod.rs:1488,1878,1893,1908,1931,1948,1967,1981,1994,2008,2022,2047,2068` | analysis-time memory metering |
| `surfaces_trait_compliance_cost_errors()` | `v2_1/mod.rs:732` | a cost error inside trait checking is itself, not `IncompatibleTrait` |
| `meters_in_contract_trait_entry()` | `v2_1/mod.rs:1084` | `AnalysisUseTraitEntry` for in-memory traits too |
| `switch_on_global_epoch!`'s `Epoch10` arm | `functions/mod.rs:42` | unreachable in any epoch nano runs |
| `mempool_garbage_behavior()`, `mining_commitment_frequency()`, `uses_nakamoto_*()`, `starts_reward_cycle_at_0()`, `includes_sip_031()`, `enforces_strict_signature_order()`, `allows_pox_punishment()`, `block_commits_to_parent()`, `supports_shadow_blocks()`, `supports_pox_missed_slot_unlocks()`, `supports_sip040_post_conditions()`, `supports_staking_post_conditions()`, `allows_tx_signatures_with_high_s()`, `supports_specific_budget_extends()`, `coinbase_reward()` | outside `clarity/`, in `stackslib` | not VM predicates at all; nano's counterparts are its chainstate, and each is pinned by its own conformance test rather than here |

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
reason, and accounted for on [[060]]: the fix is a runtime branch inside the module
on a flag the host would have to publish, which is more than a mapping change and
wants its own measurement against mainnet.

`ContractCallExpectName` is also the other unit variant a host function raises that
`clone_runtime_check_error` still mangles into `Expect(…)`. Left alone deliberately,
and said so in the code: correcting it turns a refused block into a failed
transaction, which is consensus-visible and wants an oracle rather than a guess made
while passing.

## What the audit turned up: a refused argument was refused differently

`sanitize_in_function_invocation()` is new in 4.0, so its row could not be settled
by reading — the question is whether the wasm path *reaches the same answer by
layout*, and the only way to know is to ask both engines. Measured at the executing
epoch, with `crosscheck_cost`, which compares the value and all five cost
dimensions:

```
(define-public (f (a {x: uint})) (ok (get x a)))  called with {x: u1, y: u2}
compiled     Err(RuntimeCheck(TypeError(         {x: uint}, {x: uint, y: uint})))
interpreted  Err(RuntimeCheck(TypeValueError(    {x: uint}, "Tuple(...{x,y}...)")))
```

Both refuse — layout is not the divergence — but **they refuse with different error
identities, and for different costs**, and a mistyped contract-call argument is an
ordinary transaction anyone can send. At 4.0 a `RuntimeCheck` error is
`ClarityRuntimeTxError::AnalysisError`, which keeps the transaction and records
`error.to_string()` as its `vm_error`
(`stackslib/src/chainstate/stacks/db/transactions.rs:1448`), so the string is in the
receipt; and a refused call writes nothing, so a state root sees neither.

Two fixes in `initialize.rs`'s `call_function`, both measured before being claimed:

* **the identity.** `clarity2_implicit_cast` (`callables.rs:527`) and the
  `admits` arm after it (`callables.rs:334`) both raise `TypeValueError(expected,
  value)`. The compiler raised `TypeError(expected, type_of(value))` for every
  mistyped argument, of any shape — a wider tuple, a wrong primitive, a sequence
  longer than the parameter.
* **the cost.** `execute_apply` charges `UserFunctionApplication` and one
  `InnerTypeCheckCost` per argument *before* it type-checks or even counts them
  (`callables.rs:174-210`), so a refused call has paid for all of them. In a
  compiled contract those charges are in the function's own prelude, which a call
  refused at this boundary never enters, so it paid nothing: 115 against 183 for one
  `int` passed as a `uint`. `charge_refused_application` pays them on the refusal
  path only, over the passed arguments at 4.0 and the declared parameters before 3.3
  because that is the split `uses_arg_size_for_cost()` makes.

Five crosschecks in `initialize.rs`'s `refused_arguments`: a wider tuple, a wrong
primitive type, an over-long sequence, too many arguments and too few. Each fails on
the identity with the first fix reverted and on the cost with the second.

The wrong-arity pair is there because the interpreter charges *before* it counts, so
the two directions charge over different sets — and it is the one shape where the
refusal is not about a type at all.

## What none of this proves

No mainnet block in the replayed window calls an `at-block` contract, so this is
conformance ahead of a divergence rather than a fix confirmed against the chain — the
881 contracts are a population, not a hit. And the planted fixture is nano's own
state-writing, not stacks-core's: what it reproduces is a contract whose stored
analysis predates 3.4, which is checked against the mainnet capture's real
`metadata_table` in [[064]] rather than here.
