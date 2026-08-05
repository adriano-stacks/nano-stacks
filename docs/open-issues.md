# Open issues, 2026-08-05

Written down because the session that found them is out of context. Each is stated
with the evidence, so the next person does not have to re-derive it. Where a claim
was measured, the number is here; where it was inferred, it says so.

Replay depth stands at **8,668,095**, parked on issue 1. Six consensus divergences
were found and five fixed today; the sixth is diagnosed.

## 1. clar2wasm rejects a legal name — blocks the replay

```
SP1E0XBN9T4B10E9QMR7XMFJPMA19D77WY3KP2QKC.auto-alex-v3-endpoint-v2-02::rebase
  compiler     analysis failed: Internal error: Name already used ClarityName("err")
  interpreter  (ok u390)
```

Mainnet executed this contract. `words/maps.rs`'s `DefineMap::traverse` calls
`generator.is_reserved_name`, which is
`lookup_reserved_functions(name, &version).is_some() || variables::is_reserved_name(…)`
— and `err` is a native function in every Clarity version, so `(define-map err …)`
is refused. The reference analysis has already run and passed, so clar2wasm is
stricter than the reference and second-guessing a judgement that was already made.

Unverified hypothesis: the reference's `check_define_map` guards only against names
already used *in this contract*, not against native function names. Read the
reference rather than inferring. Check whether `define-data-var`,
`define-fungible-token` and `define-trait` copied the same check.

Do not simply delete the guard: find the test that wanted it first, and note that
being wrong in the permissive direction means accepting a contract the network
rejects, which is worse.

## 2. A killed checkpoint import is unrecoverable

Journalling is off during import (deliberately — a mainnet import fell from 60 MB/s
to under 3 with a WAL that could never checkpoint), and
`Vm::open_from_checkpoint` treats **any** `marf_block` row as "already imported".
So a killed import resumes on a partial trie and nothing downstream notices. This
is a worse failure mode than anything fixed today. Belongs with task 051.

## 3. The supertype asymmetry, reachable through two words

`least_supertype` walks the *first* argument's tuple fields and looks each up in the
second, silently dropping the rest — so `(if c {a,b} {a,b,c})` types as `{a,b}`
while the reverse is rejected by analysis outright.

Where it still bites: when such an expression is a function's **return value**, the
interpreter yields the taken branch's wide tuple and wasm must yield the narrowed
layout. Genuinely different values; no conversion reconciles them. `default-to` is a
far more common route to it than `if`.

Pinned as `a_narrowed_default_handed_back_whole_agrees` and
`two_tuple_shapes_under_one_if_compile_to_a_loadable_module`'s sibling case,
`#[ignore]`d with reasons. Needs a decision at the analysis layer, not a compiler
patch. `blacklist-susdh-v1` reads its `default-to`s through `get`, so 8,667,509 does
not depend on it — another mainnet contract could.

## 4. The event dispatcher blocks execution

`EventDispatcher::post` retries five times with 0/100/200/300/400 ms backoff,
awaited inline per block. Against an observer that does not answer that is **about a
second of sleeping per block** — measured, and it was most of the 28–34 blocks/min
this replay showed all day. The immediate cause was a config error (see issue 8),
but the design is the hazard: dispatch does not need to block `execute_staged` at
all.

## 5. The winning leader key derives 12 of 14

Local sortition derivation is exact for consensus hash, sortition hash and burn
total (14/14 captured, and matching `api.hiro.so/v3/sortitions` at burn 960,259).
The winner's *identity* derives 12 of 14; the two misses name a different commitment
carrying the same seed. So the sortition hash is right and the leader key is not.

`check_tenure_vrf` rejects on a wrong key, so publishing a 12-in-14 answer would
reject one valid tenure in seven — the key is therefore only published when the burn
block leaves no choice. `WINNERS_FLOOR` is 12 and fails at 13, so the gap is
measured rather than hidden. The remaining difference is `make_burn_sample`'s
min-median weighting of empty window slots; a variant fixing those two breaks
960,228. Tried and reverted; recorded in task 049.

## 6. `context.vrf_seed` still comes from the peer

The locally derived seed matched at all 14 captured blocks and every live block, but
it is Clarity-visible, so switching it moves a state root and wants its own change
with its own evidence. Until then a consensus input is taken on a peer's word.

## 7. `TenureAccounting::earnings` is unbounded

~130 bytes per tenure, and now written per block rather than per round. Grows with
the chain.

## 8. Two self-inflicted problems, for the record

**A dead event observer in the mainnet config.** `event_observers =
["http://127.0.0.1:20460"]` pointed at the node's *own* RPC port, which does not
serve `/new_block`. Every block paid five retries with backoff. Fixed, but it
corrupted every throughput number quoted before it was found, and it is why issue 4
looked like a CPU problem.

**Three wrong attributions, each fixed only by measuring the running process.** The
restart cost was blamed on sortition re-derivation (the tracker could not advance
there at all); the follower was said to exit after each round (that message is the
SIGTERM handler, and a harness timeout was killing the process group); and "all
cores saturated" sent two agents after Cranelift compile time when the process was
at 16% of one core waiting on the network. The lesson is uniform: sample
`/proc/<pid>/stat` and `/proc/<pid>/io` on the live process before believing any
story about where time goes.

## What was fixed today, for context

Six divergences, five fixed, each a different cause: `as-contract` leaking its
sender on an early return; a `let`-bound placeholder laid out for the binding;
`fold` not widening its accumulator over a buffer; a tuple narrowed by position
instead of by name; `default-to` dropping a field it never converted. Plus the
matured-tenure fee rule, the SIP-029 emission table, and a sponsor dropped from
every sponsored contract call.

Throughput: CPU per block fell **9.6x** (`call_function` was building a fresh
`Engine` and recompiling on every contract call), one peer round-trip per burn view
instead of per block, and startup silence from **6 min to 20 s** (an 8.6-million-row
ancestor walk to use one entry).
