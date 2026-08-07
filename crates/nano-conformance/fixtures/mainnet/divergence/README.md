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

Next: what it calls, and which of its arguments or return values is a principal. The
two offset reads above are where a principal comes out of memory, so the value that
lands on the wrong side of one is the thing to find.
