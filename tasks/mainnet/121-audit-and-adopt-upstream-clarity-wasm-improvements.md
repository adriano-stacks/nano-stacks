---
id: "121"
group: mainnet
title: "Audit and adopt upstream clarity-wasm improvements"
status: completed
priority: high
effort: large
type: chore
dependencies: []
tags: ["vm", "clarity-wasm", "upstream", "maintenance"]
created_at: 2026-08-12
completed_at: 2026-08-12
---

# Audit and adopt upstream clarity-wasm improvements

## Objective

Compare the vendored clarity-wasm with upstream `stx-labs/clarity-wasm` main at
`8354ef005414e6edc181b7ed0a7372169de7bebe`. Adopt upstream fixes and simpler
code generation that apply to nano-stacks while preserving the intentional
Epoch 4.0, Clarity 6, cost, and production-dependency work carried locally.

## Tasks

- [x] Inventory every source and test difference from the pinned upstream tree.
- [x] Classify differences as upstream improvements, intentional nano changes,
      already-equivalent changes, or unrelated upstream drift.
- [x] Port every applicable upstream improvement, including the compact
      `to-ascii?` UTF-8 validator.
- [x] Record the disposition of every remaining difference.
- [x] Run formatting, the vendored test suite, and clippy through Nix.

## Acceptance Criteria

- No applicable correctness or maintainability improvement in the pinned
  upstream diff is left unaddressed.
- Nano-specific Epoch 4.0 and production dependency constraints remain intact.
- The vendored clarity-wasm tests and clippy pass without warnings.

## Audit Basis

- Upstream: `stx-labs/clarity-wasm` main at
  `8354ef005414e6edc181b7ed0a7372169de7bebe`.
- Nano baseline: the vendored tree at `f36060a8`, whose upstream rebase is
  `9f4ec58f` and whose pre-rebase upstream base was `dc6c98aa`.
- Method: compare clean source mirrors with build output and `.git` excluded,
  inspect every upstream first-parent merge after `dc6c98aa`, then inspect all
  residual hunks. The source mirror has 82 differing paths and 28,941 diff
  lines. Every path is assigned below.

## Upstream Merge Disposition

| upstream merge | disposition |
|---|---|
| #814 epoch/version handling | **Keep nano.** Upstream's strict assertion rejects the old-contract and deliberately mixed test configurations nano supports. Nano's warn/coerce boundary and explicit epoch/version matrix are covered by tasks 037 and 066. |
| #820 `secp256r1-verify` | **Equivalent production implementation; take tests.** Upstream emits the curve implementation into Wasm. Nano delegates to the pinned Clarity host implementation, preserving its version-dependent single/double hashing and avoiding duplicated cryptography. Added compact crosschecks for arity, all three lengths/outcomes, keys/messages, and Clarity 4/5/6 hashing behavior. |
| #822 stacks-core integration fixes | **Already incorporated or superseded.** The `OUT_DIR` standard module and relevant compatibility fixes are in nano; dependency/build changes are superseded by the pinned release-feature setup. |
| #812 Clarity 4 costs | **Already incorporated, keep nano tables.** Nano's cost-4/5 tables match the pinned interpreter exactly. Upstream's tier-4 hard-coded expectations target a different stacks-core revision. The hunk review did expose the omitted `to-ascii?` charge, fixed here with interpreter-order and runtime-value sizing. |
| #827 UTF-8 `to-ascii?` | **Adopt and strengthen.** Take the one-shift normalization, compact `!0x3600 >> byte` control-character bitset, direct byte allocation, and missing cost charge. Charge after argument evaluation, as the interpreter does, and size the runtime value rather than its declared maximum. Exhaustive validation now covers all 256 byte values. |
| #828 reserved `contract-hash?` names | **Already incorporated.** Nano has the same generated-name exclusion with a clearer local explanation. |
| #824 Clarity 1 trait-principal return | **Already equivalent.** Nano's `TraitReferenceType` handling uses the called contract's captured version and has the mainnet regression. Reading the version back through upstream's store does not change the result. |
| #825 constant contract calls | **Already incorporated.** Nano retains `constant_contract_principals` and the upstream regression coverage. |
| #813 stacks-core sync | **Do not take dependency drift.** Nano intentionally pins stacks-core `efc34a07` and disables testing/developer features in production. Applicable compiler fixes from the sync are already present. |
| #823 Clarity 1 contract-call annotation | **No code needed.** Both upstream regressions pass against nano's existing compiler. Nano's pinned Clarity lacks `concretize_deep`; runtime/type-shape fixes make the extra annotation pass redundant. |

## Residual Path Ledger

The four groups below partition all 82 paths in the clean mirror diff.

### Packaging and tools (4)

`Cargo.lock`, `clar2wasm/Cargo.toml`, `clar2wasm/benches/comparison.rs`, and
`clar2wasm/src/bin/utils/mod.rs` are intentional. They pin stacks-core, keep
testing-only Clarity features out of production, and adapt tooling to nano's
compiler/runtime APIs.

### Compiler and runtime core (23)

- Nano-only files `src/bitcoin.rs`, `src/cost/clar5.rs`, `src/error.rs`,
  `src/layout.rs`, `src/phases.rs`, `src/runtime_shape.rs`, and
  `src/standard/standard.wasm` implement Epoch 4/Clarity 6, the production
  error boundary, explicit ABI phases/layouts, runtime-shape preservation, and
  the reproducibly built standard module. Keep all seven.
- `src/copy.rs`, `src/cost/clar4.rs`, `src/cost.rs`, `src/datastore.rs`,
  `src/deserialize.rs`, `src/duck_type.rs`, `src/error_mapping.rs`,
  `src/initialize.rs`, `src/lib.rs`, `src/linker.rs`, `src/serialize.rs`,
  `src/standard/standard.wat`, `src/test_utils.rs`, `src/tools.rs`,
  `src/wasm_generator.rs`, and `src/wasm_utils.rs` carry the pinned exact-cost
  tables, packed ABI/runtime shapes, host admission and crypto boundary,
  `InstancePre`/module caching, and nano's crosscheck harness. The upstream
  side of every hunk was checked; applicable #822/#824/#825 changes are already
  represented. Keep nano's versions.

### Words (31)

- `src/words/to_ascii.rs`: adopt #827's simpler validator/allocation and restore
  the omitted consensus charge, with the interpreter-order/runtime-size
  refinements described above.
- `src/words/secp256r1.rs`: keep the vetted host call; adopt upstream's missing
  behavior coverage in a smaller version-matrix test module.
- Nano-only `src/words/bitcoin.rs` supplies the Epoch 4 Bitcoin words.
- `src/words/arithmetic.rs`, `bindings.rs`, `blockinfo.rs`, `comparison.rs`,
  `conditionals.rs`, `consensus_buff.rs`, `constants.rs`, `contract.rs`,
  `control_flow.rs`, `conversion.rs`, `data_vars.rs`, `default_to.rs`,
  `enums.rs`, `equal.rs`, `functions.rs`, `hashing.rs`, `index_of.rs`,
  `maps.rs`, `mod.rs`, `noop.rs`, `principal.rs`, `print.rs`, `secp256k1.rs`,
  `sequences.rs`, `stx.rs`, `tokens.rs`, `traits.rs`, and `tuples.rs` retain
  nano's exact dynamic charging, evaluation/short-return order, borrowed ABI,
  runtime-shape propagation, Clarity 6 surface, and captured-mainnet fixes.
  Removed upstream charge sites were checked individually: nano either emits
  the same charge later with the runtime value size, or charges once at the
  enclosing allowance form. `to-ascii?` was the only true omission.

### Tests and fixtures (24)

- `tests/bin_tests.rs`, `tests/contracts/equal.clar`, `tests/epoch40.rs`, and
  `tests/lib_tests.rs` reflect nano's API, exact costs, and Epoch 4 coverage.
- All eleven files under `tests/proptest-regressions/` (`blockinfo`,
  `conditionals`, `consensus_buff`, `contracts`, `equal`, `print`, `response`,
  `sequences`, `serialization_size`, `to_ascii`, and `traits`) are minimized
  local failures retained as regression seeds.
- `tests/standard/unit_tests.rs` and the eight differing
  `tests/wasm-generation/` modules (`conditionals`, `contracts`, `equal`,
  `principal`, `sequences`, `serialization_size`, `to_ascii`, and `tuple`)
  cover nano's dependency API, runtime shapes, exact costs, and additional
  mainnet cases. Keep them; the useful missing upstream secp coverage was moved
  next to the word implementation.

## Verification

- Clarity 1 contract-call regressions: 2 passed.
- Vendored library: 1,481 passed, 2 pre-existing ignored, 0 failed. This
  includes nine `to-ascii?` tests (exact costs, short-return ordering, empty
  UTF-8, and all 256 byte values) and five new `secp256r1` tests.
- All eight vendored integration targets: 1,203 passed, 0 failed, including
  standard-library and Wasm-generation property tests plus OOM checks.
- Vendored doc tests: passed; no doctests defined.
- Release/all-target vendored clippy: passed with `-D warnings`.
- Repository scoreboard: 340/340 state roots, 340/340 receipts, 340/340 cost
  vectors, and 500/500 frozen mainnet receipt digests.
- Repository formatting, `git diff --check`, strict task validation, and task
  ID deduplication: passed.

## Re-audit, 2026-08-26

The audit this task closed was a point-in-time one; keeping the vendored engine
current is standing work, so this records the second pass rather than opening a
task for it.

Vendor base is `8354ef00` (2026-08-06). Upstream head is `f4fb08c2`
(2026-08-25). Four merges since the base, and **nothing to import**:

| Upstream | What | Why not |
|---|---|---|
| #841 `with-staking` / `with-pox` | the two new allowance words | already implemented here, and ahead: same `with_pox` host call and allowance push, plus a `with-stx` note on the reference reading its amount through `as_ref` that upstream has no equivalent of |
| #829 `to_ascii_string_utf8` | the proptest expected the wrong answer for a non-printable byte | already corrected here, to the same rule — `Value::string_ascii_from_bytes`' — arrived at independently |
| #830, `155f1031` | clarity dependency bumps and a clarity-1 test sync | the pinned revision `efc34a07` is deliberate; a bump is a rebase decision, not an import |
| `443f2320` | a known-bug carve-out for a string ending in `\` under parser v1 | parser v1 is epoch < 2.1, which a 4.0-only node never runs |

Worth noting that two of the four were things this project had already found and
fixed on its own. That is the expected shape from here: the fork is ahead on
epoch-4.0 behaviour and behind only on dependency bumps it declines on purpose.

The recipe, so the next pass costs minutes: the GitHub API over
`stx-labs/clarity-wasm` (`stacks-network/clarity-wasm` is a mirror with identical
SHAs but needs `curl -L`), diffed against the recorded vendor base rather than
tip. Batch any import *before* a checkpoint ceremony — every clarity-wasm edit
moves `COMPILER_IDENTITY` and invalidates both the attestation and any running
import.

## Re-check of the re-audit, 2026-08-27

Upstream head is still `f4fb08c2`, so the pass above is current. Both of its
"already here" findings verified independently: our tree carries the `WithStaking`
and `WithPox` words, the `with_pox` linker function and its `standard.wat` import,
and our `to-ascii?` proptest answers `Value::string_ascii_from_bytes`' rule with
an exhaustive 128-byte test beside the word that cannot drift from Clarity's
answer. Upstream's `Epoch40`/`Epoch41` cost arms are still `todo!()`, and our
`link_stx_account_fn` already passes `v4_unlock_ht`, so the "ahead on epoch 4.0,
behind only on declined bumps" reading holds.

Two amendments to the table above.

**`9a0b2a3b` is not a dependency bump.** It is `cfg` gating plus a new
`crosscheck_expect_failure_with_clarity_version` helper we do not have, so the
reason recorded for it is wrong even though the disposition is right. The real
reason is stronger: nothing outside `vendor/` ever sets `test-clarity-v1`, `-v2`
or `-v3`, so those matrices never run in this repo and their gates buy nothing
here. We did already reach `crosscheck_multi_contract_with_env` independently.

**One candidate is open, not closed.** #841 also shipped tests — `23b9a398`
(+224) and `d76a98d8` (+121) in `src/words/contract.rs` — and the eight
`with-pox` cases among them are gated `cfg(not(test-clarity-v4|v5))`, so they
would run in our default configuration. They cover allowance violations and the
*violation index* `as-contract?`/`restrict-assets?` report, which is
consensus-visible. Our only `with-pox` coverage is a `runtime_shape_audit`
snippet; `with-staking` is covered by delegating to `WithStacking`, whose tests
we have. #820 was dispositioned "equivalent implementation; take tests" and this
is the same shape, so #841 closing as code-only was one step short.

**Why it was not imported on the spot.** Tests change no compiled output, so the
import is behaviour-neutral and buys only regression cover — but
`COMPILER_IDENTITY` hashes every file under `vendor/clarity-wasm` including
tests, and it feeds `compatibility_profile_fingerprint()`, so a test-only edit
still repins the profile, still makes existing state fail `check_profile`, and
still needs a fresh import and a re-issued attestation rather than a restart. At
the time of writing, task 106's hold was mid-flight (subject 8,717,601, witness
8,721,601, network 8,851,428, 0/0 mismatches, 254 GiB free), and rebuilding for
these eight tests would have cost that run. Land them in the batch immediately
before the next import or ceremony, never during one.

