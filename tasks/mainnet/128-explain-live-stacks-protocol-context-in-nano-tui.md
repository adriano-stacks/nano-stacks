---
id: "128"
title: "Explain live Stacks protocol context in nano-tui"
status: in-progress
priority: high
effort: medium
type: feature
group: mainnet
dependencies: ["126"]
tags: ["tui", "ux", "education", "sortition"]
touches: ["crates/nano-tui"]
created_at: 2026-08-14
verify:
  - type: bash
    run: "nix develop -c cargo test -p nano-tui"
  - type: bash
    run: "nix develop -c cargo clippy -p nano-tui --all-targets -- -D warnings"
---

# Explain live Stacks protocol context in nano-tui

## Objective

Make the live relationship between Bitcoin decisions, miner commitments,
tenures and Stacks blocks understandable without prior Stacks vocabulary.

This is delivery slice 3 from task 125's usability study.

## Tasks

- [x] Organize the primary views as Overview, Activity, Election and Operations,
      with stable number-key navigation.
- [x] Rename Mining to Election and keep the network miner distinct from this
      process's roles.
- [x] Render the latest Bitcoin block -> commitment -> tenure -> Stacks-block
      relationship as one causal story, followed by what happens next.
- [x] Add contextual `?` help for every view: meaning, relevance, provenance and
      local keys.
- [x] Define every visible protocol term, starting with burn block, tenure,
      extension, election/sortition, fork choice, signer, PoX phase, state root
      and uSTX.
- [ ] Prefer human units and times while retaining exact values in details.
- [x] Retain commitment sampling and relative-weight detail without describing
      the value as win probability.
- [ ] Cover help/navigation, missing context and the full story at supported
      terminal sizes.
- [ ] Run rustfmt, tests and strict clippy without warnings.

## Acceptance Criteria

- Overview says what Bitcoin decided, what tenure it affected, which Stacks
  blocks belong to it and which boundary comes next.
- A user can open a definition and data provenance from every visible panel
  without leaving the TUI.
- Plain-language summaries do not remove exact hashes, commitments, burn values
  or sample-window context from detail views.
- STX values lead with STX and preserve exact uSTX; timestamps show an absolute
  time and relative age rather than only a Unix integer.
- Election wording cannot imply that this node mined or that relative weight is
  a probability.
