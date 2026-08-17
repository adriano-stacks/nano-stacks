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
- seal durable state; and
- expose health and metrics on loopback.

It has no public HTTP surface. Inbound P2P serving and persistent native Wasm
modules remain out until measurements show that omitting either violates the
documented catch-up or liveness bound.

## Out-of-process capabilities

Mining, signing, proposal validation/hosting, StackerDB replication, mempool,
TUI, event streaming and compatibility RPC are separate products. They do not
link into `stacks-follower`, receive its state path, or open its databases. A
separately supervised adapter may consume a bounded read-only protocol and may
be stopped, compromised or restarted without acquiring chainstate write
authority.

The development `stacks-node` remains useful for conformance and Hacknet roles,
but it is not the mainnet follower release artifact.
