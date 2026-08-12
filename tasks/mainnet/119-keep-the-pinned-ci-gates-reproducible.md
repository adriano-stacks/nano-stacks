---
title: "Keep the pinned CI gates reproducible"
id: "119"
status: completed
priority: high
effort: small
type: bug
group: mainnet
tags: ["ci", "tooling", "release", "gates"]
created_at: "2026-08-11"
verify:
  - type: bash
    run: "nix develop --command scripts/ci.sh formatting"
  - type: bash
    run: "nix develop --command scripts/ci.sh workflow"
  - type: bash
    run: "test \"$(git config --local --get core.hooksPath)\" = .githooks"
completed_at: 2026-08-12
---

# Keep the pinned CI gates reproducible

## Objective

Keep the checked-in workspace green under the exact offline commands in
`.github/workflows/gates.yml`, and catch formatting failures before they reach a
hosted runner.

## Tasks

- [x] Reproduce the CI failure under the pinned Nix toolchain.
- [x] Format both the workspace and the independently vendored clarity-wasm
      workspace.
- [x] Fix every clippy warning with `-D warnings` for both workspaces.
- [x] Run the remaining offline gates exactly as CI runs them.
- [x] Add a cheap regression check for the failure mode.
- [x] Share one checked-in gate driver between Actions and pre-push validation.
- [x] Refuse pushes from a dirty or concurrently changing worktree.
- [x] Verify the exact staged tree before commit and admit only that tree at push.
- [x] Keep vendor tests outside the root release-LTO workspace without losing them.
- [x] Avoid production LTO, example links, and duplicate conformance in tests.

## Acceptance Criteria

- Both formatting checks exit successfully without modifying the tree.
- Both release/all-target clippy checks exit successfully with `-D warnings`.
- Scoreboard, fixture integrity, conformance, unit tests and offline release
  evidence have the exit statuses required by the workflow.
- A tracked pre-commit hook runs the same checked-in gates as Actions against a
  fully staged tree. Pre-push admits only that recorded tree and refuses a dirty
  worktree.

## Evidence

- The pinned Rust 1.97.1 toolchain, `actionlint`, and `scripts/fmt.sh --check`
  pass; the formatter checks both Cargo workspaces.
- Release/all-target clippy passes with `-D warnings` in the root workspace and
  the vendored clarity-wasm workspace.
- The scoreboard reports 340/340 state, receipt, and cost matches and 500/500
  frozen-mainnet matches; execution-fixture validation and the 283 active
  conformance tests pass.
- The optimized CI profile runs all library, binary, integration, and documentation
  tests without production LTO or duplicate conformance execution.
- The consolidated test gate passes in 14m08s from cold vendor artifacts: 284
  active conformance tests, all root unit/integration/doc tests, 1,475 active
  vendor library tests, and every vendor integration/property/OOM suite.
- The offline release report builds and inspects the artifact, then exits with
  the required non-qualifying status 2; all three pinned lockfiles are unchanged.
- The release-dependency checks force the workflow's `CARGO_TERM_COLOR=always`
  locally and request uncoloured `cargo tree` output before parsing it.
- `scripts/ci.sh` is the single implementation of the offline gates used by
  both Actions and `.githooks/pre-commit`; it exports the workflow colour setting.
- The tracked pre-commit hook requires a fully staged tree, records the verified
  tree under `.git`, and fails if the index or worktree changes during the run.
  Pre-push admits only that tree. Entering the Nix shell installs both hooks via
  `core.hooksPath`.
- The root workspace explicitly excludes the vendored clarity-wasm workspace.
  Its tests still run through its own manifest, without the root release
  profile's ThinLTO and single codegen unit.
