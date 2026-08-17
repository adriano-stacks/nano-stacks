# Follower artifact boundary

The first mainnet artifact is `stacks-follower`, built from the
`nano-follower` package. Its closed capability and dependency policy is
[`release/follower-policy.json`](../release/follower-policy.json). The policy is
release input, not an aspiration: dependency-tree, symbol, route and
configuration gates read it and fail when the artifact grows outside it.

## In-process capabilities

The follower owns one state directory and is the only process allowed to write
chainstate. It may:

- authenticate a checkpoint against pinned builders and local Bitcoin Core;
- derive the Bitcoin/sortition view locally;
- discover peers and acquire Stacks blocks over outbound P2P;
- authenticate blocks, select forks and execute Clarity through clarity-wasm;
- seal durable state and retain bounded block and receipt commitments; and
- expose health and metrics on loopback.

It has no public HTTP surface. Inbound P2P serving and persistent native Wasm
modules are omitted. The mandatory packaged lifecycle gate gives every
externally observed catch-up, fork-refusal, restart and recovery condition 60
seconds. With persistent modules enabled it completed in 39.53 seconds; the
cache-free, outbound-only package completed in 39.71 seconds, discovered its
HTTP block source over outbound P2P with no configured HTTP peer, and left no
native-module directory after any follower process exited. The exact artifact
sizes, hashes and decisions are recorded in
[`release/follower-liveness.json`](../release/follower-liveness.json). Neither
omitted capability has a measured liveness justification for inclusion.

The receipt commitment covers each transaction's identity, status, serialized
result and five cost dimensions plus every ordered event. It is kept only for
the same bounded recent-block window as the block archive and is removed with a
retracted or pruned block. It provides release evidence without adding an event
service or public route.

## Out-of-process capabilities

Mining, signing, proposal validation/hosting, StackerDB replication, mempool,
TUI, event streaming and compatibility RPC are separate products. They do not
link into `stacks-follower`, receive its state path, or open its databases. A
separately supervised adapter may consume a bounded read-only protocol and may
be stopped, compromised or restarted without acquiring chainstate write
authority.

The development `stacks-node` remains useful for conformance and Hacknet roles,
but it is not the mainnet follower release artifact.
