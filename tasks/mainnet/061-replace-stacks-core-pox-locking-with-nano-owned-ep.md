---
id: "061"
title: "Replace stacks-core pox-locking with nano-owned Epoch 4 logic"
status: completed
priority: critical
effort: large
dependencies: ["009"]
tags: ["mainnet", "vm", "pox", "clarity", "conformance"]
created_at: 2026-08-04
type: improvement
group: mainnet
completed_at: 2026-08-05
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

- [x] Inventory every PoX-5 function whose successful or failed response causes
      a native balance, lock schedule or event side effect in epoch 4.
- [x] Implement strict parsing of the PoX-5 response tuples and reproduce the
      reference error boundary for malformed responses versus ordinary
      contract `(err ...)` values.
- [x] Implement stake and bond registration, stake updates and unstake over the
      Clarity account snapshot API, including amount and unlock-height
      invariants.
- [x] Emit the same native lock and PoX action events, including the epoch-4
      action-only functions that do not alter a lock.
- [x] Define the epoch-4 behavior of calls to defunct PoX-1 through PoX-4
      contracts so an old contract cannot bypass the native boundary.
- [x] Differential-test the nano handler against pinned `pox-locking` as a
      dev-only oracle for success, contract error, malformed response, overflow,
      insufficient balance, existing lock and missing lock cases.
- [x] Replay captured PoX-5 transactions and compare account state,
      events, receipts and state roots before removing the reference handler.
- [x] Remove `pox-locking` from `nano-vm`'s normal dependencies and make the
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

## What nano owns now

`crates/nano-vm/src/pox.rs`, about 500 lines against the reference crate's 5,848,
because a 4.0-only node needs one PoX contract's lock semantics and not five.
`pox-5` gets the real thing — `stake` and `register-for-bond` (fresh lock or
roll-over), `stake-update` (extend and raise), `unstake` (bring the unlock
forward), the `STXLockEvent` each emits, the `log_stacking` asset-map entry, the
gated `log_pox_action` for the four position-altering names, and the strict
response parse. `pox` through `pox-4` collapse to one question: does this
function read, or write? A read still answers, because `get-pox-info` is how a
client asks what happened to an old position; a write is `DefunctPoxContract`.

The read-only lists are each contract's own and are *not* interchangeable —
`get-pox-rejection` is on pox-1's, pox-2's and pox-3's and not pox-4's;
`verify-signer-key-sig` and `check-caller-allowed` are on pox-4's alone. The test
asserts exactly those asymmetries, because a shared list would look right and
turn a rejection into a state change.

`pox_locking` is now a dev-dependency of `nano-conformance` and nothing else.
`pox_locking.rs` runs fifteen cases through both handlers against two identically
funded stores and compares the balance, the unlock height, the events and whether
there was an error: fresh stake, bond registration, roll-over up and down,
roll-over that fails to move the unlock forward, extend, extend that lowers,
extend with no lock, unstake, unstake with no lock, over-balance, contract
`(err …)`, ten malformed-response shapes, no-effect functions, and the defunct
contracts' reads and writes. All fifteen agree, and `cargo tree -p nano-node
--edges normal` no longer contains `pox-locking`.

**One case is asserted rather than compared, deliberately.** stacks-core gates
pox-1 on its v1 unlock height against the *current burn height*; an in-memory
store reports height 0, so the reference concludes pox-1 is still live and tries
to apply a v1 lock. nano asserts what every real chain says — mainnet's v1 unlock
was in epoch 2.1 — because a node with no pox-1 lock code has no useful answer
for a chain where pox-1 is live, and reading the height would only let it write a
lock this module cannot compute. Comparing the two there would be comparing
environments rather than handlers, so pox-1's reads and writes are asserted
directly and the reason is in both files.

## The captured stake window before mainnet's first pox-5 reward cycle

At the replay frontier, mainnet's reward cycle 140 had been prepared under
`pox-4`, so it could not supply a pox-5 reward set or signer window even though
Epoch 4.0 and the pox-5 contract were active. Pox-5's first mainnet reward cycle
is 141. Waiting for that boundary would have left the handler with no chain-level
oracle in the meantime, so the implementation used the captured stake window
below; [[052-wire-the-complete-rpc-and-event-surface-into-the-n]] and
[[053-pass-the-mainnet-node-release-gate]] retain the live cycle-141 gate.

The captured chain *does* stake. Fourteen `stx_lock_event`s across eight of its
340 blocks, three stakers rolling their positions forward through five reward
cycles — a fresh lock, then four roll-overs each, from 420 to 500. So the window
exists; it was simply in the capture rather than on mainnet.

`pox_five_replay.rs` replays it and compares nano's own locks — read out of its
receipts, not out of its handler — against the ones the network published, in
order: contract, address, amount, unlock height. All fourteen agree, and the
state root and the whole receipt including these events already agreed at every
one of the 340 blocks, which is what `cargo xtask scoreboard` reports.

Two things about the test are deliberate:

- **It asserts the capture contains locks before comparing anything.** Without
  that, a recapture that lost the stake window would leave this test green while
  it compared two empty lists — and every other test in the suite green too,
  since none of the rest looks at a lock. That is exactly the failure the
  ground-truth strategy in `plan.md` exists to refuse.
- **The captured events are sorted by `event_index` first.** The capture's JSON
  array is not in event order and nano's is, because nano's comes from the
  receipts in the order the block executed them. Comparing the two unsorted
  disagreed on two blocks out of eight — same locks, different order — which is
  a difference in the fixture's serialization and not in the handler.

So this is now checked two ways that fail differently: `pox_locking.rs` against
stacks-core's handler on synthetic and adversarial responses, and this against
the chain on the responses a chain actually produced.

## Remaining

Nothing on the dependency-removal implementation. What remains next door is
proof against mainnet's first pox-5 reward cycle, cycle 141. That is recorded on
[[052-wire-the-complete-rpc-and-event-surface-into-the-n]] and
[[053-pass-the-mainnet-node-release-gate]] because it gates the signer set and
client-facing behavior rather than whether `pox-locking` remains in production.
