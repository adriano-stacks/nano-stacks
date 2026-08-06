---
id: "064"
title: "Compile a contract under the epoch it was deployed in"
status: in-progress
priority: high
effort: medium
type: bug
group: mainnet
dependencies: ["060"]
tags: ["mainnet", "vm", "clarity", "conformance"]
created_at: 2026-08-06
---

# Compile a contract under the epoch it was deployed in

## Objective

`ensure_wasm_module` compiles a contract under epoch 4.0 and, when clarity-wasm
refuses it, retries under **the newest older epoch that happens to accept it**,
then executes that build. A live mainnet catch-up prints it once a contract:

```
SP4SZE494VC2YC5JYG7AYFQ44F5Q4PYV7DVMDPBG.reserve-v1 does not compile under
epoch 4.0, rebuilt as Epoch33
```

The nearby comments claim that an older semantic epoch also selects an older
cost schedule. That diagnosis is stale: `compile_under` passes the chosen
semantic epoch and `Epoch40` cost epoch separately to
`clar2wasm::compile_for_cost_epoch`. The separation is correct and needs a
receipt-cost regression, but it is not the defect observed here.

The actual defect is worse than a stale comment: **the semantic epoch is chosen
by what the compiler happens to accept**, not by the
chain. A clarity-wasm fix that makes epoch 4.0 accept the contract silently
changes which language semantics are compiled. Nothing in consensus should be
a function of nano's own compiler revision or retry order.

stacks-core does not guess: a contract is analyzed once, at deploy time, under
the epoch and Clarity version then in force, and that analysis is stored. Keyword
availability — `at-block`, removed in 3.4 — is an analysis-time property, so an
old contract keeps working without any epoch being inferred at call time.

## Tasks

- [x] Find what `reserve-v1` uses that epoch 4.0 rejects, and how many mainnet
      contracts the replay hits it for. Report the count, not an example.
- [x] Read the deploy epoch out of the stored contract analysis rather than
      searching for an epoch that compiles.
- [x] Preserve and verify the existing separation between language semantics
      (deploy epoch) and execution charging (current epoch). Remove the stale
      comments and pin a receipt/cost regression proving an old contract called
      in Epoch 4.0 is charged with the Epoch 4.0 schedule.
- [x] Include the chain-derived semantic epoch and compiler identity in native
      module cache identity or otherwise prove a cached module cannot outlive
      either input.
- [x] Make a contract whose deploy epoch is unavailable reject the block rather
      than execute under a guessed epoch, per [[060]]'s boundary.
- [x] Pin it: a contract using a removed word, deployed before its removal,
      called after it, with the receipt and cost dimensions asserted against the
      network's.

## Acceptance Criteria

- No production path chooses a Clarity epoch by trying compilations until one
  succeeds.
- The epoch a contract is compiled under is a function of chain state and nothing
  else, so a clarity-wasm fix cannot move a receipt.
- Costs charged for executing an old contract are the current epoch's.
- The mainnet replay reports how many contracts this affected and none of them is
  still executed under a guessed epoch.

## Where this came from

Found in the log of the mainnet catch-up run of 2026-08-06, at the same time as
the peer-sortition stall on
[[049-derive-canonical-sortitions-from-the-local-burncha]]. It is not an
interpreter fallback and so does not violate [[060]]'s engine boundary — it is
clarity-wasm either way — but it is the same *kind* of thing one epoch across: a
compiler gap being worked around in the production path instead of being closed,
in a way that shows up in receipts rather than in roots.

## Note, 2026-08-06

### What it is, measured

`reserve-v1` uses `at-block`, on one line of 118:

```clarity
(at-block (unwrap! (get-block-info? id-header-hash block) (err ERR_BLOCK_INFO))
  (get-stx-stacking))
```

`StacksEpochId::supports_at_block()` is `< Epoch34`, so epoch 4.0 refuses the
source outright while the analysis stacks-core wrote at deploy time accepted it
and still stands.

Counted over the mainnet checkpoint's `metadata_table` at height 8,665,600, which
holds it as stacks-core wrote it:

- **146,141** contracts have a stored analysis.
- **881** of them have an `(at-block` call site in their stored source. That is
  the population epoch 4.0 rejects and the old code rebuilt under a guess.
- **116** were analyzed in `Epoch40`. The other 146,025 were analyzed under an
  older epoch, the largest group being 62,076 in `Epoch2_05`/Clarity1.

And the guess was wrong on the case that produced the log line.
`reserve-v1`'s stored analysis says **`Epoch24` / Clarity2**; the search picked
`Epoch33`, because the list it walked — `Epoch{20,21,25,30,33,34}` — contains no
2.4 at all. Five epochs of language rules apart, chosen by which one the compiler
happened to accept.

### What changed

`ensure_wasm_module` reads `ContractAnalysis.epoch` through
`AnalysisDatabase::load_contract_non_canonical` and compiles once, under that.
There is no retry and no epoch list; `ACCEPTING_EPOCHS` is deleted.
`epoch_for_version` survives only for the two readers that parse a source without
judging it — the scan for referenced contracts, whose only effect is which
modules get built early, and `nano-oracle`, which is not a node path — and its
doc comment now says so.

The analysis is present for imported state: `metadata_table` is copied whole by
the checkpoint import, and `clr-meta::<id>::analysis` was read directly out of the
capture to confirm it rather than inferred from the copy.

The stale cost comments are gone. `compile_under` already passed the semantic
epoch and `Epoch40` separately to `clar2wasm::compile_for_cost_epoch` and still
does; its comment now says which argument means what and why.

### Pins

- `a_contract_its_recorded_epoch_refuses_is_not_rebuilt_under_another` — a source
  whose recorded epoch withdrew the word it uses. A search finds 3.3 and runs; the
  chain's answer refuses. **Fails with the search restored.** This is the
  anti-guessing pin.
- `a_contract_with_no_recorded_analysis_stops_the_block` — the analysis deleted.
  The refusal must not report as an analysis failure, because that becomes a
  transaction receipt and the block carries on. **Fails with the search
  restored**, which compiled it happily.
- `an_old_contract_is_charged_the_current_epochs_costs` — three assertions,
  because the interesting one is vacuous alone: the charging epoch does change the
  module bytes, the production path's module is the one charging at 4.0, and it is
  not the one charging at the deploy epoch. Fails if `epoch` is passed for both.
- `a_cached_native_module_cannot_outlive_the_inputs_that_built_it` — the cache
  keys on the compiler's output, so no entry is reachable from inputs that would
  build something else, and an entry shared by inputs that build identical bytes
  is not a stale hit. Goes through the real `NativeModuleCache`.
- `a_rebuild_uses_the_epoch_the_chain_recorded` — the recorded epoch is read and
  its module built byte for byte. This one **does not** fail with the search
  restored, and that is a finding rather than a weak test: see below.

### The removed-word pin, and what stands in for the network

The last item is closed by `conformance/at_block_refusal.rs` and by [[066]], which
is where it went: a contract whose stored analysis names **Epoch 3.3 / Clarity 2**
and whose source uses `at-block`, called at 4.0 through both engines from planted
state — because a contract *containing* the word cannot be deployed at 4.0 by
either engine, which is the whole shape of the case.

**"The network's" receipt is not obtainable, and what replaces it is stronger than
a paraphrase would be.** The mainnet capture declares `receipts = false` and no
public API serves a historical receipt, so the oracle is stacks-core's own
consensus snapshot: `runtime_check_error_kind_at_block_unavailable_ccall`
(`stackslib/src/chainstate/tests/runtime_analysis_tests.rs`) deploys a contract in
3.3, calls it in 3.4, and records `vm_error: Some(AtBlockUnavailable)`, `(err
none)`, the block accepted, and

```
ExecutionCost { write_length: 0, write_count: 0, read_length: 159, read_count: 3, runtime: 275 }
```

nano charges that, in both engines, for that contract's source copied byte for byte
— `read_length` is the contract's size, which is why it is copied rather than
paraphrased. All five dimensions are asserted, and the write dimensions are the
reason this was ever invisible: a refusal writes nothing, so the block it produces
seals the root an untouched block seals.

Writing it falsified two of [[066]]'s own closed items — the `AtBlock` cost was
being charged for a refusal that charges nothing, and the error was arriving as
`Internal(InvariantViolation(…))`, which would have stopped the block where
stacks-core fails the transaction and carries on. Both are recorded there.

The three tests also cover the other half of the item, which is that the contract
is still *callable*: `a_branch_that_never_reaches_at_block_still_answers` — a
removed word under an untaken branch answers, and only the taken branch refuses.
Widening the refusal to the contract would have satisfied "it errors" and been a
different divergence.

### What it does not prove

- **The module bytes do not distinguish these epochs.** Measured: for the
  `at-block` source, 2.4 and 3.3 compile to identical wasm, and for a plain source
  2.1 and 4.0 do too. So on `reserve-v1` the old guess and the chain's answer
  produced the *same native code*. What was wrong with the guess was never these
  particular bytes — it was that the answer came from nano's compiler revision, so
  a clarity-wasm fix could move it silently. The consequence for the cache is that
  an explicit semantic epoch in the key would have partitioned entries holding the
  same code; the key stays the output.
- **That `at-block` refuses is [[066]]'s doing, not this task's.** stacks-core
  checks `supports_at_block()` twice: at analysis time against the *deploy* epoch
  (`type_checker/v2_1/natives/mod.rs:138`) and again at *runtime* against the
  *current* epoch (`functions/database.rs:562`, `RuntimeCheckErrorKind::
  AtBlockUnavailable`). Only the first is an epoch-selection question and only the
  first was this task's. The second was a clarity-wasm gap on the same 881
  contracts, closed there, and the pin above is shared between the two because the
  fixture is the same one.
- The count is of `(at-block` call sites in stored sources, not of contracts a
  compilation refuses. Compiling all 146,141 to get the exact figure was not done.
- No offline gate in the suite executes real mainnet contracts, so this change is
  not checked against a mainnet replay here. `mainnet_receipts` would catch a
  moved receipt but needs `NANO_MAINNET_RECEIPTS` pointing at an observer run.
