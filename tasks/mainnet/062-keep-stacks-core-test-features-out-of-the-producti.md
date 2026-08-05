---
id: "062"
title: "Keep stacks-core test features out of the production node"
status: in-progress
priority: critical
effort: medium
dependencies: ["061"]
tags: ["mainnet", "vm", "dependencies", "release", "conformance"]
created_at: 2026-08-04
type: bug
group: mainnet
---

# Keep stacks-core test features out of the production node

## Objective

Make the release node's allowed stacks-core dependency boundary explicit and
compile it without test or developer behavior. Reusing clarity-wasm necessarily
brings `clarity`, `clarity-types` and `stacks-common` into the VM ABI, but
vendored `clar2wasm` currently enables `clarity/testing` as a normal dependency.
Cargo feature unification consequently enables testing features in all three
reference crates inside `nano-node`.

Keep the reference frontend, values, database ABI and cost machinery required by
clarity-wasm. Move crosscheck helpers and reference oracles to test/dev tooling,
and prevent a release build from acquiring test schedules, faucets, overrides
or broad reference-node crates by accident.

## Tasks

- [x] Record the normal and feature dependency trees of `nano-node`, separating
      the minimal clarity-wasm ABI from conformance-only reference crates.
- [x] Remove `features = ["testing"]` from clar2wasm's normal `clarity`
      dependency; feature-gate or relocate the crosscheck utilities that need
      it.
- [ ] Disable unnecessary `stacks-common` and Clarity default/developer features
      in the production dependency closure and enable only the required ABI,
      database and cryptographic surfaces.
- [ ] Keep `stackslib`, `stacks-codec`, `libsigner`, `libstackerdb`, the
      reference PoX implementation from
      [[061-replace-stacks-core-pox-locking-with-nano-owned-ep]] and every
      test-only interpreter/healing entry point out of the release node's
      normal dependency graph and callable runtime surface.
- [x] Assert that production crates contain no reference to `eval_all`,
      interpreter contract-call execution, engine-selection APIs or the former
      interpreter environment switches. The Clarity interpreter may exist
      transitively for the frontend ABI, but the node must have no callable
      path to it under any feature or build profile.
- [x] Add a CI dependency/feature assertion for the release package so a future
      workspace or dev dependency cannot silently re-enable a forbidden crate
      or feature through unification.
- [x] Verify the production build observes real mainnet coinbase and SIP-031
      schedules and exposes no test override or faucet surface.
- [x] Preserve the full stacks-core differential suite in `nano-conformance` as
      dev-only tooling.

## Acceptance Criteria

- `cargo tree -p nano-node -e features` contains none of
  `clarity/testing`, `clarity-types/testing`, `stacks-common/testing` or
  `stacks-common/developer-mode`.
- The normal release graph contains only the documented stacks-core crates and
  features required by clarity-wasm; it contains no `stackslib`, `stacks-codec`,
  `libsigner`, `libstackerdb` or `pox-locking`.
- A release build cannot select test emission or coinbase schedules, mutate a
  global test override, or call a reference test faucet.
- A release or development build of the production node cannot select, invoke,
  crosscheck or fall back to the interpreter on any network or after any
  clarity-wasm failure.
- Conformance tests still use the pinned reference implementation without
  changing the release graph.
- The dependency assertion, release build, `clippy --all-targets --all-features`
  and tests pass from a clean checkout.

## What closed, and the two things left

`clar2wasm` asked for `clarity/testing` as a *normal* dependency, and Cargo
unifies features across a build graph — so `clarity`, `clarity-types` and
`stacks-common` were all built with their test behaviour inside `stacks-node`
itself. It is now a dev-dependency, which took two source changes: `to_ascii.rs`
used `TypeSignature::new_ascii_type_checked`, a testing-gated wrapper, and
`nano-vm` used `clarity::vm::ast::parse`, likewise. Both have non-testing
equivalents that report an invalid input rather than panicking on it
(`new_ascii_type`, `build_ast`), which is what the production paths wanted
anyway. clar2wasm's own 1,375 tests stay green.

`release_dependencies` asserts the boundary from `cargo tree`, because nothing
short of that actually knows: no `testing` feature on any reference crate, no
`stackslib`, `stacks-codec`, `libsigner` or `libstackerdb` in the normal graph,
and — the other half — `nano-conformance` still holding the reference
implementation, so the suite cannot report green by having stopped comparing.

**Deviation, deliberate.** The acceptance criteria ask for no
`stacks-common/developer-mode` either. It stays, and the test asserts it stays.
It is in `stacks-common`'s *default* features, so a stock mainnet node runs with
it, and all it does is keep source spans on AST nodes. Turning it off to tidy a
feature list would make nano's parser report differently from the network's for
no benefit, which is the opposite of what this task is for.

**Still open:** `pox-locking` is in the normal graph, and taking it out is
[[061-replace-stacks-core-pox-locking-with-nano-owned-ep]] rather than a
dependency edit — the node needs its own Epoch 4 lock/unlock semantics first.
