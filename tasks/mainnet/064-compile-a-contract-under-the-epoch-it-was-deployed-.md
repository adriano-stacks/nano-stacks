---
id: "064"
title: "Compile a contract under the epoch it was deployed in"
status: pending
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

- [ ] Find what `reserve-v1` uses that epoch 4.0 rejects, and how many mainnet
      contracts the replay hits it for. Report the count, not an example.
- [ ] Read the deploy epoch out of the stored contract analysis rather than
      searching for an epoch that compiles.
- [ ] Preserve and verify the existing separation between language semantics
      (deploy epoch) and execution charging (current epoch). Remove the stale
      comments and pin a receipt/cost regression proving an old contract called
      in Epoch 4.0 is charged with the Epoch 4.0 schedule.
- [ ] Include the chain-derived semantic epoch and compiler identity in native
      module cache identity or otherwise prove a cached module cannot outlive
      either input.
- [ ] Make a contract whose deploy epoch is unavailable reject the block rather
      than execute under a guessed epoch, per [[060]]'s boundary.
- [ ] Pin it: a contract using a removed word, deployed before its removal,
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
