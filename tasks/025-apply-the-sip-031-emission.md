---
id: "025"
title: "Apply the SIP-031 emission"
status: completed
priority: high
effort: small
type: feature
dependencies: []
tags: ["mainnet", "chainstate", "consensus"]
created_at: 2026-07-30
completed_at: 2026-07-30
---

# Apply the SIP-031 emission

## Objective

W8 put the SIP-031 mint and transfer in `finish_block`, gated on
`includes_sip_031()`. It is not implemented — nothing in `crates/` mentions
SIP-031 outside a contract name in the divergence example.

stacks-core does it at `stackslib/src/chainstate/nakamoto/mod.rs:5547`, minting
to the boot recipient on every block of an epoch that includes it. That gate is
open well before 4.0, so it is live from the first mainnet block nano would
execute, and it moves balances and the liquid supply.

The Hacknet fixtures do not catch it, which is why replay is green without it.

## Tasks

- [ ] Mint and transfer the SIP-031 amount alongside the other native effects.
- [ ] Gate it on the epoch, not on a constant.
- [ ] Fold it into the liquid supply the same way the matured rewards are.
- [ ] Cover it against stacks-core's schedule in `nano-conformance`.

## Acceptance Criteria

- The recipient's balance and the liquid supply match stacks-core for a chain
  that includes the emission.
- The fixture replay still reports depth 600/600.
