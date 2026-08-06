---
id: "048"
title: "Carry complete mainnet tenure accounting"
status: completed
priority: critical
effort: medium
type: bug
group: mainnet
dependencies: ["043", "056"]
tags: ["mainnet", "checkpoint", "chainstate"]
created_at: 2026-08-02
completed_at: 2026-08-06
---

# Carry complete mainnet tenure accounting

## Objective

The mainnet capture's `native-effects.json` contains one matured effect at
coinbase height 251,321 and no `tenures` or `coinbase_schedule`. It was captured
before [[043-carry-every-unmatured-tenure-with-the-checkpoint]] fixed the Hacknet
export. The next mainnet tenure cannot derive its earnings, and the first payout
not explicitly seeded must fail with `UnknownTenure`.

## Tasks

- [x] Recapture mainnet accounting with the complete maturity window, emission
      schedule, current started tenure and accumulated fees.
- [x] Make capture fail when any required tenure in the maturity window is
      absent instead of writing a partial checkpoint.
- [x] Validate network and checkpoint tenure height against the exported
      schedule and entries.
- [x] Replay across at least 101 tenure starts, including a restart, and compare
      every state root.
- [x] Pay the maturing tenure's parent its own tenure fees, not the child
      tenure's fees.
- [x] Rebuild the live accounting from accepted chain history and reject the
      result unless all 201 required tenures are present and contiguous.
- [x] Re-execute mainnet block 8,673,864 from the reconstructed state and match
      its expected root before counting any later replay depth.
- [x] Replace the incomplete mainnet artifact used by the node and scoreboard.

## Acceptance Criteria

- The artifact contains every pre-checkpoint earning needed until nano's own
  executed tenures mature.
- The first post-checkpoint tenure and the first nano-derived maturity both match
  stacks-core.
- Missing, duplicate or short accounting windows are rejected during capture
  and startup.
- Repeatedly rejecting the first maturity block cannot change `started`, tenure
  earnings or matured effects on disk.

## Why this task is open again

The replacement artifact now contains 102 tenures and three schedule entries,
and its structural fixture test passes. That is necessary but it does not meet
the behavioral acceptance criterion: replay has crossed only a few tenure
starts, not the 101 required to observe nano-derived earnings mature.

The live accounting file is also polluted by failed retries at 8,665,780 as
described in [[056-make-rejected-block-execution-leave-no-state]]. It is not
evidence for this task and must be regenerated before the 101-tenure replay.

## `rebuild-accounting` needs to say where it is

Re-deriving mainnet accounting from a public peer walks every block of every
tenure in the maturity window — roughly 200 tenures — and a rate-limited peer
turns most requests away. In practice that is **over an hour with no output at
all**, and nothing distinguishes it from a hang: no tenure counter, no block
counter, no note when a request is retried.

The retry itself is right (`count_fees` backs off up to 8 times, because a
repair that is not complete is worth nothing). What is missing is saying so.
It should log the tenure it has reached, so a run can be judged rather than
waited on.

## The silence was hiding a starved backoff, not a slow walk

`/proc/<pid>/io` showed the 1h45m run had read **nothing in 90 seconds**, on the
same socket, with 6 seconds of CPU. It was not slow; it was starved.

The cause was in `nano-sync`'s 429 handling: it took the peer's `Retry-After`
and applied the same 2-second ceiling it uses for its own guess, so a peer
asking for a minute was asked again two seconds later — earning another 429,
indefinitely. Fixed: a peer's answer is honoured as given, bounded at two
minutes so a broken header cannot park a catch-up.

With that and the progress line in, the same walk runs visibly:

```
tenure 251419: 2 counted, 199 to go
...
tenure 251394: 27 counted, 174 to go
```

About 0.6 tenures a minute against a public peer, so the full window is a
multi-hour run — but a bounded and observable one, and it reaches the tenure
that matters (251321) around the halfway point.

## The replay reached the first nano-derived maturity

The durable mainnet chain now matches **8,263 consecutive roots**, through
8,673,863. Block 8,673,864 starts tenure 251,422 and is the first block that
matures a tenure nano accounted for itself. Its receipts succeed, but its root
does not match.

The discrepancy named a consensus rule: the new tenure pays its parent the
parent's own accumulated fees. Nano paid the new tenure's fees instead. The rule
is fixed and covered by a focused test, but it is not yet live-root evidence:
the existing `accounting.json` has only 158 tenure records and is missing 44,
including 251,335–251,336 and 251,378–251,419.

`rebuild-accounting` is reconstructing the 201-tenure window from chain history.
Do not resume against the old file or close this task on the formula unit test.
The acceptance event is a clean reconstruction followed by a matching root at
8,673,864 and a restart that preserves the same accounting.

## The checkpointed payouts agree with the corrected rule — a false alarm, corrected

An earlier version of this note claimed the mainnet capture's one `matured_effects`
entry encoded the pre-8,665,722 fee rule and so silently overrode the corrected
derivation. **That was wrong**, from an arithmetic slip, and reading the capture
tool settles it.

`capture-fixtures` builds each entry from `scheduled_payment(coinbase_height -
MINER_REWARD_MATURITY)` — the *maturing* tenure — and splits it:

```rust
let (own, parent) = if earned.nakamoto { (earned.coinbase, earned.anchored) } else { … };
let previous = scheduled_payment(coinbase_height - MINER_REWARD_MATURITY - 1) … recipient;
```

so the parent's **amount** is the maturing tenure's anchored fees and the parent's
**recipient** is the tenure before it. That is exactly the rule the chain proved at
8,665,722 and exactly what `effects_for_tenure` now derives. For coinbase height
251,321 both give 27,865,898, tenure 251,221's own fee total. The slip was reading
`earned.fees` in the derivation as the *previous* tenure's rather than the maturing
one's.

So the two sources agree and the precedence between them does not matter. The
entries are also not merely convenient: `scheduled_payment` reads the archive's
own scheduled-payment rows, so they come from stacks-core rather than from a
reimplementation of its arithmetic.

Attempting to reverse the precedence anyway broke three tests that assert it, which
was the right signal for the wrong reason — there was nothing to fix.

## `TenureAccounting::earnings` is unbounded

About 130 bytes per tenure, and since the ledger landed it is written per block
rather than per catch-up round. It grows with the chain.

## The hole, measured: eight tenures nano executed and never recorded

The durable ledger held **193 tenures spanning 251,220 to 251,420 with exactly
eight missing — 251,322 through 251,329**. Not the checkpoint's fault: the
capture's `native-effects.json` carries 251,220–251,321 with no gaps. Those eight
are the first tenures nano itself executed after the anchor, and nothing recorded
them.

`MINER_REWARD_MATURITY` is 100, so tenure 251,422 pays 251,322. The replay had
reached tenure 251,420. It would have run for another two tenures and then failed
with `UnknownTenure`, having sealed 8,000 blocks that all had to be thrown away —
which is precisely the outcome `check_maturity_window` exists to prevent, and it
was not being called.

**The cause is not determinable from this state, and saying so is the honest
answer.** `accounting.json` in the same directory has the *same* eight missing and
stops at 251,351, so the hole predates the ledger and came in through the
migration from that file. The blocks concerned are 8,665,60x–8,665,80x, which is
the range `047` records a root mismatch at 8,665,780 being retried 1,417 times,
and the log for that period is gone. What *is* determinable: the current code path
records every tenure — 91 contiguous from 251,330 to 251,420 — so this is
historical rather than live. `start_tenure` calls `record_earnings` on every
tenure start where a coinbase schedule exists, and the schedule is present.

## The resume path validated nothing at all

That is the bug worth fixing, rather than the hole. `check_maturity_window` ran on
the checkpoint and on `accounting.json` — and *not* on a recovered ledger, which is
the path a running node actually takes. So the one artifact a node executes from was
the one artifact nobody checked. `recover_ledger` now applies it, which is one
comparison at startup against hours of execution.

Note the interaction with `known_earnings_span` answering the **contiguous** run:
the outer pair 251,220–251,420 says the window is 201 long and healthy. Only
counting down from the top says it is 91, and the first unpayable tenure is 27
away.

## `repair-ledger`, and where its numbers come from

`cargo xtask repair-ledger <state-dir> <stacks-core-index.sqlite>` fills the holes
in every stored ledger row from **stacks-core's own `payments` rows**, for the same
reason `capture-fixtures` reads them there: they are stacks-core's arithmetic
rather than a reimplementation of it. `rebuild-accounting` walks a peer summing
fees, which is a reimplementation, takes hours against a rate limit, and writes
`accounting.json` — a file the node stopped reading once a block had sealed, so its
repair went nowhere.

It also settles the one field a fee-walk would have had to guess: tenure 251,329's
coinbase is **2,000,000,000**, twice the emission, because the burn block before it
produced no sortition and the coinbase accumulated. A reconstruction that assumed
the schedule's amount would have been silently wrong there.

**Verified before being trusted.** For every tenure the capture and the archive
share, recipient, coinbase and fees agree exactly — including 251,321's oddly small
15,114, which is the kind of value a plausible-looking reconstruction gets wrong.

All 256 rows were repaired, each to 251,220–251,420 contiguous, and a second run
says "every ledger row already owes a contiguous window".

### Two mistakes made getting there, both worth recording

**The first repair wrote TEXT into a BLOB column.** Every row passed validation and
none could be read afterwards: rusqlite's byte reader refuses a text value, so the
node started with "no ledger was committed with block …" and fell back to
`accounting.json` — which still had the hole, so the new startup check fired, on the
wrong artifact, for the right reason. The tool validated what it was *about to*
write rather than what came back out. It now writes `x'…'` and re-reads the row,
checking both the type and the bytes.

**Then the `UPDATE` was too long for an argument list.** A repaired row is 67 KB of
hexadecimal and `execve` refused it. `sqlite_script` feeds statements on standard
input, where there is no limit to hit.

Both are the same lesson in different clothes: verify through the path the reader
uses, not the path the writer took.

## The `fees` field named the wrong tenure, and both ends agreed on it

251,321's oddly small **15,114** was recorded above as a value a reconstruction
would get wrong. It is stranger than that: it is not tenure 251,321's fee total
at all. It is tenure 251,320's.

stacks-core cannot total a tenure's fees until the next tenure change proves the
tenure over, so `payments.tx_fees_anchored` on tenure *T*'s schedule row holds
the fees of *T − 1* — the field is literally `MinerPaymentTxFees::Nakamoto {
parent_fees }`. `capture-fixtures` copied it into the checkpoint's `fees` under
*T*'s name, and `effects_for_tenure` read it as *T*'s own and paid it to *T − 1*'s
recipient. Two mistakes cancelling: for every tenure the checkpoint carried, the
amount and the recipient were both right.

They stop cancelling at the first tenure nano totals itself, because `add_fees`
counts the *current* tenure's blocks. That is 251,321, which starts nine blocks
past the anchor, and its maturity is 100 tenures later at block **8,673,846** —
where the replay parked with matching receipts and a wrong root
([[060-make-the-consensus-execution-engine-explicit-and-r]] has the diagnosis).

One convention now, held at both ends: **`fees` is what this tenure's own
transactions paid**. `capture-fixtures` reads it from the following tenure's
schedule, as `hacknet/signer-checkpoint.sh` already did; `effects_for_tenure`
takes the recipient and the amount of the second credit from the same entry,
`earnings[matured - 1]`, so they cannot come apart again.

`repair-ledger` restates the field as well as filling holes, from the same
`payments` rows, and that makes it a checker rather than only a repair tool:
against the parked state it moved 110 of 201 entries and left 91 alone — the 110
being everything the checkpoint handed over or a previous repair had filled, and
the 91 being every tenure nano totalled itself, agreeing with stacks-core to the
microSTX. The split falls exactly on the seam between carried and executed, which
is what a convention mismatch looks like when you measure it instead of arguing
about it.

Checkpoints already written stay readable, but they carry the field one tenure
out of phase and have to be restated. `/home/aldur/mainnet-capture` was, with the
original kept beside it as `native-effects.json.parent-fees`.

## The behavioural criterion is met: 392 tenure starts, 26 restarts

The pristine mainnet replay has executed **392 tenure starts** past the
checkpoint's anchor at 251,321 — `started` is 251,713 — across **26 process
starts**, with every state root matching. Well past the 101 this task asked for,
and past it four times over.

The maturity boundary that opened this task is behind it. Tenure 251,421 was the
first payout nano derived from earnings it recorded itself, and it is where the
fee-*phase* bug surfaced (recorded on [[060]]): stacks-core cannot total a tenure's
fees until the next tenure change proves it over, so it records them in the
following tenure's schedule and pays them to *that* schedule's parent. Nano paid
the right recipient the wrong tenure's money, and no receipt showed it. Fixed, and
the 292 tenure starts since then have all matched.

Six distinct heights have ever diverged — 8,665,719, 8,665,722, 8,666,585,
8,667,509, 8,668,096 and 8,673,846 — each one a named bug with a regression test,
and none of them recurring.

**The earnings bound is visibly working**, which is worth saying because a bound on
consensus accounting is the kind of thing that fails quietly: the ledger holds
exactly 201 tenures and the window slides — 251,513 to 251,713 now, where it was
251,220 to 251,420 before the bound landed. Contiguous, and long enough for the
startup check to judge.

## What is left, and it is capture-side

The two unchecked items are both about the *export* refusing to write something
short, rather than about replay: capture failing when a required tenure in the
maturity window is absent, and validating the network and checkpoint tenure height
against the exported schedule. `repair-ledger` now makes the hole *fixable* from
stacks-core's own rows, and the startup check makes it *visible* — what neither
does is stop a short capture being written in the first place.

## Capture refuses a short window now, on contiguity rather than count

`write_native_effects` already refused a window shorter than the maturity window —
by **counting** its entries. A count cannot see the failure that actually happened:
the live ledger held 193 tenures spanning 201 heights with eight missing in the
middle, so its outer bounds said complete and long, and the first payout it could
not make was 27 tenures away. The `continue`s in the export are how a hole gets in
— a tenure the archive cannot answer for is skipped rather than refused.

Three refusals now, each naming what it saw: a hole anywhere inside the window
(with the missing height), a window that stops short of the tenure the captured
blocks reach, and a window shorter than a maturity window. The message for the hole
says why a hole is worse than a short window — "not a shorter window, a delayed
failure" — because that is the reasoning somebody re-reading this will need.

Exercising those refusals needs the 505 GB stacks-core archive, so they have no
offline test and `mainnet_accounting.rs` says so out loud. What that file does check
is an artifact that got *past* them, and it now also validates the schedule: the
network flag, and that `first_bitcoin_height` is mainnet's 666,050. Without a
schedule a node cannot price the coinbase of a tenure it executes itself; priced
against the wrong network it prices every one of them wrongly, because the emission
intervals and the first burn height both differ, and the first tenure start past the
checkpoint diverges with nothing saying why.

That closes this task. What it does not close is next door: `repair-ledger` reads
the archive, and a node with no archive cannot repair itself — see
[[054-join-and-synchronize-over-the-stacks-p2p-network]]'s bulk-history item.
