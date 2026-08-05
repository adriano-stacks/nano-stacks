---
id: "054"
title: "Join and synchronize over the Stacks P2P network"
status: in-progress
priority: critical
effort: large
type: feature
group: mainnet
dependencies: ["027"]
tags: ["mainnet", "p2p", "sync", "networking"]
created_at: 2026-08-02
---

# Join and synchronize over the Stacks P2P network

## Objective

The production runtime currently selects the first HTTP peer that answers and
uses its Stacks RPC for every block, tenure, sortition, mempool and signer data
request. Pointing that client at `api.mainnet.hiro.so` makes a hosted service's
rate limit, availability and view of the chain part of nano's liveness.

Join the canonical Stacks peer network directly. Nano must discover multiple
independent peers, exchange and serve protocol data, and feed locally validated
candidates into the fork-choice and execution paths from
[[027-choose-a-fork-instead-of-following-a-peer]]. HTTP may remain as an
operator-selected bootstrap or diagnostic source, but neither catch-up nor
steady-state operation may require a hosted Stacks API.

## Tasks

- [x] Reuse a vetted Stacks wire codec and message definitions where possible;
      document every protocol/version compatibility boundary that remains in
      nano.
- [x] Implement the mainnet handshake, framing, network and chain checks,
      liveness messages, neighbor discovery and persistent peer database.
- [ ] Maintain bounded outbound and inbound peer sets with connection limits,
      retry backoff, scoring and isolation of malformed or dishonest peers.
- [ ] Exchange inventories and acquire Nakamoto blocks, tenures and required
      sortition data from multiple peers without making any one peer a
      consensus input.
- [ ] Route bulk history consumers, including accounting reconstruction and
      missing historical-header acquisition, through a local or P2P-backed
      source so repair does not serialize thousands of requests through a
      hosted API rate limit.
- [ ] Persist enough authenticated canonical block data to answer peer inventory
      and block requests after a restart.
- [ ] Relay locally accepted transactions and blocks, and carry the signer and
      StackerDB messages required by the enabled node roles.
- [ ] Feed all received data through the local burnchain, signer, miner, VRF,
      transaction and state-root checks before fork choice or relay.
- [ ] Make peer disconnects, slow peers, duplicate inventory, invalid messages,
      ordinary forks and bounded network queues non-fatal and observable.
- [ ] Interoperate with stock `stacks-node` peers in deterministic integration
      tests, including restart, reorganization and one malicious peer.
- [ ] Document seed configuration, advertised/listen addresses, resource
      limits, peer database recovery and the optional HTTP fallback.

## Acceptance Criteria

- Starting from the attested checkpoint, nano catches up to and holds mainnet
  tip with all hosted Stacks HTTP APIs disabled.
- Removing, stalling or lying through one peer does not stall synchronization,
  select a different canonical chain or corrupt durable state.
- A stock `stacks-node` can complete the handshake with nano, exchange
  inventory and blocks in both directions, and receive a relayed transaction.
- Restarting preserves the validated chain and usable peer knowledge without
  redownloading sealed blocks.
- The P2P implementation has bounded memory, disk and connection use, rejects
  wrong-network and malformed messages, and passes `clippy` without warnings.
- The mainnet release gate records no dependency on Hiro or another hosted
  Stacks API for synchronization, propagation or consensus inputs.
- Rebuilding the maturity window and filling required historical headers
  completes with hosted HTTP APIs disabled.

## The first slice: nano talks to mainnet

`crates/nano-p2p` exists, and a real mainnet seed node completes a handshake with
it, answers its ping and hands it its neighbour list. That is the whole of items
one and two; the rest is still open, and the last section says where the next cut
starts.

Three modules, 2,159 lines: `wire` the message codec, `session` one authenticated
conversation, `peers` the durable peer table.

### The codec is nano's own, and stacks-core is its oracle

The first subtask asks to *reuse* a vetted codec, and the answer is that nano
cannot: [[062-keep-stacks-core-test-features-out-of-the-producti]] forbids
`stackslib`, `stacks-codec`, `libsigner` and `libstackerdb` in the release node's
normal dependency graph, and `release_dependencies` asserts it from `cargo tree`.
`nano-p2p`'s normal graph is `nano-chainstate`, `nano-codec`, `nano-crypto`,
`nano-primitives`, `rusqlite` and `tokio`, and nothing else.

What the rule does not forbid is using the reference implementation as an
*oracle*, which is exactly what `stackslib` being a dev-dependency of
`nano-conformance` is for. `tests/conformance/p2p_wire.rs` puts every message
through four directions: stacks-core encodes it and nano decodes it; nano
re-encodes the frame **from its decoded structure** and matches theirs byte for
byte; nano verifies the signature stacks-core made; nano signs the same payload
and stacks-core decodes it and verifies that. 256 proptest cases across thirteen
payload shapes, plus every transaction and block in `fixtures/nakamoto/blocks`.

The second direction is the one that matters and the one that is easy to fake. A
decoded `Message` keeps the payload frame it arrived as, so comparing *that*
against the peer's bytes proves nothing; `wire::encode_frame` re-encodes from the
structure, and mutating one field of nano's encoder (`services ^ 1`) does fail the
suite, which was checked rather than assumed.

### The compatibility boundaries that remain

Two deliberate departures from `stackslib/src/net/codec.rs`:

**A message keeps its frame, and is authenticated from it.** stacks-core verifies
a signature by re-serialising what it parsed. Nano hashes the bytes that arrived.
That makes authentication independent of nano's encoder — no encoder disagreement
can turn a valid signature invalid — and it is what makes the next point possible.

**The epoch-2.x messages are recognised and discarded.** `Blocks`, `Microblocks`,
`BlocksAvailable`, `MicroblocksAvailable` and the block and PoX inventories decode
to `Payload::Unhandled(id)`, which skips the body. A mainnet peer really does send
these unsolicited — the live test counted eight from one seed in a single
conversation — so failing to parse one would drop the connection, and modelling
one would be code for a chain a 4.0-only node does not follow. The same variant
holds the StackerDB replication messages (21..=25), which nano carries over HTTP
today. An identifier the protocol never assigned, 20 included, stays an error: a
peer sending one is not merely old. `encode_frame` refuses an `Unhandled`, so a
message nano did not model can never be relayed as something it is not.

### `NETWORK_ID_MAINNET` is a trap, and it cost the afternoon

The codec suite was green on the first run and all four mainnet seeds still
dropped every message without a reply — the same 0.35 s close as a deliberately
wrong network id and as 300 bytes of zeroes, while an incomplete message got the
5.4 s handshake timeout. So they were parsing a whole message and rejecting it.

Varying every handshake field changed nothing. Sending an unauthenticated `Ping`
also got silence, and that is what located it: an unauthenticated `Ping` is
answered with a `Nack`, and a `Nack` needs no valid signature, so being dropped
before that meant the rejection was in `is_preamble_valid` and not in the payload
or the signature at all.

`stacks-common` exports `NETWORK_ID_MAINNET = 0x17000000`. **Nothing in the p2p
path uses it.** `stacks-node` passes `config.burnchain.chain_id` as
`PeerDB::connect`'s `network_id` (`neon_node.rs:4713`), so the identifier in every
mainnet preamble is `1` — which `api.mainnet.hiro.so/v2/neighbors` prints as
`"network_id":1` for every peer, and which is what the field had to be. The p2p
network id *is* the chain id, so `Protocol::for_network` now takes it from
`nano_primitives::Network::chain_id()` and the constant is gone.

A differential codec test cannot find this: the value is policy, not encoding.
Only a real peer can, which is the argument for the live test below being in the
tree rather than being a thing somebody once ran.

### What a live peer confirms

`cargo test -p nano-p2p --test live_peer`, skipped unless `NANO_P2P_PEER` names
one. Against all four stacks-core mainnet seeds, with both a real Bitcoin view and
a stale one:

```
seed.mainnet.hiro.so:20444 at 34.150.184.50:20444: key 6d6ce48c…, services 0x0007,
  heartbeat 3600s, data url "http://34.150.184.50:20443"
  its Bitcoin view: tip 961198 (00000000000000000000d379…0684), stable 961191
  it knows 77 neighbors
  learned 77 new addresses from it
  1 unmodelled messages, 1 unsolicited
```

The peer's advertised tip hash matched the one fetched independently from an
explorer, which is the check that `BitcoinHeaderHash`'s byte order is the
displayed one on this path too.

**A stale view gets in, and that is a test affordance rather than a design.** A
peer refuses a message whose stable header hash contradicts its own, but it only
keeps about 288 blocks below its stable height, so a claim about an ancient height
is not checkable and stacks-core treats not-checkable as merely stale. Useful for
proving the protocol without a synced burnchain; wrong for a node, because a stale
view is one no peer will walk toward. `Session::advertise` takes the view from the
caller, and the caller in production is nano's own sortition database.

### The peer table

One sqlite table, against `PeerDB`'s ten. Two things it keeps apart: a key **hash**
from gossip is a third party's claim and is stored as a hint, while a key from a
handshake is proof and overwrites it — a `Neighbors` reply must not be able to
decide who nano thinks another peer is. And a failure is counted, never fatal: 30 s
doubling to an hour, and the peer stays in the table, because the reason is far
more often a restart than dishonesty. Candidate order is fewest failures, then most
recently seen, then untried, then address — determined entirely by what this node
observed, so no peer can promote itself by answering first, which is the same
reasoning as `PeerPool`'s tip ranking in
[[027-choose-a-fork-instead-of-following-a-peer]].

### Where this stops, and what the next slice is

Not started, in order:

1. **The inventory and download driver.** `Session::nakamoto_inventory` and the
   `NakamotoBlocks` codec exist and round-trip against stacks-core; nothing drives
   them. The next slice is a `GetNakamotoInv` walk across a reward cycle from
   *several* sessions at once, `NakamotoBlocks` fetches spread over the peers that
   claim a tenure, and the results handed to the same locally-authenticated
   selection boundary `nano-sync` already feeds — never to fork choice directly.
   `Payload::NakamotoBlocks` is deliberately request-shaped already: it decodes
   with a duplicate check, so a peer cannot buy thirty-two validations with one
   message.
2. **Bounded peer sets.** The table is bounded and backs off; there is no
   connection manager above it holding N outbound sessions, retiring the worst and
   isolating a peer that serves invalid data. `SessionError` is already entirely
   per-peer for this reason: no variant is a reason to stop syncing.
3. **The inbound listener**, which is what a stock node needs to handshake *with*
   nano, and which needs the reply side of `Handshake`, `Ping` and `GetNeighbors`
   — the codec is done, the handlers are not.
4. **Relay**, which is why `encode_frame` takes a relayer list: appending ourselves
   changes the frame, so a relayed message is re-encoded and re-signed rather than
   forwarded verbatim.

Not wired into `nano-node`. When it is: `ExecutingNode` would hold a
`PeerDb` beside its chainstate, seed it from `MAINNET_SEEDS` or configuration,
open `Session`s from `PeerDb::candidates`, advertise a `ChainView` built from the
sortition DB's Bitcoin tip and tip−7, and hand what it collects to `PeerPool`'s
candidate selection instead of the pool holding `SyncClient`s. `nano-p2p` also has
to join `PRODUCTION` in `release_dependencies.rs` at that point, since it will be
in the release closure.

### Tried and reverted

- **Depending on `stackslib`'s codec** was never attempted: `release_dependencies`
  makes it a compile-time no. Recorded here because the subtask asks for reuse and
  the answer is a rule, not a preference.
- **`Payload::Transaction` is boxed.** A transaction is by far the largest thing a
  message carries, and clippy's `large_enum_variant` was right — every other
  variant was being padded to 624 bytes.
- **The first `is_due` doubled once too early**, so a peer's *first* failure cost
  it 60 s instead of 30. Caught by its own test, which is the only reason it is not
  still there.
