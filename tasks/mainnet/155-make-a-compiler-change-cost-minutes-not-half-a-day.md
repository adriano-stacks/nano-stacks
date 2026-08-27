---
id: "155"
title: "Make a compiler change cost minutes, not half a day"
status: pending
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
| Sortition derivation | ~1 h | derives ~4,000 burn blocks forward before any Stacks block can execute. |
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
- [ ] **Let `verify-checkpoint` check a manifest without re-reading the payload.**
      The third pass proves nothing the second build has not already proved
      *within one ceremony* — it matters for a third party, which is why it stays
      the default. A `--manifest-only` mode would cut ~20 min from a re-issue.
- [ ] **Reuse a pristine imported state across compiler changes, verifiably.**
      The import is deterministic from the payload, so the same state can be
      adopted under a new compiler *if* it is checked rather than trusted: the
      state's root at the checkpoint height must equal the attested
      `published_state_index_root`, the provenance must name the same content root
      and height, and nothing may have executed past the checkpoint. Then a
      compiler change costs a reflink and a repin instead of three hours, and the
      repin is no longer a hand edit but a checked operation. This is the large
      one and the one worth most.
- [ ] **Stop re-deriving the sortition chain from the checkpoint's burn height on
      every fresh import.** The derivation is a pure function of Bitcoin and the
      checkpoint, so it is the same 4,000 burn blocks every time; a fresh state
      could adopt a previously derived chain under the same verification as above.
- [ ] Keep a warm burn-in state at a known height, refreshed as the release run
      advances, so `fast-mainnet-replay.sh` always has a recent starting point
      rather than only the checkpoint.

## Acceptance Criteria

- A clarity-wasm change can be put in front of real mainnet blocks in under ten
  minutes, without a hand edit to any state.
- A re-issued attestation costs one read of the payload, not three, and two
  independent manifest builds still agree byte for byte.
- Every shortcut is *checked* against the attestation rather than trusted, and any
  state that cannot prove its provenance is refused exactly as it is today.
- The diagnostic paths remain unable to produce release evidence: task 053's
  claims still require an import nothing hand-edited.
