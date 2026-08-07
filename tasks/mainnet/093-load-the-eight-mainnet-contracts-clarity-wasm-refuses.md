---
id: "093"
group: mainnet
title: "Load the eight mainnet contracts clarity-wasm refuses"
status: pending
priority: critical
effort: large
dependencies: ["073"]
tags: ["mainnet", "vm", "clarity", "wasm", "conformance", "release"]
created_at: 2026-08-08
type: bug
---

# Load the eight mainnet contracts clarity-wasm refuses

## Objective

Eight contracts the network deployed and accepted cannot be compiled by
clarity-wasm. Task 073's margin sweep found them and left them unclassified;
they are classified now, and they are conformance bugs rather than harness
artifacts or invalid sources. Under the release rule — production never falls
back to the interpreter, so a mainnet-valid contract that cannot load is a
code-generation bug — each one blocks the release.

## Classified 2026-08-08, on the production path

Every one was re-run through nano-vm's own `compile_under` via
`cargo xtask check-module`, not through the sweep's `clar2wasm::compile`. All
eight reproduce, so **none is a sweep-harness fault**.

And none is a Clarity analysis failure either, which was the other way out. Every
error string exists **only in clarity-wasm** and nowhere in clarity's shared
analyzer:

| error | lives in | contracts |
|---|---|---|
| `Not implemented` | `clar2wasm/src/wasm_generator.rs` | `SPXWGJQ101N1C1FYHK64TGTHN4793CHVKTJAT7VQ.amm-swap003`, `SP93HY1S36HJXC5MY9TKKKPDM2YZSKK385VBV15P.pool`, `SP11KTHSX0QSQWB4KRAH2S6BA058XPEZW4HT0D55W.pool` |
| `Incompatible types for duck typing: buff 1 against string-ascii 256` | `clar2wasm/src/duck_type.rs` | `SP2BRB6P0BK6T35DHTGXCV6MZ5TGRN5E0RKZ1T8B5.gated-pages`, `.gated-pages-004`, `.gated-pages-005`, `.gated-page-006` |
| `Tuples fields should be typed` | `clar2wasm/src/words/tuples.rs` | `SP1J70VWT7MRRP635NZ6E3J86PFE78JFXS0QR5ZAH.trajan-endorsement-alpha` |

So this is task 073's own phrasing, confirmed: **the network accepts what nano
cannot load.** Three distinct defects, eight contracts.

The sources are preserved at `/home/aldur/nano-073-sources/`, dumped read-only
through `NANO_DUMP_SOURCE` from `/home/aldur/mainnet-8716986/state`, 15–35 KB
each. They are the reproduction inputs; nothing here needs a live node.

```sh
cargo xtask check-module /home/aldur/mainnet-8716986/state \
  SPXWGJQ101N1C1FYHK64TGTHN4793CHVKTJAT7VQ.amm-swap003
```

## Tasks

- [ ] Name the word `Not implemented` refuses, for each of the three contracts.
      `wasm_generator.rs` raises it where a `SymbolicExpression` shape has no
      code generator; the message does not say which, and it should.
- [ ] Reduce each of the three families to the smallest source that reproduces,
      from the real contract rather than towards it — the lesson of task 086,
      where two invented reductions passed while the real one differed.
- [ ] Fix the duck-typing rule: `buff 1` against `string-ascii 256` is a
      comparison four deployed contracts make and the network permits.
- [ ] Fix `Tuples fields should be typed` for whatever shape
      `trajan-endorsement-alpha` uses.
- [ ] Crosscheck each minimized source against the reference interpreter for
      result, receipt, cost, events and writes — not merely that it compiles.
- [ ] Add every reduction to the mandatory conformance suite, and re-run the
      full mainnet sweep to confirm 137,340/137,340.

## Acceptance Criteria

- All eight contracts compile and load through `compile_under`, with no
  per-contract exception, interpreter fallback or healing path.
- Each of the three defects has a minimized regression in the mandatory suite
  that fails on the current revision.
- The mainnet-state sweep reports every imported contract compiling, so the
  denominator is the whole state rather than the part that worked.
- Interpreter and clarity-wasm agree on the minimized sources, not just on
  whether they compile.

## Evidence that opened this task

Task 073 measured the locals boundary and closed it — no source-level shape
reaches wasmtime's limit any more — but its sweep over all 137,340 imported
mainnet contracts left 8 failing and unclassified, with a note that "some may be
harness artifacts". They are not. Splitting them out of 073 keeps that task's
locals result, which is finished and green, from being held open by three
unrelated code-generation defects.
