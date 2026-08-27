---
id: "151"
title: "Import upstream's with-pox allowance tests"
status: completed
priority: high
effort: small
dependencies: []
tags: ["mainnet", "vm", "clarity-wasm", "upstream", "maintenance"]
created_at: 2026-08-27
parent: 121
type: chore
---

# Import upstream's with-pox allowance tests

## Objective

The 2026-08-27 re-check of task 121 found one upstream item still worth taking.
Upstream `stx-labs/clarity-wasm` #841 shipped the `with-staking` and `with-pox`
words *and* tests for them; the audit took neither because both words already
existed here, but #820 was dispositioned "equivalent implementation; take tests"
and this is the same shape.

The eight `with-pox` cases in `23b9a398` are gated
`cfg(not(test-clarity-v4|v5))`, so they run in our default configuration. They
cover allowance violations and the *violation index* that `as-contract?` and
`restrict-assets?` report, which is consensus-visible. Our only `with-pox`
coverage is one `runtime_shape_audit` snippet. `with-staking` needs nothing:
`WithStaking` delegates to `WithStacking`, whose tests we have.

Upstream test names, all in `clar2wasm/src/words/contract.rs`:
`with_pox_too_many_args`, `as_contract_safe_pox_ok`,
`as_contract_safe_pox_does_not_allow_stx`, `as_contract_safe_pox_and_stx_ok`,
`as_contract_safe_pox_then_stx_violation_index`,
`as_contract_safe_pox_and_staking_pox`, `restrict_assets_pox_no_asset_movement`,
`restrict_assets_pox_does_not_allow_stx`. `d76a98d8` re-adds `with-stacking`
cases under the `with-staking` name; take only what is not already here.

## The sequencing constraint, which is the whole reason this is a task

Importing tests changes no compiled output, so this is behaviour-neutral and buys
only regression cover. It is not free anyway: `COMPILER_IDENTITY` hashes every
file under `vendor/clarity-wasm`, tests included, and feeds
`compatibility_profile_fingerprint()`. So a test-only edit repins the consensus
profile, makes every existing state directory fail `check_profile`, and — there
being deliberately no repin subcommand — costs a fresh import and a re-issued
attestation rather than a restart.

Therefore: land this **in a batch immediately before the next import or
checkpoint ceremony, and never during a hold or replay run**. On 2026-08-27 it
was deliberately not imported because task 106's hold was mid-flight at subject
8,717,601 / witness 8,721,601 with 0/0 mismatches, and a rebuild would have cost
that run.

## Tasks

- [x] Confirm upstream head and re-diff `src/words/contract.rs` against the
      recorded vendor base before porting, in case #841's tests moved.
- [x] Port the eight `with-pox` cases, adapting them to nano's helper names and
      exact-cost expectations rather than pasting them.
- [x] Check each ported case actually bites — a violation-index assertion that
      passes against a deliberately wrong index proves nothing.
- [x] Take any `d76a98d8` `with-staking` case not already covered here.
- [x] Run the vendored suite and clippy through Nix, then the repository
      scoreboard.
- [x] Record the new `COMPILER_IDENTITY` and re-issue the attestation, or hand
      the batch to whoever is running the ceremony.

## Acceptance Criteria

- Every `with-pox` allowance outcome upstream asserts, including the reported
  violation index, is asserted here.
- No behavioural change: the frozen mainnet receipt digests and cost vectors are
  unchanged by the import.
- The compiler identity change is accounted for — either a re-issued attestation
  or an explicit note that the affected states need re-import.

## Imported and passing, 2026-08-27

All eight upstream cases are in, and all eight pass: the arity refusal, the two
`as-contract?` PoX-only cases, `with-pox` alongside `with-stx` both under and
over its allowance, the **violation index** when an amount-less `with-pox`
precedes an exceeded `with-stx`, the pox-4 delegation under
`with-staking`+`with-pox`, and the two `restrict-assets?` cases.

So `with-pox` was already right here, including the index that lands in a
receipt. The value of the import is that this is now asserted rather than assumed
— which was the argument for taking tests from an equivalent implementation.

One adaptation was needed and is worth recording for the next import. The
allowance tests in `words/contract.rs` live in a module that runs at **Clarity
4**, where the reference has `with-stacking` and does *not* have `with-pox` or
`with-staking` (`with-stacking` is Clarity 4 deprecated at 5; both of the others
are Clarity 6). Dropped in beside them the new tests failed with
`UnknownFunction("with-pox")` from both engines — correctly. They now live in
`clarity_v6_allowances`, which pins Epoch 4.0 / Clarity 6 for itself, and
upstream's `cfg(not(test-clarity-v4|v5))` gates are removed because that module
decides its own version.

`with-staking` needed nothing, as expected: `WithStaking` delegates to
`WithStacking`, whose cases were already here.

