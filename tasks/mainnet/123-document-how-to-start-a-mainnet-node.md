---
id: "123"
title: "Document how to start a mainnet node"
status: completed
priority: high
effort: small
type: docs
group: mainnet
dependencies: []
tags: ["mainnet", "documentation", "operations"]
created_at: 2026-08-14
context: ["README.md", "docs/running-a-mainnet-node.md", "docs/build-mainnet-checkpoint.md", "docs/mainnet-node.example.toml", "flake.nix"]
verify:
  - type: bash
    run: "rg -q 'Run a mainnet node' README.md && rg -q 'Hiro mainnet archive' docs/build-mainnet-checkpoint.md && rg -q 'capture-fixtures' docs/build-mainnet-checkpoint.md && rg -q 'verify-block' docs/build-mainnet-checkpoint.md"
  - type: bash
    run: "nix develop --command cargo run --quiet -p nano-node --bin stacks-node -- check-config --config docs/mainnet-node.example.toml"
  - type: bash
    run: "nix develop --command zstd --version"
completed_at: 2026-08-14
---

# Document how to start a mainnet node

## Objective

Give an operator one clear path to build, configure, start, check, stop, and
restart a mainnet follower. Give exact steps to convert a Hiro archive into a
nano-stacks checkpoint bundle.

## Tasks

- [x] Add a full mainnet configuration example.
- [x] List every file that the checkpoint bundle must contain.
- [x] Document the build, start, health check, stop, and restart commands.
- [x] State the current limits and link the trust and peer guides.
- [x] Link the new guide from the README.
- [x] Explain what a Hiro archive contains and what trust it does not provide.
- [x] Add exact download, checksum, extraction, and conversion commands.
- [x] Add an independent signer-set check and checkpoint assembly commands.

## Acceptance Criteria

- A reader can find the guide from the README.
- The guide does not claim that this repository ships a mainnet checkpoint.
- The example uses mainnet network values and passes `stacks-node check-config`.
- The guide explains how to check that the executed height is moving.
- The guide warns that mainnet release tests are not complete.
- A reader can convert the current Hiro mainnet archive into the files used by
  the example configuration.
- A reader checks the checkpoint block with a signer set from another source.

## Outcome

The README links the mainnet follower and checkpoint guides. The checkpoint
guide converts the latest Hiro archive, checks the download, builds the bundle,
and checks its signed block with a signer set from another source. The example
configuration passes the node parser, and the Nix shell supplies every named
tool.
