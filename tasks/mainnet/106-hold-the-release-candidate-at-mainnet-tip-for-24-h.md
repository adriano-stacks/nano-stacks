---
title: "Hold the release candidate at mainnet tip for 24 hours"
id: "106"
status: pending
priority: critical
effort: large
type: chore
group: mainnet
parent: "053"
dependencies: ["037", "046", "047", "054", "079", "082", "107"]
tags: ["mainnet", "liveness", "operations", "release"]
created_at: "2026-08-09"
---

# Hold the release candidate at mainnet tip for 24 hours

## Description

Hold the same release binary at the public mainnet tip for one continuous
24-hour interval. Use P2P only for synchronization and liveness.

## Tasks

- [x] Re-run the checkpoint builder ceremony for the compiler identity the hold
      will run: a node refuses a bundle whose recorded compiler identity is not
      its own and the section is mandatory, so a compiler change invalidates the
      attested bundle and no fixed-compiler replay can start until an attested
      one exists. Hand-editing a copied bundle would forge the builders'
      binding. Moved here from task 146, which closed its differential and left
      this needing the builders rather than a compiler fix.
      Re-issued 2026-08-23, recorded below.
- [ ] Import under the real 2-of-2 builder policy, not the functional one. The
      witness and follower that produced earlier evidence imported under
      `follower-import-final-16e0928a/functional-builders.toml`, a
      single-signature policy whose builder public key is
      `0279be667e…f81798` — the secp256k1 generator point, so its private key is
      1 and anyone can sign for it. That is adequate for exercising the import
      path and worthless as attestation, so the hold must use
      `checkpoint-builder-keys/builder-policy.toml` and the signatures below.
- [ ] Re-run this window's deferred receipt verification end to end green, which
      is the check task 146 built the canonical-record oracle for and which
      cannot run before the bundle above exists.
- [ ] Start the hold only after the clean replay in task 037 and the no-hosted
      P2P qualification in task 054 pass for the same release binary.
- [ ] Run with no hosted data service for 24 continuous hours.
- [ ] Sample once per minute: Bitcoin tip, selected Stacks tip, followed tip,
  executed tip, peer count, queue depths, disk use, memory use, open file count,
  RPC health, and observer backlog.
- [ ] Record every peer change, fork, restart, and block rejection.
- [ ] Compare each new executed state root and receipt set with the oracle.
- [ ] Confirm that RPC and P2P service never expose staged or unexecuted data.
- [ ] Check resource measurements for an unbounded trend.
- [ ] Restart the complete 24-hour measurement after a node defect or process
  stop. Keep planned recovery tests outside the continuous interval.

## Evidence

- Start and end times in UTC.
- One-minute health and resource samples.
- Per-block root and receipt comparisons.
- Peer, fork, rejection, and service logs.
- Final selected, followed, executed, and network tip values.

## The bundle the hold will import, re-issued 2026-08-23

Task 146's ten cost fixes and task 147's refusal fix moved `COMPILER_IDENTITY`,
so the bundle attested on 2026-08-19 — compiler `sha256:1f78d344…`, profile
`5561d364…`, content root `146ade17…` — is refused by the current binary. The
attestation was re-issued for the identity the hold will actually run:

```text
binary            target/release/stacks-node  sha256 34dbb7ac…  (HEAD 4dad76c1)
compiler_identity sha256:ee7af998a74190c44f41b829593ad78ac8effbba98ff1ca9a39dc35b175192d1
profile           6a83746edc16895eb6886c37474ab7693bc31272b5d350366fc4606663965a35
content_root      943ecf7b7c0702a603b4db9f8d554d35caa31cefea543c579f510982ada0580e
bundle            /home/aldur/checkpoint-builder-ee7af998-8665600-20260823/first-build/bundle
signatures        /home/aldur/checkpoint-builder-keys/signatures-ee7af998/
policy            /home/aldur/checkpoint-builder-keys/builder-policy.toml (unchanged, 2-of-2)
verified          content root 943ecf7b… verified by aldur-host-primary, aldur-host-recovery
```

Two independent workspaces, each a reflink clone of the payload, produced
**byte-identical** manifests (`e0c359be…`), which is the determinism evidence at
production scale — 359 GB rehashed twice. The 2026-08-19 payload and its
published signatures were never written to, and the run asserts their digests are
unchanged afterward; the new signatures live in their own directory beside the
old ones rather than replacing them. Scripts and logs:
`checkpoint-builder-keys/run-ceremony-ee7af998.sh`,
`continue-ceremony-ee7af998.sh` and `ceremony-*-ee7af998*.log`.

**Residual operational assumption, named as task 142 requires.** Both builder
keys belong to this host's single operator, so the 2-of-2 threshold separates key
custody, not parties. It is not independent corroboration, and task 138 was
cancelled precisely because this project has one operator.

## Why the hold has not started

The ceremony cleared the attestation blocker, and one thing now stands between it
and a start: **free disk**. A fresh import replays from 8,665,600 to the network
tip, which was 8,825,301 on 2026-08-23 — roughly 160,000 blocks. A comparable
state directory (`witness-16e0928a/state`) measures 102 GB against 109 GB free on
a filesystem already at 95%, shared with Bitcoin Core and three running nodes. The
import reflinks the 143 GB MARF so most of that 102 GB is shared extents rather
than new blocks, but replay divergence consumes real space, and filling this
filesystem would take down the burnchain source and the live followers with it.
Freeing space means deleting another run's state, which is the operator's call,
not a step to take unattended.

The live follower is meanwhile stuck at 8,815,025 on the defect task 147 fixed,
10,276 blocks behind tip, and cannot be restarted onto the fix: `check_profile`
compares the state's recorded profile with the running binary's and there is
deliberately no subcommand to repin an imported state, so a compiler change means
re-import. That is the same fresh import above, not a separate blocker.

## Acceptance criteria

- One continuous 24-hour interval has no process failure or consensus
  difference.
- The node remains within the stated catch-up bound after each new block.
- Selected, followed, and executed tips do not hide persistent lag.
- Resource use has no unbounded trend.
- All served data was validated and executed locally.
