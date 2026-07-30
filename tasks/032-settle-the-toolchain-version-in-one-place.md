---
id: "032"
title: "Settle the toolchain version in one place"
status: completed
priority: low
effort: small
type: chore
dependencies: []
tags: ["notes", "build"]
created_at: 2026-07-30
completed_at: 2026-07-30
---

# Settle the toolchain version in one place

## Objective

From the code review in `notes.md`.

Three places have an opinion about which Rust builds this and none of them agree:
`Cargo.toml` pins `rust-version = "1.85"`, `rust-toolchain.toml` asks for
`channel = "stable"`, and `flake.nix` supplies `pkgs.rustc` and `pkgs.cargo` from
whatever nixpkgs is pinned to. The workspace is on edition 2024.

One of these should decide, and the others should follow it or go away.

## Tasks

- [x] Decide whether `rust-version` in `Cargo.toml` is doing anything that
      `rust-toolchain.toml` is not.
- [x] Make the flake supply the toolchain `rust-toolchain.toml` names, rather
      than a parallel one.
- [x] Remove whichever of the three is redundant.

## Acceptance Criteria

- `rustc --version` in the nix shell is the version the repository asks for.
- The version is stated once.

## What was decided

`rust-version` was the redundant one. It declares a minimum this repository
never builds against — CI and the shell both run whatever the pinned nixpkgs
provides — so it was an unverified claim, not a constraint. `edition = "2024"`
already requires 1.85 and the compiler enforces that.

`rust-toolchain.toml` stays: it is what rustup reads outside the nix shell, and
it asks for the same channel the flake supplies. The flake now says so where
someone changing it will look.
