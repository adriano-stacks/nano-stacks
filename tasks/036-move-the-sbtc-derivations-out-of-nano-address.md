---
id: "036"
title: "Move the sBTC derivations out of nano-address"
status: completed
priority: low
effort: small
type: improvement
dependencies: []
tags: ["notes"]
created_at: 2026-07-30
completed_at: 2026-07-30
---

# Move the sBTC derivations out of nano-address

## Objective

From the code review in `notes.md`: the PoX-5 and sBTC derivations do not belong
in `nano-address`.

`crates/nano-address/src/lib.rs` holds `POX_5_SBTC_DEPOSIT_MAX_FEE_SATS`,
`sbtc_deposit_taproot_output_key` and `sbtc_pox5_deposit_taproot_output_key`.
That crate is meant to be c32, base58 and the address types. A deposit fee cap
and a taproot output key derived from an aggregate public key are PoX-5 protocol
rules that happen to produce an address.

Left where they are, `nano-address` cannot be understood without knowing PoX-5.

## Tasks

- [x] Decide where the derivations belong — the crate that reads the sBTC
      registry, or one of its own.
- [x] Move them and their reference vectors with them.
- [x] Leave `nano-address` holding only the encodings and the address types.

## Acceptance Criteria

- `nano-address` names no PoX-5 or sBTC constant.
- The reference-vector tests move with the code and still pass.

## Where they went, and why

`nano-bitcoin::sbtc`. The output key is a taproot script rule — a deposit leaf
and a reclaim leaf hashed into a tweak — so it belongs with the crate that
already builds and parses Bitcoin script, not with c32 and base58.
`nano-bitcoin` already depended on `bitcoin` and on `nano-address`, so the move
added no dependency edge and let `nano-address` drop `sha2` entirely.

Nothing in production calls the derivation yet: the miner takes `sbtc_address`
from the reward set a peer reports rather than deriving it from the sBTC
registry's aggregate key, which is the trust gap W8 describes. That is a
follow-up for whoever derives the reward set locally.
