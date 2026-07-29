---
id: "015"
title: "Update Hacknet master PoX-5 integration"
status: completed
priority: high
effort: medium
dependencies: []
tags: ["hacknet", "pox5", "interop"]
created_at: 2026-07-29
completed_at: 2026-07-29
---

# Update Hacknet master PoX-5 integration

## Objective

Move the Hacknet test configuration from the superseded PoX-waterfall branch
and prerelease API image to the released PoX-5 implementation on Stacks Core
main and the stable Stacks API.

## Tasks

- [x] Update Hacknet's Stacks Core and API pins.
- [x] Confirm the rendered Compose configuration targets the released stack.
- [x] Update nano's Hacknet instructions to use the released configuration.

## Acceptance Criteria

- A fresh Hacknet genesis uses Stacks Core main and API 9.0.1.
- The existing configurable non-mainnet sBTC contracts remain wired into
  the rendered node configuration.
- Nano's live-interoperability instructions do not depend on the retired
  PoX-waterfall branch.
