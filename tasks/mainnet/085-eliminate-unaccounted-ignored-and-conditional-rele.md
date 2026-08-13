---
title: "Eliminate unaccounted ignored and conditional release tests"
id: "085"
status: in-progress
priority: critical
effort: large
type: bug
group: mainnet
dependencies: []
tags: ["mainnet", "conformance", "ci", "release", "gates"]
created_at: "2026-08-07"
---

# Eliminate unaccounted ignored and conditional release tests

## Objective

No required behavior may disappear behind `#[ignore]`, `skip_gate`, a missing
`NANO_*` input or a reason-string heuristic. Infrastructure tests may need a
different job, but the release decision must either run them successfully or fail
while naming the missing input. Semantic, consensus and required interoperability
tests may not be waived.

## Tasks

- [x] Build a machine-readable inventory of every `#[ignore]`, `skip_gate`,
      conditional fixture lookup and environment-gated assertion in the workspace
      and vendored compiler tests the workspace owns.
- [x] Give every entry an explicit class, owner task, required environment and
      release policy. Do not infer semantic versus infrastructure from substrings
      such as `needs to be implemented`.
- [x] Remove every ignored semantic, cost, receipt, state, consensus and
      interoperability differential; a known unequal answer must remain a failing
      test until fixed, not two accepted expectations. The current inventories
      contain no semantic or unclassified ignore and no declared known
      differential.
- [ ] Run infrastructure-only tests in named CI/release jobs with their fixtures or
      services. A missing input must produce `could not run` and a failed release,
      never a green test count.
- [x] Add a source-level gate that rejects a new ignore or conditional skip unless
      it is added to the inventory with an open owner task and explicit policy.
- [x] Reconcile the current ignored Hacknet, hosted-signer, miner, sync, Clarity
      cost and block-info tests; record which ordinary CI job and which release job
      exercises each one.
- [x] Make the release report consume the inventory and fail when any required
      entry did not run or any semantic entry still exists.

## Acceptance Criteria

- The release report accounts for every test that did not execute, by exact test
  name and owner task.
- No semantic or required conformance test remains ignored or conditionally green.
- Every infrastructure-only test has a reproducible command and a mandatory job
  that supplies its prerequisites before task 053 can complete.
- Adding an unowned `#[ignore]`, `skip_gate` or equivalent conditional fails CI.
- A release run with any missing required capture, network, signer or tool exits
  non-zero.

## Evidence that opened this task

The audit found ignored tests throughout the workspace, including a Clarity cost
test whose reason says the implementation is missing. `release-report` classifies
that semantic gap as infrastructure because the reason contains `needs to be
implemented`. Normal conformance also lets conditional mainnet assertions report
green when their capture is absent, while the committed release-report CI command
has no capture and is expected to fail. Reporting these individually is necessary,
but ensuring that required ones actually run needs an explicit gate of its own.

## The guess is gone, 2026-08-07

`ignored-tests.toml` is the inventory, keyed by the exact reason text, with a
class and an owner task per entry. `report_differentials` reads it and
`is_infrastructure` — the substring rule this task was opened over — is deleted.
A reason the inventory does not list is `unclassified`, which the report counts
against the release exactly as `semantic` does, so the undecided case cannot be
the quiet one.

What that turned up, by name and line rather than as a count:

```
ignored tests
  infrastructure       18 (a service, network or fixture this machine does not have)
  tools                1  (assert no required behaviour)
  blocking             15 -- each one is a failed release gate
    [semantic]     vendor/.../src/cost.rs:1895: Clarity 4 costs needs to be implemented
    [unclassified] vendor/.../blockinfo.rs (×12): test system needs to be improved
                     relative to versioning and epochs
    [unclassified] vendor/.../blockinfo.rs (×2): block-reward is not simulated in the
                     test framework
```

The first is the one the evidence named: `needs to be implemented` matched the
environment marker list, so a cost differential was filed as a missing machine. A
cost decides block admission even where the state root matches.

The fourteen `unclassified` are honest rather than resolved. Both reasons describe
the *harness* — which may mean they hide nothing, or may mean they hide a
per-epoch differential — and nobody has looked. Classing them by what their prose
sounds like is the mistake being removed, so they are recorded as undecided and
count against the release until somebody decides them. Owner 060.

**The source-level gate is in the mandatory suite.**
`release_inventory::every_ignored_test_is_named_in_the_inventory` fails on any
`#[ignore]` whose reason is not inventoried — verified by adding one and watching
it fail — and `the_inventory_names_no_test_that_is_gone` fails on an entry left
behind after its test was fixed, which is the direction that rots and would let a
later test inherit a waiver silently.

## What is still open

The source and policy inventories now agree on every ignore and conditional
site, and the blocking semantic count is zero. What remains is operational: run
all required infrastructure entries in the named release jobs with their real
captures, services and clients, and retain the report that proves none was
unexecuted.

## Classified by running them, not by reading them, 2026-08-07

The inventory is keyed by the **test's own name**, not its reason. Keying by
reason was the substring rule wearing a different hat: twelve sites share the
wording *"test system needs to be improved relative to versioning and epochs"*
and they are not one thing — some are words epoch 4.0 removed, and some are
`asserts!` and `as-contract`, which epoch 4.0 very much has.

Every entry was classified by running it (`cargo test -p clar2wasm
--all-features -- --ignored`) and reading what it did. The count went from 15
guesses to **5 measured semantic differentials**:

| test | measured |
|---|---|
| `contract_call_with_epoch_3_3` | `CostContractLoadFailure` on `costs-4`. The cost schedule epoch 4.0 runs under cannot be loaded, so nothing is crosschecked against it. Owner 023. |
| ~~`asserts_false`~~ | **Fixed and running.** `(asserts! false V)` raises `EarlyReturn(AssertionFailed(V))` — which is correct; `UnwrapFailed` is `try!`'s — and the thrown value's list type comes back narrowed to the data's own length. The engines *agree* on both: the failure was at the expected-value assertion, not the engine-divergence one, so only the hand-built expectation was stale. Rewritten as `crosscheck_compare_only_with_expected_error` and un-ignored. |
| ~~`asserts_with_begin_false`~~ | **Fixed and running**, same cause. |
| ~~`as_contract_can_return_any_value`~~ | **Out-of-scope, measured.** `UnknownFunction("as-contract")` in both engines, and clarity's registry settles it: `AsContract("as-contract", Clarity1, Some(Clarity3))` — removed after Clarity 3, replaced by `as-contract?`. Nano covers `as-contract` at Clarity 3 in `as_contract_sender` (which matters, because 064 compiles an old contract under its deployment epoch) and `as-contract?` in `allowance_principal`. |
| ~~`get_tenure_info_block_reward`~~ | **Covered now.** The clar2wasm harness genuinely cannot simulate a block reward, but nano can: its headers carry the number. `tenure_block_reward` builds a chain whose tenures earn different amounts and crosschecks both engines for every one of them, plus `miner-spend-total` beside it. The test stays ignored where it is; the behaviour is gated where it can be. |

Eleven are `out-of-scope`, and that is measured too rather than assumed: each
fails with `use of unresolved function` in **both** engines, because the harness
runs at epoch 4.0 / Clarity 6 where `at-block` and `get-block-info?` no longer
exist. A contract using one fails analysis in both engines on a 4.0 chain, so
nothing there can reach a receipt, and what 4.0 *does* do with `at-block` in an
older contract is nano's own unconditional `at_block_refusal` gate. The two
engines' error *envelopes* differ around the identical diagnostic; recorded as a
real difference, not blocking, and not hidden.

Eighteen are `infrastructure`, every one naming the job that supplies it, and
three of them are the stock-signer and stock-client journeys 053 requires by name.

`asserts_false` is a find rather than a reclassification: it had been sitting
behind a reason string that says the test system needs improving, and the test
system is not what is wrong with it.

**One remains, from fifteen.** `contract_call_with_epoch_3_3`: `costs-4` will not
load, so the cost schedule epoch 4.0 runs under is crosschecked against nothing.
That is task 023's work, not this task's.

Writing `tenure_block_reward` repeated the mistake this task exists to remove,
which is worth recording: its first draft asserted that tenure *H* answers
`reward_of(H)`, and it failed — on the *expected value*, with the engines in
agreement. `get-tenure-info?` resolves a tenure height through the tenure's first
block and tenure 0 has no answer at all. The assertion is agreement plus
*reading* now: the engines answer the same thing, the answers differ from tenure
to tenure, and every one is a reward some header was sealed with. A hand-derived
expectation is exactly what left `asserts_false` ignored.

## Historical follow-ups, reconciled

- `contract_call_with_epoch_3_3` is no longer a semantic entry. Its measured
  `out-of-scope` policy in `ignored-tests.toml` records that production always
  charges Epoch40, while the scoreboard and mainnet receipts cover that live
  schedule. `known-differentials.toml` is empty.
- Enabling every mutually exclusive `test-clarity-vN` feature together still
  makes the block-info harness select the wrong test version. That developer
  feature-matrix collision is neither an ignored/conditional release behavior
  nor a production configuration; the release-owned configurations remain the
  mandatory gates.
- `skip_gate` and `skip_diagnostic` are inventoried by stable call-site identity
  in `conditional-tests.toml`. Both conformance and `release-report` scan the
  current source against it and reject a missing, stale or unowned entry.

The only task item still open is therefore the infrastructure run named above.

## Infrastructure progress, 2026-08-13

The hosted Hacknet slice now ran against the current release candidate rather
than an old event directory. The pre-`f787569e` observer trees were preserved
under `run/evidence/pre-f787569e`; fresh stock and nano sinks then recorded the
new execution window. `hacknet/harness.sh verify-hosted`, held under the shared
Cargo build lock with one build job, ran all three inventoried hosted tests:

```text
a_stock_signer_answers_proposals_through_nano ... ok
a_transaction_posted_to_nano_is_admitted_and_reported ... ok
an_observer_on_nano_is_told_what_stacks_core_tells_its_own ... ok
test result: ok. 3 passed; 0 failed
the receipts agree for all 113 blocks both observers were told about
```

This proves those three infrastructure entries executed; it does not waive the
remaining mainnet, full-Hacknet and scratch-backed entries. The task and its last
checkbox remain open until the single release-qualification run accounts for all
of them.
