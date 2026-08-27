---
id: "142"
title: "Qualify the hardened Epoch 4.0 follower for mainnet"
status: pending
priority: critical
effort: small
dependencies: ["106", "082"]
tags: ["mainnet", "release", "consensus", "qualification"]
created_at: 2026-08-14
parent: 053
type: chore
---

# Qualify the hardened Epoch 4.0 follower for mainnet

## Objective

Act as the fail-closed roll-up for the new hardening program. Apply the
mainnet-ready label only to the minimal follower artifact whose complete
dependency graph, independently reproduced checkpoint and full-cycle evidence
are finished.

## Tasks

- [ ] Confirm taskmd reports every dependency complete and no critical/high
      release task, blocking semantic ignore, declared differential, advisory
      exception past expiry or unowned qualification input remains.
- [ ] Hold one continuous interval that starts before a reward-cycle prepare
      phase and runs through the rollover and the complete following cycle,
      inherited from the cancelled
      [[138-run-a-multi-operator-full-reward-cycle-qualificati]]: 106 proves
      twenty-four hours and 082 crosses one boundary, and neither is a full
      cycle held without interruption.
- [ ] Exercise loss of one peer, one Bitcoin backend and one optional edge
      service without changing the canonical result or requiring hosted HTTP,
      also inherited from 138.
- [ ] Run the qualifying release report against the signed artifact, checkpoint,
      mainnet capture and the state that interval produced.
- [ ] Verify that the report binds the clean source, reproducible artifact,
      Epoch 4.0 profile, engine/SBOM, checkpoint builders and raw operator
      evidence.
- [ ] Reconcile [[053-pass-the-mainnet-node-release-gate]] line by line; a stale
      checked box is not evidence and an unexecuted gate is not a pass.
- [ ] Publish a signed go/no-go record naming residual operational assumptions,
      supported platforms, resource floors, rollback procedure and incident
      contacts.
- [ ] Tag and publish exactly the qualified artifact without rebuilding or
      changing any input.

## The qualifying gate, run 2026-08-23

`cargo xtask release-report` was run rather than reasoned about, and it refuses
before reaching any evidence:

```text
revision  9636879558c43871af1aba450d5db266a2d7ded4
branch    main (BUILD-RELEVANT CHANGES)
FAIL      tracked crates/nano-conformance/tests/conformance/mainnet_divergence.rs
FAIL      tracked crates/nano-conformance/tests/conformance/trait_equality.rs
release qualification stopped: build-relevant source changes are not a
reproducible release input
```

So this task's second acceptance criterion cannot even be attempted while any
build-relevant file is uncommitted. Those two files are another session's
in-progress task-147 work, which makes 147 a hard predecessor of this gate and
not merely a parallel task. `cargo xtask release-tree-status` reports the same two
files and is the cheap way to check before attempting a run.

The report also states its own limits, which is worth quoting because it forecloses
using it as a shortcut:

> It is not evidence for holding mainnet tip for 24 hours, a live Bitcoin
> reorganization, or a stock stacks-signer run against this binary. Those require
> the named task-053 qualification runs.

A run from a clean worktree at the same revision is the way to see what else is
outstanding once the tree settles, since a worktree at HEAD satisfies the
reproducible-input rule without disturbing anyone's working copy. Done from
`/home/aldur/release-build-88920833`, the report clears the revision section
(`branch HEAD (clean)`) and stops one step later:

```text
release candidate
  FAIL: qualification requires --candidate and --public-key
```

**That candidate cannot be produced on this host, by design.**
`release-candidate prepare` requires `--secret-key <minisign.key>` and
`--public-key <minisign.pub>` alongside the checkpoint, its builder policy and
signatures, a provenance file or state directory, a Bitcoin RPC and a RustSec
advisory database. Everything but the keys is here — the advisory DB is at
`~/.cargo/advisory-db`, and the re-attested bundle, `builder-policy.toml` and
`signatures-ee7af998` are in place. No minisign key exists anywhere on the host,
and `release/README.md` writes the public one as
`/path/from/a/trusted/channel/minisign.pub`, so the release signing material is
meant to arrive from outside this machine. Generating a release identity here
would fabricate the very thing the trusted channel exists to establish.

So this gate has three distinct prerequisites, and only one is a clock: a tree with
no build-relevant changes (task 147), the operator's minisign key material, and
the full-cycle interval above.

The precedent artifact says the same thing about what freezing a build does and
does not achieve — `/home/aldur/release-candidate-3018ac4a/ARTIFACT.md`: *"This
freezes the candidate artifact. It is not live qualification evidence until it
imports the independently signed checkpoint and completes the required tip hold
and full-cycle runs."* Importing the independently signed checkpoint is what the
subject started doing on 2026-08-23; the tip hold and full cycle follow it.

## Acceptance Criteria

- Every formal dependency and every task 053 acceptance criterion is complete
  with artifact-bound evidence; no waiver converts missing evidence into a pass.
- `cargo xtask release-report` succeeds from the published clean source and
  rejects any substituted artifact, checkpoint, profile or evidence bundle.
- The published checksum is identical to the artifact that produced the
  qualification evidence. Single-operator only: 138 was cancelled because this
  project has one operator, so the go/no-go record must name that as a residual
  operational assumption rather than imply independent corroboration.
- The go/no-go record is signed and independently reproducible.
- Only after these conditions hold may tasks 142 and 053 be completed.

## A flaky gate, recorded rather than papered over, 2026-08-26

`nano_rpc::tests::abandoned_read_only_calls_keep_their_worker_budget_until_done`
failed once in a full-workspace run under heavy parallel load — it expected
`503 SERVICE_UNAVAILABLE` from a saturated read-only pool and got `408`, because
the probe request timed out before the semaphore reported saturation. It passes
in isolation, twice, and nothing in that path changed.

It is written down because a qualification run cannot rest on a gate that fails
under load: a red CI run that is "probably the flake" is indistinguishable from
a real regression at the moment it matters. The fix is to make the probe wait for
saturation rather than race it. Deferred on 2026-08-26 because the tree was frozen
for the release import.

**Fixed 2026-08-27**, once the tree was no longer frozen. The race was in the
test's precondition, not in the pool: it waited for *one* worker to enter the
chain and then probed, but one worker in the chain does not mean the pool is full.
A probe arriving while a permit was still free took that permit, blocked on the
chain mutex the first worker holds, and answered `408` on its own timeout instead
of the `503` saturation refusal. Under parallel load that is the likely
interleaving rather than a rare one, which is why it passed in isolation twice.

So saturation is now a precondition rather than a sample: the test waits for
`available_permits() == 0` before aborting anything, and the abort still leaves
the permits held, which is the property under test. Verified 20 for 20 in
sequence, and three full `nano-rpc` suite runs at 16 test threads with the box
deliberately loaded — 70 passed, 0 failed each time. The `entered` wait also went
from a one-second to a ten-second budget, since a loaded box is exactly when it
must not be the thing that fails.

