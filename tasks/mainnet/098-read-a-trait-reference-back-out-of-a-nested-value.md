---
id: "098"
group: mainnet
title: "Read a trait reference back out of a nested value"
status: completed
priority: critical
effort: medium
dependencies: ["097"]
tags: ["mainnet", "vm", "clarity-wasm", "consensus", "liveness", "release"]
created_at: 2026-08-09
type: bug
completed_at: 2026-08-10
---

# Read a trait reference back out of a nested value

## Objective

At the task's opening boundary, a trait reference inside a composite was written
into Wasm linear memory and read back as garbage. The read was not a wrong answer
but an `InvariantViolation`, so the block was refused outright rather than the
transaction getting a receipt. The retained failure logs stop at mainnet block
8724864. Two independent offline replays now execute the block and reproduce
the canonical receipt and root.

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

## Resolution

`TupleCons` had replaced each field's source type with its contextual result
type before evaluating it. A callable trait stored in a field widened to
`principal`, so the tuple lost the runtime shape needed when a later contract
unwrapped the optional execution plan. Tuple construction now evaluates fields
in their source layouts, captures the exact composite runtime type, and only
then projects to the analyzed result type. Legacy `TraitReferenceType` values
use the same two-word principal representation as callable traits.

The exact replay also exposed independent cost gaps. Cross-contract argument
sizes now carry the caller's pre-cast sizes, and `map-get?` charges the actual
serialized bytes returned by the database, including a tombstone. Focused
compiler/interpreter reductions pin both seams.

## Acceptance

- [x] Mainnet block 8724865 executes to the state root the network published,
  and transaction `24d63204…c061558` gets the receipt the network recorded.
- [x] A crosscheck reads a trait reference back out of a tuple inside a list through
  both engines and gets the same value and the same five cost dimensions.
- [x] Reproduced offline first, in `fixtures/mainnet/divergence/`, rather than
  diagnosed against the live node.

## Completion evidence

The focused and broad compiler gates are green:

```
cargo test -p clar2wasm --lib \
  cost::crosscheck::reads_a_trait_from_a_cross_contract_tuple \
  -- --nocapture --test-threads=1
cargo test -p clar2wasm --lib \
  cost::crosscheck::reads_a_trait_from_a_tuple_inside_a_list \
  -- --nocapture --test-threads=1
cargo test -p clar2wasm --lib cost::crosscheck \
  -- --nocapture --test-threads=1
# 37 passed
cargo test -p clar2wasm --lib -- --test-threads=1
# 1469 passed; 5 ignored; 0 failed
cargo clippy -p clar2wasm --all-targets -- -D warnings
```

The ignored `the_mainnet_8724865_nested_trait_receipt_and_root_match_the_canonical_oracle`
gate ran twice against separate fresh reflinks of the stopped immutable source.
Both runs reproduced `(ok true)`, all five canonical costs
`(891, 4545157, 18263940, 24, 4909)`, all 21 ordered events, the asset map, and
the network state root. The second scratch is
`/home/aldur/nano-098-second.ZYcFqM/chainstate`; its MARF inode differs from the
source while size and mtime match the captured source stamp.
