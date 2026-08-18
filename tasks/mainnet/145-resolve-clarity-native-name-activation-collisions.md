---
id: "145"
group: mainnet
title: "Resolve Clarity native-name activation collisions"
status: completed
priority: critical
effort: small
type: bug
tags: ["mainnet", "vm", "clarity-wasm", "consensus", "liveness", "release"]
created_at: "2026-08-18"
completed_at: 2026-08-18
---

# Resolve Clarity native-name activation collisions

## Objective

Execute an older contract's user function when a later Clarity version assigns
the same name to a native function.

## Evidence

Mainnet block 8791626 calls the Clarity 2 contract
`SP2PABAF9FTAJYNFZH93XENAJ8FVY99RRM50D2JG9.clarity-bitcoin-lib-v7`. Its local
`verify-merkle-proof` takes three arguments. Clarity 6 later reserved that name
for a five-argument native function.

The interpreter executes the local function and the block commits state root
`a2a46343cd6a5a37bc9b9589ddfe26f8f4df70851b138e7ed8b7dec7cf311ee6`.
clarity-wasm selected the newer native regardless of the contract's recorded
Clarity version, returned `IncorrectArgumentCount(5, 3)`, and produced root
`b4299a8053c9f83f2e180fde536351947c3abf8ed9dbe0b83266b31dd8712f8f`.

## Tasks

- [x] Reproduce the interpreter/Wasm disagreement with a minimal Clarity 5 contract.
- [x] Resolve native functions at the contract's recorded Clarity version.
- [x] Execute mainnet block 8791626 to its committed root.
- [x] Run the strict Clippy and test gates.

## Acceptance Criteria

- An older contract can call a user function whose name becomes native in a
  later Clarity version.
- A Clarity 6 contract still resolves `verify-merkle-proof` to the native.
- Mainnet block 8791626 executes with the committed receipt and state root.
- Strict Clippy and the affected test suites pass without warnings.
