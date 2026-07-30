---
id: "036"
title: "Move the sBTC derivations out of nano-address"
status: pending
priority: low
effort: small
type: improvement
dependencies: []
tags: ["notes"]
created_at: 2026-07-30
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

- [ ] Decide where the derivations belong — the crate that reads the sBTC
      registry, or one of its own.
- [ ] Move them and their reference vectors with them.
- [ ] Leave `nano-address` holding only the encodings and the address types.

## Acceptance Criteria

- `nano-address` names no PoX-5 or sBTC constant.
- The reference-vector tests move with the code and still pass.
