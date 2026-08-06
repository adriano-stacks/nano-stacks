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

- [x] Mint and transfer the SIP-031 amount alongside the other native effects.
- [x] Gate it on the epoch, not on a constant.
- [x] Fold it into the liquid supply the same way the matured rewards are.
- [x] Cover it against stacks-core's schedule in `nano-conformance`.

## Acceptance Criteria

- The recipient's balance and the liquid supply match stacks-core for a chain
  that includes the emission.
- The fixture replay still reports depth 600/600.

## What was already there, and what was not

Three of the four were code, and the code was written: `finish_block` calls
`mint_sip_031_on_new_tenure` after the matured rewards and the unlocks, in
stacks-core's order — raise the supply, credit `.sip-031`, report an
`STXMintEvent` on the coinbase receipt. What was missing was the part that says
so. Two of the four assertions this task asks for did not exist, and one of the
two that did was comparing against a copy of stacks-core's lookup rather than
stacks-core's lookup.

`sip_031_emission_matches_stacks_core` now installs the release schedule into
stacks-core's own `testing` override — which is the only table a `testing` build
reads — and asks `SIP031EmissionInterval::get_sip_031_emission_at_height`. That
covers the scan rule as well as the amounts: a table read with `>` instead of
`>=` mints a tenure late at every boundary, and comparing tables cannot see it.
Every probe height now comes from stacks-core's table rather than from a list
restated in the test.

`a_tenure_start_block_mints_the_sip_031_emission` executes the same captured
tenure-start block twice, once at a Bitcoin height inside the schedule and once
at the height it was really mined at, and compares the
`_stx-data::ustx_liquid_supply` leaf. The supply is not readable as a number —
the MARF holds the hash of the value — so the leaf is what can be compared, and
the emission is the only term of that write the Bitcoin height moves. Removing
`increment_liquid_stx_supply` from `mint_sip_031` was tried: the balance
assertion, the receipt assertion and the whole fixture replay stay green, and
this is the assertion that fails. A mint that credits the recipient without
raising the supply seals a root the network does not have, and nothing else here
notices.

## The epoch gate is unreachable, not missing

stacks-core guards the mint on `evaluated_epoch.includes_sip_031()`, which is
`>= Epoch32`. nano has no counterpart and cannot need one, for two reasons that
are now both pinned rather than argued:

- mainnet's first emission interval *is* the epoch 3.2 boundary — 907,740 either
  way — so the schedule returning zero and the gate being shut are the same
  condition. The test asserts that against
  `BITCOIN_MAINNET_STACKS_32_BURN_HEIGHT`, so a table that grew an earlier
  interval would fail rather than mint.
- nano executes epoch 4.0 blocks only. Every block it will ever finalize is past
  the gate, on any network.

So the item is closed by an oracle rather than by a branch. Writing the branch
would have added a condition that cannot be false, and no test could have shown
it doing anything.

## What this does not prove

The emission has never been checked against a chain that mints it *and* whose
state root nano verified. The hacknet capture's burn heights are three orders of
magnitude below the testnet schedule, so the fixture replay is green with or
without the mint, and the height these tests execute at is not the one the block
committed to — the root is not checked. The real evidence is the mainnet node:
every tenure it has executed is past 907,740, so 475 STX has been minted to
`.sip-031` on each one, under signed headers that verified the roots. That is a
live run, not a gate in this suite.
