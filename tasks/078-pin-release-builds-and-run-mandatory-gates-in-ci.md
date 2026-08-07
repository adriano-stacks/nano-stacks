---
id: "078"
title: "Pin release builds and run mandatory gates in CI"
status: pending
priority: high
effort: medium
type: chore
group: build
dependencies: ["032", "033"]
tags: ["build", "ci", "release", "reproducibility"]
created_at: 2026-08-07
---

# Pin release builds and run mandatory gates in CI

## Objective

Make a clean checkout select one immutable toolchain and automatically run the
repository's required gates. A local cache or ignored lock file must not decide
which compiler produced release evidence.

## Tasks

- [ ] Track `flake.lock` and pin `nixpkgs` to an immutable revision.
- [ ] Pin an exact Rust toolchain and make the Nix shell and non-Nix setup select
      the same compiler, Cargo, Clippy and rustfmt versions.
- [ ] Add repository-root CI for formatting, release workspace Clippy, the full
      conformance suite, the scoreboard and the release report's offline gates.
- [ ] Require the scoreboard and release report commands to propagate failures;
      CI must not infer success by parsing a table.
- [ ] Build the release artifact from the checked-out revision before hashing or
      inspecting it, then verify its embedded compiler identity.
- [ ] Make `cargo fmt --all -- --check` clean for the workspace, including the
      vendored compiler sources the workspace owns.
- [ ] Prove `nix develop` changes no tracked or ignored repository file in a clean
      checkout.

## Acceptance Criteria

- Two clean checkouts select the same Nix inputs and exact Rust toolchain without
  generating or rewriting a lock file.
- Root CI runs every required offline gate and blocks a deliberately introduced
  formatting, Clippy, replay or conformance failure.
- Release artifact identity is tied to the checked-out source and compiler, not
  to a pre-existing `target/` entry.
- `cargo fmt --all -- --check` and release workspace Clippy both pass.

## Evidence that opened this task

The repository has no root CI configuration, ignores `flake.lock`, follows
`nixos-unstable`, and asks rustup for floating `stable`. Nix regenerated the
ignored lock during the audit, while `cargo fmt --all -- --check` failed across
vendored clarity-wasm and workspace tooling.
