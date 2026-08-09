---
id: "098"
group: mainnet
title: "Read a trait reference back out of a nested value"
status: in-progress
priority: critical
effort: medium
dependencies: ["097"]
tags: ["mainnet", "vm", "clarity-wasm", "consensus", "liveness", "release"]
created_at: 2026-08-09
type: bug
---

# Read a trait reference back out of a nested value

## Objective

At the task's opening boundary, a trait reference inside a composite was written
into Wasm linear memory and read back as garbage. The read was not a wrong answer
but an `InvariantViolation`, so the block was refused outright rather than the
transaction getting a receipt. The retained failure logs stop at mainnet block
8724864; a later rebuilt process has now crossed the block, but the isolated
receipt/root replay remains the completion gate.

## Evidence

Reached only after [[096-cross-a-stacks-fork-inside-one-sortition-chain]] let the
node off the branch it was stranded on and
[[097-cast-a-trait-argument-the-callee-declares-differently]] let this argument
past `admits`:

```
invalid transaction: transaction 24d63204544d02e7e7a8e43a4cde396f92b174fcb576bfed035536641c061558
  of 8724865 failed the block rather than its own receipt:
  Clarity execution error: Internal(InvariantViolation("Expect(
    \"principal representation for CallableType(Trait(SP1CGXWEAMG6P6FT04W66NVGJ7PQWMDAC19R7PJ0Y.pyth-traits-v2.decoder-trait))
      at value offset 18287 points to offset 4345 with invalid length 4345\")"))
```

The offset now identifies the boundary exactly. Governance's argument area begins
at 18271: the preceding principal occupies 8 bytes, then the optional tag and
tuple runtime-shape handle occupy 4 bytes each. `18271 + 8 + 4 + 4 = 18287`, the
lexicographically first `pyth-decoder-contract` field of the forwarded execution
plan. The unaligned base is ordinary after a 2,007-byte buffer, and equal garbage
offset/length is diagnostic evidence rather than proof of an off-by-one read.

`validate_principal_length` (`clar2wasm/src/wasm_utils.rs:445`) is what catches
it, and it is the only reason this is a refusal rather than a wrong principal.

## Where to look

`write_to_wasm`/`read_from_wasm` agree on the leading four-byte runtime-shape
handle for both tuples and lists, and `get_type_size` agrees with both. So the
disagreement is likelier to be in a path where the handle is **zero** — the
short-circuit in `read_from_wasm_indirect` does not apply and the fields are read
by stride — or in the `in_mem_offset` a nested composite hands to its children.

## Acceptance

- [~] Mainnet block 8724865 executes to the state root the network published,
  and transaction `24d63204…c061558` gets the receipt the network recorded.
- [x] A crosscheck reads a trait reference back out of a tuple inside a list through
  both engines and gets the same value and the same five cost dimensions.
- [~] Reproduced offline first, in `fixtures/mainnet/divergence/`, rather than
  diagnosed against the live node.

## Current boundary

The first independent defect at this call boundary was cost metadata, not the
invalid principal itself. A static cross-contract call used the callee's widened
trait type when charging the caller's original principal values: the exact
three-trait `write-feed` shape overcharged runtime by `3 * 256`. The general fix
carries each caller-evaluated pre-cast `Value::size()` through the existing
`contract_call` memory ABI. These exact gates are green:

```
cargo test -p clar2wasm --lib \
  cost::crosscheck::reads_a_trait_from_a_cross_contract_tuple \
  -- --nocapture --test-threads=1
cargo test -p clar2wasm --lib \
  cost::crosscheck::reads_a_trait_from_a_tuple_inside_a_list \
  -- --nocapture --test-threads=1
cargo test -p clar2wasm --lib cost::crosscheck \
  -- --nocapture --test-threads=1
# 25 passed
cargo test -p clar2wasm --lib words::contract::tests \
  -- --nocapture --test-threads=1
# 87 passed
```

The faithful caller → oracle → governance reduction, including the bound
2,007-byte buffer and `(some execution-plan)`, is also green. It therefore does
not pin the historical invalid representation, and no layout change is justified
from it.

A rebuilt live binary resumed exactly at 8,724,864 and logged a successful
8,724,864 → 8,725,364 `RootPolicy::Verify` batch. Its read-only `/v2/info` later
reported 8,728,953. Hiro's canonical record says tx `24d63204…c061558` at index 2
of block 8,724,865 succeeded with `(ok true)`, costs
`(891, 4545157, 18263940, 24, 4909)`, and 21 ordered events. The node's own archive
served the exact 6,316-byte block. Both are now checked in as
`block-8724865.hex` and `tx-24d632-receipt.json`; the always-on fixture integrity
test is green, as are the release-inventory policy gate and strict conformance
Clippy.

The ignored `the_mainnet_8724865_nested_trait_receipt_and_root_match_the_canonical_oracle`
gate now runs against an immutable checkpointed copy and a separate writable
reflink, never the live node. Its exact-prestate comparison executes block
transactions 0 and 1 through the production path, then asks both engines about
transaction 2 with the same cumulative tracker. The interpreter exactly matches
Hiro's cost `(891, 4545157, 18263940, 24, 4909)`; the compiler returns the same
value but charges `(891, 4545156, 18259366, 24, 4909)`. Recursive temporary
repair of compiler-stubbed contract bodies is reverted after the diagnostic;
all 342 original stub definitions compare byte-for-byte afterward. The task is
therefore still open on a measured compiler undercharge of one read byte and
4,574 runtime units, not on the historical trait-layout refusal.
