---
id: "061"
title: "Replace stacks-core pox-locking with nano-owned Epoch 4 logic"
status: pending
priority: critical
effort: large
dependencies: ["009"]
tags: ["mainnet", "vm", "pox", "clarity", "conformance"]
created_at: 2026-08-04
type: improvement
group: mainnet
---

# Replace stacks-core pox-locking with nano-owned Epoch 4 logic

## Objective

Remove the production dependency on stacks-core's `pox-locking` crate. Nano
currently installs `pox_locking::handle_contract_call_special_cases` directly
as its Clarity backing store hook, so native lock, update, unstake and PoX event
semantics are executed by roughly 6,790 lines of the reference node.

Implement the epoch-4 boundary nano actually needs using nano-owned code and the
existing Clarity database ABI. The reference crate may remain a dev-only oracle
while the behavior is established; it must not be linked by `nano-node`. This
closes W7 in `plan.md`, which called for reimplementing these native side
effects rather than merely wiring the reference handler.

## Tasks

- [ ] Inventory every PoX-5 function whose successful or failed response causes
      a native balance, lock schedule or event side effect in epoch 4.
- [ ] Implement strict parsing of the PoX-5 response tuples and reproduce the
      reference error boundary for malformed responses versus ordinary
      contract `(err ...)` values.
- [ ] Implement stake and bond registration, stake updates and unstake over the
      Clarity account snapshot API, including amount and unlock-height
      invariants.
- [ ] Emit the same native lock and PoX action events, including the epoch-4
      action-only functions that do not alter a lock.
- [ ] Define the epoch-4 behavior of calls to defunct PoX-1 through PoX-4
      contracts so an old contract cannot bypass the native boundary.
- [ ] Differential-test the nano handler against pinned `pox-locking` as a
      dev-only oracle for success, contract error, malformed response, overflow,
      insufficient balance, existing lock and missing lock cases.
- [ ] Replay captured mainnet PoX-5 transactions and compare account state,
      events, receipts and state roots before removing the reference handler.
- [ ] Remove `pox-locking` from `nano-vm`'s normal dependencies and make the
      release dependency audit in
      [[062-keep-stacks-core-test-features-out-of-the-producti]] reject it.

## Acceptance Criteria

- `cargo tree -p nano-node --edges normal` contains no `pox-locking` package.
- No source from `pox-locking` is vendored or copied into a production crate;
  the reference implementation is used only through differential tests.
- Every supported PoX-5 transition produces the same balances, lock heights,
  events, errors and MARF writes as stacks-core over captured and adversarial
  cases.
- Calls to obsolete PoX contracts behave exactly as epoch 4 requires.
- Mainnet replay passes the captured PoX window and the workspace is clean
  under `fmt`, `clippy --all-targets --all-features` and tests.
