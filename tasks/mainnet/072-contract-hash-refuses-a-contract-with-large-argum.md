---
id: "072"
title: "A generated contract with large argument types fails to deploy"
status: pending
priority: high
effort: medium
dependencies: []
tags: ["mainnet", "vm", "clarity", "conformance"]
created_at: 2026-08-07
type: bug
---

# contract-hash refuses a contract with large argument types

## Objective

`clar2wasm`'s `contracts::contract_hash_returns_correct_hash_for_any_contract`
fails: `contract-hash?` answers `(err u2)` where the test expects the contract's
hash. Make it answer the hash, or establish that `(err u2)` is what stacks-core
answers and fix the expectation.

## What `(err u2)` turned out to mean

`err u2` is `contract-hash?`'s *contract missing* (`linker.rs`, "contract missing
=> (err u2)"). So the caller deployed and ran fine; what failed is the **callee's
own deployment**, and `contract-hash?` is only the messenger. The title's original
guess -- that hashing was refusing -- was wrong.

That reframes the whole task. Both engines refuse to deploy the generated callee,
and `crosscheck_multi_contract` compares the two engines per contract *before* it
compares against the expectation, so that half passes: nano and the reference agree
the contract does not deploy. The property then asks for its hash anyway.

The suspect is therefore the **expectation**, not the engines: a generated contract
that legitimately fails to deploy has no hash to return. What has to be established
is *why* it fails -- a size, a cost or an analysis bound on those argument types --
and whether stacks-core refuses the same deployment. If it does, the property must
stop expecting a hash for a contract that never existed; if it does not, the
refusal is the bug.

## What is known

- **Both engines agree.** `crosscheck` compares compiled against interpreted
  before it compares against the expectation, and that comparison passes. So this
  is not a nano-versus-reference divergence — it is both engines answering
  `(err u2)` where the test says a hash.
- **It is pre-existing.** The last commit to that test file is `a9c73a4e`
  (2026-07-30, *"fix: size a contract for the arguments it is given"*), which
  predates this session, and clar2wasm depends on none of the crates changed in
  it. It is surfaced rather than caused: the property draws a fresh contract each
  run and had not drawn this shape before.
- **The shape is the clue, and it is the one that commit is about.** The minimal
  failing contract is a `define-public` taking three arguments whose types are
  large: two tuples of five and three fields including `(string-ascii 58)`,
  `(string-utf8 19)` and `(buff 105)`, plus an `(optional uint)`.

## Tasks

- [x] Reproduce with the minimal input alone, outside the property, so the failure
      is a fixture rather than a seed. It reproduces on every run rather than by
      seed, and the minimal contract is recorded below.
- [ ] Capture the callee's own deployment error rather than the caller's `(err u2)`,
      which only says the contract is absent. That error names the bound.
- [ ] Establish what stacks-core answers for the same contract, since both engines
      agreeing means the expectation is as likely to be wrong as the engines.
- [ ] Name what `(err u2)` is: which `contract-hash?` failure it encodes and which
      size or bound produced it for these argument types.
- [ ] Fix the owning side — the sizing introduced by `a9c73a4e`, or the test's
      expectation — and keep the property, so the next shape it draws is still
      checked.
- [ ] Confirm no captured mainnet block reaches the shape, and say so in the
      release accounting either way.

## Acceptance Criteria

- The property passes across runs rather than by seed.
- `contract-hash?` on a contract with large argument types answers what
  stacks-core answers, asserted against it rather than against a guess.
- No red test remains in the workspace suite.
