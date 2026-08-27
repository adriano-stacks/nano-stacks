---
id: "155"
title: "Make a compiler change cost minutes, not half a day"
status: in-progress
priority: high
effort: medium
dependencies: []
tags: ["mainnet", "release", "tooling", "checkpoint"]
created_at: 2026-08-27
type: improvement
---

# Make a compiler change cost minutes, not half a day

## Objective

A clarity-wasm fix moves `COMPILER_IDENTITY`, and the release path answers that
with a signing ceremony and a fresh import before a single block executes. On
2026-08-27 that price was paid **five times** in one day, and three imports were
thrown away part-finished. The guard is right; the price is not, and most of it is
recomputation of things that did not change.

## Measured, on this host

| Step | Cost | What it actually spends it on |
|---|---|---|
| Ceremony | ~55 min | reads the 384 GB payload **three times** — two manifest builds and one verify — at about 250 MB/s. Disk bound, not CPU bound: parallel hashing would not help. |
| Import | ~3 h | rebuilds a 20 GB MARF and a 14 GB Clarity database from that payload. Its output is a pure function of the payload; nothing has executed yet. |
| Sortition derivation | ~1 h *as observed* | derives ~4,000 burn blocks forward before any Stacks block can execute. **Measured later at ~8 min of actual work — see below; the rest was contention.** |
| Fetch | ~1 h | ~180,000 blocks over HTTP from discovered peers. |
| Catch-up | ~9 h | 240 blocks/min at 139 ms a block. This is the part that finds bugs. |

Only the last line earns its time. The block that broke twice today,
**8,667,169**, sits 1,568 blocks past the checkpoint — about seven minutes of
execution behind five hours of setup.

## Tasks

- [x] Reflink an already-imported state, repin the profile in both places that
      record it, and run a candidate against the copy:
      `scripts/fast-mainnet-replay.sh`. Three minutes per attempt, and both of
      today's consensus differentials were found with it. Diagnostic only — a
      hand-repinned profile lets a state claim a compiler that never produced it.
- [ ] **Overlap the two manifest builds.** They read the same extents through
      reflinks, so the second is served from page cache for whatever the first has
      just read. Two reads become roughly one; independence is untouched, because
      the builds share nothing but a read-only payload and the whole point of the
      `cmp` is that they agree without knowing about each other. Drafted in
      `run-ceremony-template.sh`; needs one ceremony to confirm the saving and
      that the manifests still match byte for byte.
- [x] **Let `verify-checkpoint` check a manifest without re-reading the payload.**
      The third pass proves nothing the second build has not already proved
      *within one ceremony* — it matters for a third party, which is why it stays
      the default. Landed as `verify-checkpoint --manifest-only`, cutting ~20 min
      from a re-issue; see the section below for what it does and does not say.
- [x] **Reuse a pristine imported state across compiler changes, verifiably.**
      Landed as `stacks-node adopt-imported-state --state <dir> --checkpoint <bundle>`.
      The import is deterministic from the payload, so the same state can be
      adopted under a new compiler *if* it is checked rather than trusted: the
      state's root at the checkpoint height must equal the attested
      `published_state_index_root`, the provenance must name the same content root
      and height, and nothing may have executed past the checkpoint. Then a
      compiler change costs a reflink and a repin instead of three hours, and the
      repin is no longer a hand edit but a checked operation. This is the large
      one and the one worth most.
- [x] **Stop re-deriving the sortition chain from the checkpoint's burn height on
      every fresh import.** Closed by measurement, not by code: the derivation
      costs about **eight minutes** of work, not an hour, so there is nothing here
      worth the risk of reusing an unverifiable chain. Breakdown below.
- [~] Keep a warm burn-in state at a known height, refreshed as the release run
      advances, so `fast-mainnet-replay.sh` always has a recent starting point
      rather than only the checkpoint. **The mechanism exists and the automation
      is deliberately not built — see below.**

## Acceptance Criteria

- A clarity-wasm change can be put in front of real mainnet blocks in under ten
  minutes, without a hand edit to any state.
- A re-issued attestation costs one read of the payload, not three, and two
  independent manifest builds still agree byte for byte.
- Every shortcut is *checked* against the attestation rather than trusted, and any
  state that cannot prove its provenance is refused exactly as it is today.
- The diagnostic paths remain unable to produce release evidence: task 053's
  claims still require an import nothing hand-edited.

## The three-hour item is done, 2026-08-27

`adopt-imported-state` lets an artifact use a state another compiler imported,
and it is a proof rather than a repin. Four checks, all before anything is
written, and a state that fails any of them is left untouched:

- the recorded checkpoint matches the one offered field by field — format,
  height, state identifier, state root, Bitcoin height — with the profile
  fingerprint the single exception, because it is the thing being changed;
- the state seals the checkpoint's own block, so nothing has executed past it and
  no compiler's decisions are in it;
- the root it seals at that block is the root the checkpoint claims, which a
  signed Nakamoto header endorsed;
- and only then are the two records rewritten, together.

Each refusal is its own typed variant with its own message, and each is tested:
no record at all, another checkpoint (three ways), a state that has executed, and
a root that disagrees with what both records claim. Demonstrated against real
mainnet state as well — the release run's own mid-replay copy is refused with
"the state is sealed at be28458d… and the checkpoint ends at a8733890…; a state
that has executed is one compiler's continuation, not an import", exit code 1.

`CheckpointProvenance::rewrite` is new beside `record`, which refuses a directory
naming a different checkpoint. That refusal is what keeps one chain's state from
being extended under another chain's blocks and it stays; adoption is the single
case where the record must change and the state must not, and the caller proves
that before asking.

What this replaces: a hand edit to `checkpoint-provenance.toml` and the
`consensus_profile` row, which is what `fast-mainnet-replay.sh` had been doing
and what made every copy it produced diagnostic-only. That script now tries the
adoption first and falls back to the hand repin only for a state that has already
executed — so a pristine copy carries a proof and a dirty one carries a label.

The acceptance path on real data will run the next time a compiler change lands,
which is exactly the case it exists for: the unit test covers it on a real MARF
with real provenance, and the mainnet demonstration above is the refusal.

## `--manifest-only`, 2026-08-27

The payload read is now a named choice rather than an implicit one:
`PayloadBytes::{Verified, Assumed}`, threaded through
`verify_checkpoint_bundle_with` and `verify_signed_checkpoint_bundle_with`.
Verified stays the default and stays the only mode that may authenticate a
payload before import — `runtime.rs` and `adoption.rs` both keep it, and signing
has no assumed mode at all, because a builder attests bytes it has read.

What the assumed mode still checks, all of it cheap:

- the manifest's content root, **recomputed from its own per-file digests**, so an
  altered entry is caught by arithmetic instead of by a read;
- the builder signatures over that root, against local policy at that height;
- the claims against `checkpoint.toml`;
- that the checkpoint's Bitcoin block is locally canonical;
- and the signer attestation, re-derived from the checkpoint block and reward set.

What it does not check is whether the files on disk are the files the manifest
describes. So it establishes that a bundle **is attested**, never that a payload
**is the attested one**, and the command prints that distinction rather than
letting the output look like the default's.

Three tests pin exactly that boundary: an attested bundle verifies without a read,
an edited manifest entry is refused without a read, and a swapped payload byte
passes the assumed mode and is refused by the default — the last one being the
honest statement of the trade rather than a claim to have avoided it.

## The sortition item, measured 2026-08-27 — and why it is not adoption

Two things came out of looking, and both contradict the item as written.

**There is nothing to verify an adopted forward chain against.** State adoption
works because the checkpoint *publishes* a state root that a signed Nakamoto
header endorses, so a state can prove it is the import. The sortition chain a
fresh import derives runs from the checkpoint's burn height *forward*, and the
bundle attests only the history up to that height (`sortition/history.bin`).
Above it there is no published root, so copying a chain in and calling it verified
would be a hand repin with extra steps — exactly what `adopt-imported-state`
exists to stop being.

The one sound corroboration available is signed headers: every Nakamoto block
commits to its tenure's `consensus_hash`, so a chain that agrees with every
signed header the node holds is not a fabrication. A fresh import holds no blocks
yet, so that check cannot run at adopt time — but it already runs, per block, as
the executor validates each one. So an adopted chain's errors surface at the first
block of the affected tenure rather than at adoption, which is a worse place to
find them and not a proof.

**Neither expected cost is the hour.** Measured against this host's own bitcoind:

| Component | Measured | Notes |
|---|---|---|
| Bitcoin fetch | ~100 ms per burn block, so **~7 min** for 4,100 | `getblockhash` + `getblock` over loopback, ~3 MB a block |
| History rewrite | **~28 writes** per derivation, ~100 ms each | 12.7 MB of JSON, but written per *walk batch*, not per burn block: observed once every ~5 min on the live follower, growing 1.4 KB |

I expected the 12.7 MB rewrite to dominate — one write per burn block would have
been 52 GB and ~20 min per import — and went looking to throttle it. It is
already batched by the bounded walk, so there was nothing to fix and the throttle
would have been unjustified complexity. Recorded because the wrong hypothesis is
worth as much as the right one here: the remaining time is in parsing 4,100
mainnet blocks and classifying their operations, about 12 million transactions,
which is where any future work on this belongs.

So this item stays open, and its honest form is no longer "adopt a chain" but
"make the derivation cheaper" — the parse, not the fetch and not the write. A
reflinked state already carries its derived chain, which is why
`adopt-imported-state` meets the ten-minute criterion without this.

## The warm state stays on demand, 2026-08-27

Every piece of it already works: `fast-mainnet-replay.sh` reflinks a stopped
state in seconds and `adopt-imported-state` now repins it with a proof. What is
missing is a *stopped recent source*, because the script refuses a live one — a
state directory with a live writer has a WAL a copy cannot interpret.

Keeping one warm permanently is the part not to automate, and today is the
evidence why. A reflink is free at creation and stops being free as both sides are
written: the copy pins the extents the original overwrites. This filesystem lost
**45 GiB an hour** while three finished diagnostic copies were still running, and
reclaiming them took the release run's headroom from 179 GiB back to 291 GiB with
the drain down to 21 GiB/h. A warm copy refreshed automatically as the run
advances would rebuild exactly that, against the one resource that has actually
threatened the release run.

So the recipe stays manual and the trigger stays a planned stop: at the next
restart, reflink the state aside before starting again, and adopt it when it is
needed. On-demand costs seconds at a moment that already exists; standing costs
gigabytes an hour at every other moment.

## The derivation is eight minutes of work, 2026-08-27

Three hypotheses, all measured, all wrong — so the item closes with no code.

| Component | Measured | For 4,100 burn blocks |
|---|---|---|
| Bitcoin fetch | ~100 ms a block (`getblockhash` + `getblock`, ~3 MB, loopback) | **~7 min** |
| Block parse | 6.9 ms a block on a real 4,370-tx mainnet block | **~28 s** |
| History rewrite | 0.2–0.3 s, once per 144-block walk batch, from the node's own log | **~7 s** |

That totals about eight minutes. The hour in the table above was real as *observed*
and misattributed as *cost*: at the time, six node processes were hammering one
bitcoind and the filesystem was absorbing 900 GiB/h of writes from three finished
diagnostic arms. Retiring those took the disk drain from 45 GiB/h to 21 GiB/h, and
the same contention is the honest explanation for the derivation's wall clock.

The parse figure also found a real inefficiency that is **deliberately not being
fixed**: `block_at` calls `get_block`, which deserializes every transaction, then
re-serializes the block to bytes so that `decode_block_with_pre_stx` can
deserialize it a second time. Two parses and a serialize where one parse would do,
measured at 6.9 ms against 3.2 ms. Across a whole derivation that is **15 seconds**,
and this is the code path that extracts burn operations — where a missed operation
is a wrong sortition. Fifteen seconds does not buy a change there.

The lesson worth keeping is about the method rather than the result: every cost in
this task's first table was estimated, and two of the three I checked were out by
one to two orders of magnitude. Measure on the box, under the load the box is
actually carrying.