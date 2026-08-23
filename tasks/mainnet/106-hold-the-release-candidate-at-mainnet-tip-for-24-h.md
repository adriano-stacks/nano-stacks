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
- [x] Import under the real 2-of-2 builder policy, not the functional one. The
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

## The release follower accepted the attestation, 2026-08-23

`/home/aldur/release-hold-ee7af998/` ran the re-attested bundle under the real
policy, with `peers = []` and no `p2p_seeds`, so every block, tenure and
sortition input arrives from peers discovered over the binary P2P transport and
the only configured HTTP is this operator's own Bitcoin Core on loopback. It read
the whole 359 GB payload at ~575 MB/s before accepting anything, and logged:

```text
checkpoint bundle 943ecf7b…0580e authenticated by aldur-host-primary, aldur-host-recovery
checkpoint a87338900f…e932d attested by 2708 of 2599 signer weight
```

So the re-issued attestation is not merely self-consistent: a production node
authenticates it under the unchanged 2-of-2 policy and takes the state.

**It was then stopped, at about 6% of the space it needed.** The import has to
build its own `marf.sqlite` and `clarity.sqlite`; it does not reflink them from
the bundle. Measured while it ran: 6.4 GB written in 1 h 45 m, a steady 3.2 GB of
real disk per hour, against 104 GiB free on a filesystem at 95% shared with
Bitcoin Core and three writing nodes. The finished size is not a guess —
`witness-16e0928a/state/chainstate` is `marf.sqlite` 65 GB plus `clarity.sqlite`
35 GB, so a fresh import needs about **100 GB**. It was stopped with SIGTERM,
which it honoured in five seconds, leaving
`chainstate/checkpoint-import-unfinished.toml`; the partial state was removed and
its provenance record kept as `accepted-attestation-provenance.toml`.

**A reflink measurement was misread first, and this corrects it.** `btrfs
filesystem du` reports 809 MiB *exclusive* for the witness state, which is true
and does not mean the state was cheap to create: those blocks are shared with the
57 `task146-*` diagnostic clones, so exclusivity measures how many copies exist,
not what the first one cost. The consequence for reclaiming space is the opposite
of intuition — deleting any one clone frees almost nothing, and only removing
every reference to a shared set returns its ~100 GB. The ceremony's two 359 GB
workspaces are genuinely near-free at 11.92 MiB exclusive, because nothing
rewrites the payload.

**The space was then found without touching anyone's evidence, and the import is
running.** Task 146's 57 `task146-*` diagnostic clones were this session's own
scratch for a closed task; retiring all of them, two of which were registered
worktrees needing `git worktree remove`, took free space from 109 GiB to
**141 GiB**. That is the whole shared family, which is why it freed 27 GiB where
deleting any single clone would have freed megabytes.

The follower was restarted at 2026-08-23 11:59 UTC and authenticated the bundle
again under both builders. It runs behind `release-hold-ee7af998/disk-guard.sh`,
a watchdog that samples free space every minute, records a chainstate-growth line
every ten, and sends SIGTERM if free space falls below a 20 GiB floor — the node
honoured SIGTERM in five seconds when stopped by hand, leaving a resumable
unfinished-import marker rather than a torn state. So the import can run
unattended for two days without the possibility of starving Bitcoin Core or the
three other writing nodes.

**The receipt criterion needed an observer before the hold, not after it.** This
task requires comparing each executed state root *and receipt set* with the
oracle, and receipts only exist where an event observer records them. The
follower had started with `event_observers = []`, which would have produced a
rootful but receiptless hold and forced the whole interval to be repeated. A
third sink (`nano-release-sink`, the same `hacknet/event-sink.py` the hold and
witness sinks run, on `127.0.0.1:20472` writing to
`/home/aldur/release-hold-receipts`) was started and the follower restarted onto
it twenty minutes into the import rather than two days into it.

**That restart also established that an import cannot be interrupted at all**,
which is worth more than the config change that provoked it. The node refused to
start and said why:

> the checkpoint import … did not finish … Journalling is off during an import,
> so what it left cannot be rolled back and cannot be told apart from a complete
> state by reading it — the trie is missing nodes, and every state root computed
> on it would be wrong. Remove … and start again; an import is not resumed, and a
> mainnet checkpoint takes about four and a half hours.

So the `checkpoint-import-unfinished.toml` marker is a refusal, not a resume
point, and the earlier note that the attestation was "reauthenticated from
persisted provenance" on restart describes a start that then failed — the
reauthentication line is real, the successful restart it implied was not. The
partial state was removed and the import restarted once, with the observer
already configured so nothing needs to interrupt it again. It also corrects the
import estimate: **about four and a half hours**, not the thirty-one implied by
extrapolating its first hour's 3.2 GB/h.

Catch-up is measurable rather than open: the witness went 8,665,600 to 8,815,849
between 2026-08-21 16:21 and 2026-08-23 11:00, 150,249 blocks in about 43 hours
or ~3,500 an hour, so roughly two days to a tip of 8,825,301, and the 24-hour
hold follows it.

## The hold runs itself, 2026-08-23

`release-hold-ee7af998/hold-sampler.sh` is armed and waiting. It polls until the
node reports `blocks_behind <= 3`, then samples once a minute for 24 hours into
`hold-samples.jsonl` and writes `hold-summary.json`.

It measures rather than reimplements: **every sample this task asks for is already
served by the node at `/nano/sync_status`** — the three tips it insists on
distinguishing (`followed_stacks_height`, `selected_stacks_height`,
`executed_stacks_height`), `p2p_sessions` and `p2p_known_peers`, the four queue
depths, `staged_blocks`, per-observer `undelivered`/`queued_bytes`/`oldest_age_ms`
for the backlog, `executed_state_index_root` and `blocks_behind`. The sampler adds
only what the node cannot know about itself: the Bitcoin tip, its own RSS and open
file count, free disk, chainstate size, captured `new_block` count and running
totals of the peer-loss, refusal, execution-failure and fork lines in its log.

Continuity is enforced rather than assumed: the sampler records the follower's
pid with every sample, and if it changes or disappears the interval is declared
void and the 24 hours restart, which is what "restart the complete measurement
after a node defect or process stop" requires. `hold-window.log` keeps every
start and restart, and the summary reports `continuous_pid` and the largest gap
between consecutive samples, so a stall cannot be mistaken for a clean run.

The summary's two jq programs were exercised against synthetic samples before
being trusted with a 24-hour run; doing so caught a real bug, an `all(.[]; .pid
== .[0].pid)` whose `.[0]` bound to the element rather than the array.

What this harness does **not** do is the per-block root and receipt comparison
against the oracle. It counts the receipts captured so the input exists, but the
comparison itself belongs to the conformance tooling task 146 built, and it is
still an open item above.

## The running follower is diagnostic, not this task's evidence

Written down because it would otherwise look like the hold is under way.

**The subject of this hold is the packaged follower artifact, not the full node.**
`scripts/hold-follower-mainnet.sh` — the committed, already-exercised harness —
drives the artifact's loopback `/health` and `/metrics`, and `/health` is served
only by `nano-follower` (`observation.rs::LOOPBACK_ROUTES`), never by
`stacks-node`. Task 142 says the mainnet-ready label applies "only to the minimal
follower artifact" and that the release report runs "against the signed artifact";
this task requires the same release binary that tasks 037 and 054 qualified; and
the 2026-08-19 hold ran that artifact. So the release evidence must come from the
reproducibly built, signed `stacks-follower`, driven by that harness with
`scripts/verify-hold-receipts.sh` afterwards — not from an ad-hoc
`target/release/stacks-node` copy and not from the sampler above, which duplicates
a subset of a vetted harness because it was written before that harness was found.

**Building the right subject is blocked on a clean checkout.**
`scripts/reproducible-release.sh` exits unless `git status` is completely empty,
and the tree carries another session's in-progress task-147 files. That is a real
blocker, not an inconvenience to work around by stashing someone else's work.

**The full-node run was left going anyway, for two things it does prove.** It
rehearses the import path end to end from the re-attested bundle, and its
catch-up passes through 8,815,026 — the block the live follower is stuck on and
the defect task 147 fixed — so it will show that fix executing against real
mainnet state. Both are diagnostic value; neither is this task's evidence, and the
`hold-samples.jsonl` it produces must not be presented as the 24-hour hold.

The live follower is separately stuck at 8,815,025 on the defect task 147 fixed,
10,276 blocks behind tip, and cannot be restarted onto the fix: `check_profile`
compares the state's recorded profile with the running binary's and there is
deliberately no subcommand to repin an imported state. This import is that
restart, not a separate blocker.

## Acceptance criteria

- One continuous 24-hour interval has no process failure or consensus
  difference.
- The node remains within the stated catch-up bound after each new block.
- Selected, followed, and executed tips do not hide persistent lag.
- Resource use has no unbounded trend.
- All served data was validated and executed locally.
