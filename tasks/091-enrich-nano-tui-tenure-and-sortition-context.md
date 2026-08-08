---
title: "Enrich nano-tui tenure and sortition context"
id: "091"
status: completed
priority: high
effort: small
type: feature
group: mainnet
dependencies: ["090"]
tags: ["tui", "ux", "explorer"]
touches: ["crates/nano-tui"]
created_at: "2026-08-07"
---

# Enrich nano-tui tenure and sortition context

## Objective

Add compact, contextual tenure and burnchain details without putting raw
consensus internals back into the global sync summary.

## Tasks

- [x] Show the current tenure identifier, start, parent, span and loaded block count.
- [x] Keep reward-cycle phase and stacked amount beside the tenure context.
- [x] Show the burn block, relative block time, election result and last miner win.
- [x] Show winner, commitment, parent-tenure and VRF context when present.
- [x] Count observed tenure extensions and explain which budget dimension each reset.
- [x] Show the active epoch's execution limits and clearly state whether current
      usage and remaining budget are available.
- [x] Keep identifiers shortened and explicitly labelled.
- [x] Cover the enriched panels at the default terminal width.
- [x] Run rustfmt, tests and strict clippy.

## Acceptance Criteria

- The tenure panel explains where the current tenure starts and how much of it is
  visible in the explorer.
- The sortition panel explains the Bitcoin decision and its resulting Stacks
  commitment without unexplained field names.
- Missing optional data is rendered explicitly and does not become a zero.
- Extension and budget labels distinguish protocol limits, resets and live usage.
