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

## Acceptance Criteria

- Both formatting checks exit successfully without modifying the tree.
- Both release/all-target clippy checks exit successfully with `-D warnings`.
- Scoreboard, fixture integrity, conformance, unit tests and offline release
  evidence have the exit statuses required by the workflow.
- A tracked pre-push hook runs the same checked-in gates as Actions and refuses
  a dirty or concurrently changing worktree.

## Evidence

- The pinned Rust 1.97.1 toolchain, `actionlint`, and `scripts/fmt.sh --check`
  pass; the formatter checks both Cargo workspaces.
- Release/all-target clippy passes with `-D warnings` in the root workspace and
  the vendored clarity-wasm workspace.
- The scoreboard reports 340/340 state, receipt, and cost matches and 500/500
  frozen-mainnet matches; execution-fixture validation and the 283 active
  conformance tests pass.
- `cargo test --release --workspace` passes, including the journal, catch-up,
  and cross-worktree compiler-identity regressions.
- The offline release report builds and inspects the artifact, then exits with
  the required non-qualifying status 2; all three pinned lockfiles are unchanged.
- The release-dependency checks force the workflow's `CARGO_TERM_COLOR=always`
  locally and request uncoloured `cargo tree` output before parsing it.
- `scripts/ci.sh` is the single implementation of the offline gates used by
  both Actions and `.githooks/pre-push`; it exports the workflow colour setting.
- The tracked hook refuses a dirty tree before running and refuses a changed
  HEAD or worktree afterward. Entering the Nix shell installs it via
  `core.hooksPath`.
