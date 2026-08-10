---
id: "064"
title: "Compile a contract under the epoch it was deployed in"
status: completed
priority: high
effort: medium
type: bug
group: mainnet
dependencies: ["060"]
tags: ["mainnet", "vm", "clarity", "conformance"]
created_at: 2026-08-06
completed_at: 2026-08-10
---

# Compile a contract under the epoch it was deployed in

## Objective

Before this task, `ensure_wasm_module` compiled a contract under epoch 4.0 and,
when clarity-wasm refused it, retried under **the newest older epoch that happened
to accept it**, then executed that build. A live mainnet catch-up printed:

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

- [x] Find what `reserve-v1` uses that epoch 4.0 rejects and measure the exact
      current-tip population by compiling every contract under both its recorded
      epoch and forced Epoch40. Report the count, not an example.
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
- [x] Pin the semantic/runtime-epoch separation with a contract using a removed
      word, deployed before its removal and called after it. The planted
      stacks-core snapshot in
      [[066-refuse-at-block-at-run-time-as-epoch-4-0-does]] asserts the exact
      error and all five cost dimensions.
- [x] Confirm the same behavior with a real captured network receipt from an old
      mainnet contract after the removal epoch. Block 8,686,666 calls
      `reserve-v1`; the isolated replay matches its result, all five costs, four
      ordered events and committed root through two compiled runs and the
      interpreter call comparison.

## Acceptance Criteria

- No production path chooses a Clarity epoch by trying compilations until one
  succeeds.
- The epoch a contract is compiled under is a function of chain state and nothing
  else, so a clarity-wasm fix cannot move a receipt.
- Costs charged for executing an old contract are the current epoch's.
- The mainnet replay reports how many contracts this affected and none of them is
  still executed under a guessed epoch.
- The release report distinguishes the reference snapshot from an observed
  mainnet receipt and does not mark the latter green until one is captured.

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

The original metadata/source scan found 881 textual candidates, but that was not
an exact compiler or canonical-current-state measurement. The corrected
read-only sweep at state tip
`63f2beda310a00e9c790f9f8e2e41f42f6f145f034e6cac040cd9bd46746c2b6` measured:

- **146,346** raw analysis metadata rows, **137,342** distinct identifiers and
  **58** noncanonical stale candidates, leaving **137,284** current contracts.
- **137,284/137,284** compile and load under their chain-recorded semantic epoch.
- Forced Epoch40 loads 136,413 and refuses **871**, with **zero unmeasured**:
  866 on removed `at-block`, four on unresolved `loop_n4`, and one on unresolved
  `mul-down`.
- Recorded epochs account for every current contract: Epoch2_05 55,859; Epoch20
  1,693; Epoch21 6,812; Epoch22 224; Epoch23 1,522; Epoch24 9,985; Epoch25 9,486;
  Epoch30 3,144; Epoch31 16,681; Epoch32 3,985; Epoch33 23,378; Epoch34 4,194;
  Epoch40 321.

The sweep is typed and fail-closed: compiler refusal is separate from a state or
inspection failure, and its denominator must account for every current contract.
Its retained output is `/tmp/task064-epoch-inventory-7cbed784.txt`, SHA-256
`2ecc19bc78feb3159900f10da9d27bc049fa5f26d424ef49c528b1dea56dfdcb`, bound to
compiler `sha256:f493148d4790beaa10d3f06f5f23c4fd8299b4a732c7fbb1aae1b194922e095b`.

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

### The removed-word pin and the observed network receipt

The reference pin remains `conformance/at_block_refusal.rs` and [[066]]: a
contract whose stored analysis names **Epoch 3.3 / Clarity 2** and whose source
uses `at-block`, called at 4.0 through both engines from planted state. The pinned
stacks-core snapshot `runtime_check_error_kind_at_block_unavailable_ccall`
records `vm_error: Some(AtBlockUnavailable)`, `(err none)`, an accepted block and

```
ExecutionCost { write_length: 0, write_count: 0, read_length: 159, read_count: 3, runtime: 275 }
```

nano charges that in both engines for the source copied byte for byte. This is a
reference snapshot, not the observed network receipt.

The observed receipt is transaction
`f33840c54f18a314f00b1338bc3d43e3103cbe9ce424d0418e94bc463903fe62`, transaction
zero of mainnet block **8,686,666**. It calls the old Epoch24/Clarity2
`SP4SZE494VC2YC5JYG7AYFQ44F5Q4PYV7DVMDPBG.reserve-v1` after `at-block` was
removed. Hiro records `(ok u12395909)`, costs
`{read_count:53, read_length:48263, runtime:87814, write_count:8,
write_length:83}` and four ordered events. The ignored conformance gate
`the_mainnet_8686666_old_epoch_receipt_and_root_match_the_canonical_oracle`
executes an isolated stopped-state copy twice with root verification, checks the
receipt and events, and compares the compiler call with the interpreter call.

The release report now names these separately as the reference snapshot and the
observed mainnet receipt, and refuses qualification if either is missing or
invalid. Its mandatory state inventory reports the exact 871-contract affected
population while proving all 137,284 run under recorded epochs.

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
  first was this task's. The second was a clarity-wasm gap on the same 866
  contracts, closed there, and the pin above is shared between the two because the
  fixture is the same one.

### Evidence, 2026-08-09

- `cargo test -p nano-vm --lib`: 37 passed, one unrelated diagnostic ignored.
- `cargo test -p xtask --bin xtask`: 19 passed.
- strict all-target Clippy for `nano-vm` and `xtask`: green.
- `cargo test -p clar2wasm --lib cost::crosscheck`: 30/30; STX/NFT word suites
  16/16 and 27/27; strict clar2wasm Clippy green.
- Two independent stopped-state block-8,686,666 replays matched the network
  result, all five costs, all four ordered events and committed root.
- The full typed sweep completed in about two hours, left both source database
  stamps unchanged, and produced the exact counts above with no unmeasured row.

All task-local implementation and evidence are complete. The declared dependency
[[060]] completed after its pristine release-binary replay, so this task can close.
