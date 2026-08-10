---
title: "Load the fastpool signer manager that mainnet executed"
id: "111"
status: in-progress
priority: critical
type: bug
tags: ["mainnet", "wasm", "consensus", "release"]
created_at: "2026-08-10"
parent: 053
effort: medium
---

# Load the fastpool signer manager that mainnet executed

## Steps to Reproduce

1. Run release artifact `063215c6` from the state sealed at mainnet block
   8,733,928.
2. Follow and execute block 8,733,929.
3. Observe transaction
   `6f8bf2076f0c563921be39631a4bd7643ccb093ea9935324502e3f7b59329e93`
   call `SPMPMA1V6P430M8C91QS1G9XJ95S59JS1TZFZ4Q4.fastpool-max500-signer-manager`.

## Expected Behavior

The recorded-epoch compiler emits a valid Wasm module, the transaction agrees
with the pinned interpreter on its result, receipt, all five costs, events and
writes, and the block seals at the network's state root.

## Actual Behavior

Wasmtime refuses the generated module at offset `0xa0de`: `expected i32, found
i64`. The engine fails closed and leaves the node sealed at 8,733,928.

## Tasks

- [x] Reduce the exact source expression that leaves the invalid stack shape
      and add a focused compiler regression.
- [x] Fix the general lowering rule rather than special-case this contract.
- [x] Prove compiler/interpreter parity for result, receipt, costs, events and
      writes on the real transaction.
- [ ] Replay block 8,733,929 with root verification, then show the same deployed
      release artifact follows past it.

## Evidence

- Clean release commit: `063215c6f4e62f78901c844b86c67fe9e43719d7`.
- Artifact SHA-256:
  `ef6629d966e110974beabc543a51399f451de7a41875e70379b1441f8c82acae`.
- Live fail-closed log: `/home/aldur/mainnet-tip/run-155042-docker.log`.
- Reduced cause: a local private call passed a let-bound tuple containing
  `bond-index: none` directly into a parameter whose field is `(optional uint)`.
  Replacing that argument with the widened literal made the real module load.
- Compiler checkpoint: `43eefade` (`vm: widen local call arguments to parameter
  ABI`). The focused regression, all 36 `wasm_generator` tests, all 1,475 library
  tests, and strict all-target clar2wasm Clippy are green.
- Recorded-epoch `check-module` now loads the exact signer-manager contract from
  `/home/aldur/nano-111.QRgHwq/state`. `call-both-tx` on the staged transaction
  reports the same `ArithmeticUnderflow` outcome from both engines and zero
  result disagreements.
- Exact-state checkpoint `cf561365` replays checked-in block 8,733,929 twice
  under `RootPolicy::Verify`. At transaction index 1, the compiler and pinned
  interpreter agree on the serialized result, all five costs
  `(2049, 274713, 2869483, 1062, 123401)`, both ordered events and the complete
  asset-write map. Both block passes seal the header's committed root, and the
  immutable source database stamps remain unchanged.
