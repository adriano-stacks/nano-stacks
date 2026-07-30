---
id: "032"
title: "Settle the toolchain version in one place"
status: pending
priority: low
effort: small
type: chore
dependencies: []
tags: ["notes", "build"]
created_at: 2026-07-30
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

- [ ] Decide whether `rust-version` in `Cargo.toml` is doing anything that
      `rust-toolchain.toml` is not.
- [ ] Make the flake supply the toolchain `rust-toolchain.toml` names, rather
      than a parallel one.
- [ ] Remove whichever of the three is redundant.

## Acceptance Criteria

- `rustc --version` in the nix shell is the version the repository asks for.
- The version is stated once.
