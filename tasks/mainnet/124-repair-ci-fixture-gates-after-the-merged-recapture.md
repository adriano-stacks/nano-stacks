---
title: "Repair CI fixture gates after the merged recapture"
id: "124"
status: in-progress
priority: high
effort: small
type: bug
group: mainnet
tags: ["ci", "conformance", "fixtures"]
created_at: "2026-08-14"
context: [".github/workflows/gates.yml", "crates/nano-conformance/fixtures", "crates/nano-conformance/tests/conformance"]
verify:
  - type: bash
    run: "nix develop --command scripts/ci.sh tests"
  - type: bash
    run: "nix develop --command scripts/ci.sh fixtures"
  - type: bash
    run: "nix develop --command scripts/ci.sh clippy"
---

# Repair CI fixture gates after the merged recapture

## Objective

Make the hosted CI gates pass with the checked-in capture.

## Tasks

- [x] Restore the burn context that the new capture states.
- [x] Select current reward and waterfall evidence by its data, not old heights.
- [x] Check in a real PoX-5 lock window.
- [x] Keep the fixed PoX-5 replay test on the checked-in fixture.
- [ ] Run the complete local CI gates.

## Acceptance Criteria

- The workspace and conformance tests pass.
- Fixture validation passes.
- Strict Clippy passes without a warning.
- The full local CI workflow passes.

## Outcome

Pending the complete local CI run.
