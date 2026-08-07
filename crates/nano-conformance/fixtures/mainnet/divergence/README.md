# Mainnet blocks nano could not execute

Each block here is a mainnet block the network executed and nano did not, kept as
the smallest reproducible input there is: the consensus-serialized bytes, exactly as
a peer serves them and as the node staged them.

## `block-8708126.bin`

Captured 2026-08-07 from `/home/aldur/mainnet-restored`, a healthy state that
resumed cleanly and executed 279 blocks from 8,707,846 before stopping here.

```
block 6a75fdad6a1a60e38f8418b22286f7cf1345c3df21a945e3d937798d3f5c0471
height 8,708,126, 6 transactions

e536ab3f… SPVYF6M3FD0738FWDGDMJ84EFMGM38JS0N7DE42K  payload 7  (tenure change)
c2bda8c2… SP3D0ZXGD27KW2D3ZA7KVH76VX1BJXQ14PRCX5KEV payload 2  (contract call)
fa06486a… SP21EK0KSQG7HEHBGCVRJGPGFMV8SCA2B85X01DK2 payload 2  (contract call)
ea94eb94… SP3SBQ9PZEMBNBAWTR7FRPE3XK0EFW9JWVX4G80S2 payload 0  (token transfer)
823f248a… SP2ZM1KNQNS96RM1MAJTZV7WE7F38GFGRND4E80DW payload 2  (contract call)  <-- THIS ONE
a0cb198c… SP21EK0KSQG7HEHBGCVRJGPGFMV8SCA2B85X01DK2 payload 2  (contract call)
```

nano fails the **block**, not a transaction:

```
Clarity execution error:
Internal(InvariantViolation("Expect(\"Internal(Expect(\\\"Unexpected principal data\\\"))\")"))
```

`StandardPrincipalData::new` raises that for one reason — a version byte of 32 or
more — so something handed it bytes that are not a version. The two production sites
that build a principal from raw bytes are both reads at a running offset
(`clar2wasm/src/wasm_utils.rs:464` out of wasm linear memory, `:1528` out of a
serialized buffer), which makes a read at the wrong offset the shape to look for.

An internal error fails the whole block rather than one transaction, which is why the
message did not name a transaction. It does now: `run_transactions` names the
transaction whose failure took the block down, and a re-run against this state said

```
transaction 823f248a092638cbe4e08f30e5d60d872ff35d73a6a9ee98c790720f8ebd0db3
of 8708126 failed the block rather than its own receipt
```

So it is the fifth transaction, a contract call from
`SP2ZM1KNQNS96RM1MAJTZV7WE7F38GFGRND4E80DW` paying a 5,482 fee -- notably the largest
fee in the block, which is what a call doing real work looks like.

### What it calls

```
SP1A27KFY4XERQCCRCARCYD1CC5N7M6688BSYADJ7.v0-5-market::supply-collateral-add
  arg 0  contract principal  SP…​.sbtc-token   <-- a trait reference
  arg 1  uint 46413
  arg 2  uint 45924
  arg 3  buff, ~2 KB          (a price-feed update payload)
```

Argument 0 decodes as type byte `6` (contract principal), version `20`, twenty hash
bytes, then a name length of `10` and `sbtc-token`. So the call passes a **contract
principal where a trait is expected** — which is precisely the shape the two offset
reads above exist to handle, and the same family as the trait-reference work already
pinned by `as_contract_codegen::a_contract_principal_where_a_trait_is_expected_compiles`
and `as_contract_sender::both_engines_agree_on_which_contract_a_trait_names`.

Those tests pass, so whatever is wrong here is not covered by them.

### Ruled out: the buff is not moving the principal

The obvious suspect was the two-kilobyte buff — a buff is what moves everything after
it in memory, so an offset wrong by its length would produce exactly a bogus version
byte. `a_trait_beside_a_large_buff_still_names_its_contract` minimizes that shape: a
read-only function taking `(<named> uint uint (buff 2048))`, called with buffs of 0,
1, 64, 1024, 2000 and 2048 bytes, crosschecked against the interpreter.

**It passes at every size.** So the argument *shape* is not the bug, and the test
stays as a regression pinning that.

### Also ruled out: dispatching through the trait

The next suspicion was the dispatch rather than the argument — a principal has to be
reconstructed to make a `contract-call?` through a trait, which is a second place one
comes out of memory. `a_trait_called_beside_a_large_buff_dispatches_to_the_same_contract`
minimizes that: a `define-public` taking the same four arguments and *calling* the
trait rather than naming it, at the same six buff sizes.

**It passes too.** So neither the argument shape nor the dispatch reproduces it, and
guessing at shapes has stopped paying.

### What to do instead

Reduce from the real source rather than towards it. `v0-5-market` is deployed, so it
is in the imported state, and the restored state has it:

```
cargo xtask state-value /home/aldur/mainnet-restored tip \
  "clarity-contract::SP1A27KFY4XERQCCRCARCYD1CC5N7M6688BSYADJ7.v0-5-market"
```

(the node must be stopped first — it holds the state open). With the source in hand,
`supply-collateral-add` can be read for what it actually does with the trait and the
price payload, and cut down from there. Two shape guesses have now failed, which is
the signal to stop guessing.

**Tried, and it did not resolve:** `state-value` against the sealed tip
(`cf8bd32e…`) with that key answers `no value`. The key form is right — it is what
`mainnet_checkpoint.rs` reads contracts through — so either the deepest seal is not
the block to ask (it is a *seal*, and after `tasks/mainnet/079` the deepest one is not
necessarily the deepest *committed* block), or a contract's source is metadata in the
side store rather than a trie leaf and `state-value` does not reach it. Worth ten
minutes with `MarfStore`'s metadata path before assuming the former.
