---
id: "078"
title: "Pin release builds and run mandatory gates in CI"
status: in-progress
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

- [x] Track `flake.lock` and pin `nixpkgs` to an immutable revision.
- [ ] Pin an exact Rust toolchain and make the Nix shell and non-Nix setup select
      the same compiler, Cargo, Clippy and rustfmt versions.
- [x] Add repository-root CI for formatting, release workspace Clippy, the full
      conformance suite, the scoreboard and the release report's offline gates.
- [x] Require the scoreboard and release report commands to propagate failures;
      CI must not infer success by parsing a table.
- [ ] Build the release artifact from the checked-out revision before hashing or
      inspecting it, then verify its embedded compiler identity.
- [ ] Make `cargo fmt --all -- --check` clean for the workspace, including the
      vendored compiler sources the workspace owns.
- [x] Prove `nix develop` changes no tracked or ignored repository file in a clean
      checkout.

## Where this stands, 2026-08-07

**Done.**

- `flake.lock` was **gitignored**, and `nixpkgs` pointed at the moving
  `nixos-unstable` branch -- so every `nix develop` re-resolved it and printed
  `updating lock file` on the way, and two clean checkouts a day apart could build
  release evidence with different compilers. The input is pinned to the revision the
  lock held and the lock is tracked. A second `nix develop` now prints nothing, which
  is the check for the last bullet as well.
- `.github/workflows/gates.yml` runs the offline gates on push and pull request:
  clippy over the release profile and every target, the bounded fixture replay, the
  fixture integrity check, the conformance suite, the unit tests and the release
  report. Nothing greps output -- [[075-make-the-consensus-scoreboard-an-authoritat]]
  made `scoreboard` and `release-report` propagate failure through their exit status,
  and a job that looked for a word would go green the moment the wording changed.
  The workflow also fails if a run rewrites `flake.lock`.

**Open, deliberately.** The formatting gate is present but `continue-on-error`, and
that is honest rather than convenient: the workspace has never been `cargo fmt`-clean
-- 86 files under `crates/`, 8 in the vendored compiler. Running it produces a
ninety-file mechanical commit, it reflowed a conformance test past clippy's
hundred-line limit on the attempt made here, and the vendored sources are being
edited concurrently under [[073]]. It has to land on a quiet tree as its own commit,
after which the `continue-on-error` comes off.

Still owed: building the artifact from the checked-out revision before hashing it,
and verifying its embedded compiler identity, which belongs with
[[074-make-the-release-report-readable-and-its-fixtures-b]]'s artifact bullets.

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
