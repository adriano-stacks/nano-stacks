# The Epoch 4.0 consensus firewall

Architecture decision for task 140: isolate deterministic Epoch 4.0 decisions
and chainstate write authority from network, RPC and optional-role failures.
This document records where the boundary is cut, what crosses it, what fails
how, and how the qualified in-process follower migrates onto it. It is a
follow-on architecture change to the qualified follower, not permission to
rewrite the qualified consensus rules in place.

## Where the boundary is cut, and why there

**Between authentication-context assembly and block application: the request
is an `AuthenticatedBlock`, the response is a decision record derived from
`AppliedBlock`.** Concretely, the boundary sits immediately above
`ChainState::commit_authenticated_nakamoto_block`
(`crates/nano-chainstate/src/lib.rs`), which is already the last call before
`Vm::commit_block` makes the MARF seal the durable linearization point.

The crate graph is already cut there. Everything below the line —
`nano-chainstate`, `nano-vm`, `nano-marf`, `nano-sortition`, `nano-bitcoin`'s
types — has no Tokio, HTTP or socket dependency. Everything impure lives above
it in `nano-follower`: burn-view resolution, peer walks, staging fetches, the
sortition tracker's Bitcoin reads and `check_burnchain`. The firewall makes the
existing crate boundary a process boundary rather than inventing a new one.

The request envelope also already exists and is already persisted:
`AuthenticatedBlock { block, bitcoin_context, operations, parent }` is exactly
what `Staging::put` writes to `staging.sqlite`. The response is already
serialized once, by `nano_rpc::events::new_block_payload`, and already reduced
to a bounded commitment by `nano_conformance::receipt_digest` — the same
digest the 24-hour hold compares across independently executing nodes, which
is what makes shadow mode cheap to assert.

Cuts deliberately rejected:

- **Below `nano-vm`** would split the MARF from the ledger, which
  `prepare_commit` deliberately writes in one `clarity.sqlite` transaction so
  that a crash leaves the parent-or-child shape recovery expects.
- **Above `catch_up`** would drag Tokio, staging, peer pools and the sortition
  tracker into the authoritative process — the exact surface the firewall
  exists to keep out.

## The two processes

**`epoch4-executor`** (authoritative): the sole owner of the state directory
and the only process with a writable chainstate handle. It hosts the
`epoch4-consensus` API: accept a typed request over a versioned,
length-prefixed, bounded local pipe (stdin/stdout of a supervised child, the
same shape `nano-conformance`'s `replay-blocks` already proves under SIGKILL
and fault injection); execute; append the decision record; answer. It has no
network listener and no client capability: no Tokio, no HTTP, no sockets, no
wall-clock reads in the decision path, no environment reads, no filesystem
access outside the state directory.

**The edge** (non-authoritative): P2P discovery, block acquisition, burn-view
assembly, staging, health/metrics, and every optional role. It holds only the
existing read-only capability the store level already provides
(`Access::ReadOnly`: `immutable=1`, no `-shm`, no module cache) plus the pipe.
Its crash, compromise or restart can produce at worst a refused request —
never a mutation of committed state.

## Deterministic inputs

A request carries everything the decision needs, so the answer is a pure
function of request bytes plus the state directory:

| Input | Source today | Change required |
|---|---|---|
| `NakamotoBlock` | consensus codec (`encode`/`decode`) | none |
| `BitcoinBlockContext` | flat scalars, no codec | versioned encoding that preserves the `height`/`tenure_bitcoin_height` invariant enforced by its setters |
| `Vec<BitcoinOperation>` | decode-only from Bitcoin txs | add an encoder |
| `parent: [u8; 32]` | trivial | none |
| burn header hashes | `seed_burn_headers` reads Bitcoin **and writes chainstate mid-path** | carried in the request; the executor is the only writer |
| PoX constants | peer `/v2/pox` via `PoxInfo` | moved into the consensus profile / configuration; an untrusted parent must not choose activation heights |

Two hygiene moves accompany the cut, both found by the survey rather than
assumed: `CheckpointExecutor::authenticate` currently calls
`bitcoin.block_at()` inline (only the operations-supplied variant survives on
the executor side), and the decision path reads `NANO_TRACE_*` environment
switches per write — those hoist to construction-time configuration, and the
`NANO_DUMP_WASM*` arbitrary-path file writes are removed outright as the only
decision-path filesystem access outside the state directory.

## The canonical decision record

One record per candidate, appended before the answer is sent:

- request identity: block id, parent, burn view, request hash;
- verdict: accepted, or a **typed** refusal (today's `ChainStateError`
  `Display` strings become an enum with stable discriminants, so two
  implementations can be compared refusal-for-refusal);
- state root sealed, execution cost, receipt commitment
  (`receipt_digest`-compatible), event count, native effect summary,
  matured-reward summary, derived reward set when one was computed;
- compiler identity and compatibility fingerprint.

The record is content-addressed and is the only committed-block visibility
point for the edge. Task 141 unifies durable sealing behind it; until then it
is written beside the existing seal in the same process that seals.

## Failure semantics

- A malformed, out-of-order or wrong-parent request is a typed refusal; the
  executor's `parent == execution_tip` precondition already exists and stays.
- A killed executor leaves the parent-or-child shape the existing
  `kill_during_replay`/`storage_faults` harnesses prove; the supervisor
  restarts it and the edge replays from staging, which is already the restart
  path today.
- A killed or wedged edge stops feeding requests; the executor idles. Nothing
  the edge can do — including saturating the pipe, which is length-prefixed
  and bounded like every other ingress — changes the next accepted decision.
- Oracle/observability consumers read decision records, never internal state.

## Migration plan

1. **Extract types** (`epoch4-consensus` crate): request/response/record
   codecs, typed refusals, the input changes in the table above. No behavior
   change; the in-process follower adopts the types first.
2. **Shadow mode**: the edge drives both the in-process path and a supervised
   executor child on a copy-on-write state, comparing every decision record —
   verdict, root, receipt commitment, cost — over the full offline corpus
   (the captured chain crosses five reward-cycle boundaries) and then live at
   tip across a full reward cycle. Any difference is a release blocker.
3. **Authority switch**: the executor child becomes the sole writer; the
   in-process path is removed, not gated. No fallback remains, matching the
   engine rule: a fallback can hide exactly the divergence the boundary
   exists to surface.
4. **Measure**: catch-up throughput, tip latency and restart time against the
   qualified follower's recorded bounds; the pipe adds one serialization per
   block, which the receipts side already pays for observers today.

## What this depends on

Tasks 135 (minimal follower — done: the packaging boundary this hardens),
136 (executable profile — done: where the peer-derived PoX constants move),
and 138 (full-cycle qualification) gate the authority switch: the shadow
comparison baseline must be the qualified artifact, and nothing here rewrites
consensus rules ahead of it.
