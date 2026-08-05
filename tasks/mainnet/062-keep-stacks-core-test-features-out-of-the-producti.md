---
id: "062"
title: "Keep stacks-core test features out of the production node"
status: pending
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

- [ ] Record the normal and feature dependency trees of `nano-node`, separating
      the minimal clarity-wasm ABI from conformance-only reference crates.
- [ ] Remove `features = ["testing"]` from clar2wasm's normal `clarity`
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
- [ ] Assert that production crates contain no reference to `eval_all`,
      interpreter contract-call execution, engine-selection APIs or the former
      interpreter environment switches. The Clarity interpreter may exist
      transitively for the frontend ABI, but the node must have no callable
      path to it under any feature or build profile.
- [ ] Add a CI dependency/feature assertion for the release package so a future
      workspace or dev dependency cannot silently re-enable a forbidden crate
      or feature through unification.
- [ ] Verify the production build observes real mainnet coinbase and SIP-031
      schedules and exposes no test override or faucet surface.
- [ ] Preserve the full stacks-core differential suite in `nano-conformance` as
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
