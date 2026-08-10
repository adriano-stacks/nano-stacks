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
is in the imported state. The intended inspection path is the Clarity side store,
not a raw MARF key:

```
NANO_DUMP_SOURCE=/tmp/v0-5-market.clar cargo xtask check-module \
  /home/aldur/mainnet-restored/state \
  SP1A27KFY4XERQCCRCARCYD1CC5N7M6688BSYADJ7.v0-5-market
```

(the node must be stopped first — it holds the state open). Until task 087 lands,
double-check this exact path before running it: the current inspection API creates
an empty store when given a nonexistent path. With the source in hand,
`supply-collateral-add` can be read for what it actually does with the trait and the
price payload, and cut down from there. Two shape guesses have now failed, which is
the signal to stop guessing.

**The earlier `no value` result is invalid evidence.** `state-value` was run against
`/home/aldur/mainnet-restored`, one directory above the configured working directory
`/home/aldur/mainnet-restored/state`. `MarfStore::open` created a small empty
`chainstate/` at the wrong location, then truthfully found no value in that new
store. Task 087 owns making all read-only diagnostics open-existing and
filesystem-non-mutating. Do not delete the accidental store while either node or
another operator may own it.

### Re-check: a rebuilt process crossed the block

A release binary built from `0f1628aa` restarted the same state at 8,708,125 and
logged one successful batch:

```
executed 500 blocks, 8708125 to 8708625,
state root 44d76d9ab3592521cc412973677bf380d2c25011f6c772f45f80a6c296088e11
```

That does not yet explain the earlier failure. `be3ec64e`, between the failing and
passing observations, makes `BindingUses` descend into allowance lists; the real
function may exercise exactly that shape. Task 086 requires an isolated pre-/post-fix
replay and bisect, exact receipt/root assertions and a same-process restart check.
Treating one successful catch-up batch as proof of the compiler fix would erase the
only reproducer without pinning it.

### Resolved: a `let`-bound principal an allowance read

The source was never in reach of a shape guess, and it did not need the state to
be stopped for: a peer serves it.

```
curl http://172.96.141.17:20443/v2/contracts/source/SP1A27KFY4XERQCCRCARCYD1CC5N7M6688BSYADJ7/v0-5-market
```

77,100 characters, `publish_height` 8,668,585. The first thing it says is that the
argument decoding above was wrong about the payload:

```clarity
(define-public (supply-collateral-add (ft <ft-trait>) (amount uint) (min-shares uint)
                                      (price-feeds (optional (list 3 (buff 8192)))))
```

Not a `(buff 2048)`. Both earlier reductions were minimising a type the function
does not take, which is why both passed — and the offset never came from the
arguments at all. It came from the body:

```clarity
  (let ((ft-address (contract-of ft))
        (asset (try! (get-asset ft-address))))
    ...
    (try! (contract-call? ft transfer amount account current-contract none))
    (if (is-eq ft-address ZEST-STX-WRAPPER-CONTRACT)
      (as-contract? ((with-stx amount)) ...)
      (as-contract? ((with-ft ft-address "*" amount)) ...))
```

`ft-address` is a `let`-bound **principal**, read three times, and the third read
is inside an allowance list. `BindingUses`, the use-count pre-pass behind
wasm-local reuse, returned without looking inside a list whose head is not a
word — and an allowance list is exactly that shape. It counted two. The second
read took the count to zero, `note_binding_read` handed the binding's locals back
to the pool, and entering the allowance borrowed them straight back; the allowance
then read its operands out of whatever had landed in those slots. Both halves of a
principal are `i32`, so wasmtime has nothing to object to and the module loads.
The failure is at run time, and `Unexpected principal data` is a version byte of
32 or more read from the wrong place.

`be3ec64e` is the cause and the fix. It was written for a different contract —
`SP28MP1HQDJWQAFSQJN2HBAXBVP7H7THD1W2NYZVK.keepgoing-safe` at 8,717,486, where the
count went to zero for a *uint* and the module would not even load — and it
repaired this one on the way, which is why a rebuilt binary crossed the block with
nothing in the log to say why.

**The bisect was run, not inferred.** With `be3ec64e`'s walk reverted in
`wasm_generator.rs` and nothing else changed:

```
clar2wasm  binding_uses_counts_a_principal_read_from_an_allowance
             left: [2, 2]   right: [3, 2]                                 FAILED
conformance allowance_principal::an_allowance_reads_the_principal_its_let_bound
             compiler:    Internal(InvariantViolation(
                            "Runtime(invalid utf-8 sequence of 1 bytes from index 0)"))
             interpreter: (ok (tuple (asset u1) (shares u46413)))         FAILED
```

Restoring the walk makes both pass. Reduced, the reused slot lands on the
allowance's asset-name string rather than on its principal, so the reduction
raises a different error than mainnet did from the same wrong read; the count, the
release and the fix are the one thing. `restart`, the compiled-module cache and
stale in-memory state are excluded by construction: both runs are fresh
in-process VMs with `ModuleCache::default()`, and the only difference between them
is thirty-nine lines of the pre-pass.

The regressions are `clar2wasm`'s
`binding_uses_counts_a_principal_read_from_an_allowance` and
`nano-conformance`'s `allowance_principal`, both in the mandatory suite. The
large-buffer and trait-dispatch reductions stay as the controls that ruled the
argument shape out.

### The root is the network's

The follower executes under `RootPolicy::Verify`, which refuses a block whose
sealed root is not the one its header commits to, so `executed 500 blocks,
8708125 to 8708625` already means all five hundred headers matched — 8,708,126
among them. Checked against the network rather than against that log:

```
GET /v3/blocks/a4bfccd4795ed0598f447ee302e8407583e8881ba7e6a9c658ec0ed6f058e206
  2,038 bytes; chain_length 8708625; consensus hash f2b9a8b6…
  state_index_root (header offset 101)
    44d76d9ab3592521cc412973677bf380d2c25011f6c772f45f80a6c296088e11
```

Byte for byte the root in the log. Every write in the block is therefore the
chain's; a cost is not in the root and decides block admission, and an event is
not in it at all, so those two are what task 086 still owes.

### What stopped the replay next was not the compiler

The follower reached 8,708,625 and then stopped naming burn views entirely. That
is [[088]]: the sortition tracker's lookahead runs to Bitcoin's tip while
execution lags, and the backwards lookup is bounded from the lookahead tip, so a
view execution has already walked through falls out of the window. Not this
file's defect, and not one a restart fixes for longer than one batch.

### Offline scratch replay gate

`tx-823f-receipt.json` freezes the canonical transaction result, all five cost
dimensions and all eight events in their published order. The ignored
`mainnet_divergence` conformance test checks those fields and the block's committed
state root against a real state that already contains both the parent and child:

```text
NANO_086_SOURCE=/path/to/immutable/state/chainstate \
NANO_086_SCRATCH=/path/to/fresh/reflink/state/chainstate \
cargo test -p nano-conformance --test conformance \
  mainnet_divergence::the_mainnet_8708126_receipt_and_root_match_the_canonical_oracle \
  -- --ignored --exact --nocapture --test-threads=1
```

The two variables name the direct directories containing `marf.sqlite` and
`clarity.sqlite`. The test canonicalizes both paths and refuses equal directory or
database inodes. It opens `NANO_086_SOURCE` through the read-only existing-state
API, records its database identity/size/modification time, and verifies those
measurements are unchanged after the replay. Only the fresh reflink scratch is
opened writable. Its parent and child headers, heights and roots must first equal
the source; the scratch is then discarded to 8,708,125 and the checked-in block is
executed twice, with a discard before each pass. Both complete observations must
be byte-for-byte identical.

This is deliberately an execution oracle, not a consensus-authentication claim.
The extension is seeded from the source's complete recorded parent header through
the explicitly unauthenticated fixture seam. The fixture append still uses
`RootPolicy::Verify`, so a different VM result cannot seal, but signer weights,
tenure linkage and the parent-tenure VRF proof are outside this gate.

## Fastpool signer-manager replay at 8,733,929

`block-8733929.hex` and `tx-6f8b-receipt.json` freeze the block and canonical
receipt that exposed task 111. The same scratch harness executes the block twice,
checks its committed root, and compares the compiler and interpreter at the
transaction's exact prestate, including result, all five cost dimensions, events
and asset writes:

```text
NANO_111_SOURCE=/path/to/immutable/chainstate \
NANO_111_SCRATCH=/path/to/fresh/reflink/chainstate \
cargo test -p nano-conformance --test conformance \
  mainnet_divergence::the_mainnet_8733929_fastpool_receipt_and_root_match_the_canonical_oracle \
  -- --ignored --exact --nocapture --test-threads=1
```

### Exact result and the last 24 runtime units

Two independent fresh reflink scratches, run in separate test processes, now
produce the canonical answer on both in-process replays: `(ok u118277070)`, all
five cost fields `(170, 232559, 893312, 17, 345)`, all eight ordered events and
the header's committed state root.

The last mismatch was 24 runtime units. The first cross-contract asset result
contains `oracle.callcode: none` under a declared `(optional (buff 1))`. The
reference sanitizes every contract-call result, narrowing that returned tuple's
runtime size from 490 to 478. The Wasm linker returned the unsanitized value, so
the next `LookupVariableSize` charged twice the extra 12 bytes. The shared fix
sanitizes the result and then enforces any dynamic trait return constraint before
writing it to Wasm. A map-backed None/Some differential pins this boundary.

An interpreter-only call on the block parent is not full receipt evidence here:
the target is transaction index four, and omitting transactions zero through
three changes one treasury mint from 516 to 514. The production transaction
prefix machinery is private and includes cumulative cost, fees, nonces,
postconditions and rollback. The gate therefore binds the compiled full-block
receipt to Hiro and the committed root, while focused interpreter crosschecks pin
the causal semantics; it does not claim an unsupported partial-block interpreter
injection.
