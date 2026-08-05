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
- [x] Maintain bounded outbound and inbound peer sets with connection limits,
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
- [x] Make peer disconnects, slow peers, duplicate inventory, invalid messages,
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

## The second slice: nano is a peer, and a node with no hosted API

The first slice could handshake. This one holds a peer set, answers peers that dial
it, discovers endpoints to fetch from, and is wired into the running node — so
`node.peers` is now optional and a mainnet node's way in is four public p2p
bootstrap addresses rather than somebody's API.

`crates/nano-p2p` is four modules: `wire` the codec, `session` one conversation,
`swarm` a bounded set of them with scoring and neighbour discovery, `inbound` the
reply side, `peers` the durable table.

### Blocks come over HTTP, and that is not a shortcut

Worth stating plainly because it shaped everything after it. In Nakamoto,
stacks-core downloads blocks and tenures over **HTTP** to each peer's own RPC
endpoint (`net/download/nakamoto/tenure_downloader.rs` builds
`StacksHttpRequest::new_get_nakamoto_tenure` against a `PeerHost`); there is *no
p2p message for requesting a block*. p2p carries the handshake, neighbours,
inventories, and pushed blocks and transactions.

So the product of discovery is [`Discovered::endpoints`] — the `data_url` of every
peer that handshook and advertises RPC. `https://api.mainnet.hiro.so` and
`http://54.91.222.127:20443` speak the same protocol; the difference that matters
is that the second was found by asking the network, is one of six, and is not a
service whose rate limit is nano's liveness. Building a p2p block-request message
nano's peers do not implement would have been inventing a protocol, not joining
one.

### What a stock node can now do with nano

Both directions, and each is gated by a test that has the reference implementation
on the other end.

**Nano → stacks-core.** `nano-p2p/tests/live_peer.rs`: all four stacks-core
mainnet seeds complete the handshake, answer the ping and hand over their
neighbour lists.

**stacks-core → nano.** `nano-conformance/tests/conformance/p2p_inbound.rs` puts
`stackslib` on the *dialling* end over a real socket: every message is built,
signed and serialised by the reference implementation, and every reply is
deserialised and authenticated by it. A handshake whose announced key
`verify_secp256k1` accepts, a ping, a neighbour walk, and an inventory exchange in
both directions — including a `Nack(NoSuchBurnchainBlock)` for a cycle nano does
not know, which a stock node reads as "ask somebody else" rather than "it has
nothing". What stands between that and a live stock node is the stock node's own
scheduler.

`nano-p2p/tests/loopback.rs` runs nano against nano for the conversation itself,
including two hostile peers: one announcing key A and signing with B, one answering
on another network. Both are isolated. A peer that merely hangs up is not, and
proving *that* took a test fix — aborting a listener left its already-accepted
conversations answering pings, so the peer the test had "removed" was still in the
swarm.

### Catch-up with the hosted API removed

`p2p_discovery.rs`, gated on `NANO_P2P_MAINNET`, is the acceptance criterion taken
literally. Against mainnet, from the four seeds and nothing else:

```
round 1: 4 connected, 4 dialled, 0 isolated, 52 addresses learned, 56 known
round 2: 8 connected, 4 dialled, 0 isolated,  2 addresses learned, 58 known
6 endpoints to fetch from: 3.122.176.89, 3.14.226.111, 3.231.161.121,
                           34.150.184.50, 52.77.118.154, 54.91.222.127  (all :20443)
chose peer 5 at http://54.91.222.127:20443/: stacks height 8708612, bitcoin height 961206
5 of 6 discovered peers answer HTTP
```

The gate asserts what the numbers are for: at least two sessions, a table that grew
past its seed list, no `hiro.so`, no private address, no duplicate, and a chosen tip
on mainnet weighed by the same `PeerPool` the production loop weighs. What is *not*
yet demonstrated is a full replay from the checkpoint driven only by these
endpoints; the transport and the selection are proven, the multi-thousand-block
catch-up over them is not, and that is the first thing to measure next.

### Two things only real peers could teach

**Mainnet advertises `http://10.0.1.37:20443`.** A load-balanced node naming the
address it sees itself at behind its own NAT — the same 10.0.1.37 that came back in
a nat-punch reply. Fetching from it means dialling this machine's own network. The
rule is a comparison rather than a ban: a private endpoint is accepted from a
private peer, because then we are on that network, and refused from a public one.
Hacknet keeps working with no configuration switch, which matters because
stacks-core's equivalent (`connection_opts.private_neighbors`) is a switch, and a
switch is a thing to get wrong.

**Two of eight peers advertised the same endpoint.** Left in, a pool of "eight" had
six distinct places to fetch from, and "no single peer is load bearing" would have
been counting one peer twice.

### Scoring: away versus wrong

The whole policy is one distinction. A peer that times out or hangs up has almost
certainly restarted, so its session is dropped and the table gives it a growing
backoff — 30 s doubling to an hour — and keeps it. A peer that sends a malformed
message, signs with a key other than the one it announced, contradicts itself about
its own Bitcoin view, floods instead of answering, or answers on another network is
*isolated*: the longest penalty the table can express.

Deliberately not a permanent ban. A malformed message is more often a version skew
than malice, and a node that bans permanently on protocol errors bans the network
one deployment at a time. What isolation buys is that a peer serving garbage stops
occupying one of eight session slots. `SessionError::is_protocol_fault` is where the
line lives, and no variant of `SessionError` stops the node.

### The node wiring

`node.peers` may be empty when `node.p2p_seeds` gives a way in; on mainnet the seeds
default to stacks-core's own published bootstrap nodes, because they are public and
a mainnet node with no way in does nothing. `p2p_seeds = []` is how a configuration
says "HTTP only" out loud. A configuration with neither is still refused.

`start_transport` opens the peer table under the working directory, seeds it, and
runs one round *synchronously* so a node with no configured peer has somewhere to
fetch from by the time it looks. It needs the chain identifier up front — on this
protocol the network id **is** the chain id, in the second field of the first
message — so a configuration that leaves the chain to be discovered from its peers
gets no transport and behaves exactly as before.

The identity is persisted as a seed under `p2p-seed`. Peers remember a node by its
key hash, and one that re-keyed every start would be a new stranger to the whole
network each time — including to the tables that had it on a backoff, which is the
half that would make restarting a way to launder a reputation.

The advertised Bitcoin view is derived from this node's own executed height and its
own Bitcoin source, never from what a peer said: a preamble view is a gossip hint
rather than a consensus input, but repeating a peer's claim back at the network is
how a hint becomes one. Before there is a chain to describe it advertises a
deliberately old view, which all four mainnet seeds accept — a peer keeps only ~288
blocks below its stable height, so an older claim is *uncontradictable* rather than
wrong, and stacks-core reads not-contradictable as merely stale.

`Job::Peers` is not fatal. Losing discovery leaves whatever the operator configured;
losing the listener only costs this node its place in other nodes' tables.

### Task 027's open half, closed

`choose_canonical_tip` existed and nothing called it. The follow loop now re-weighs
through `PeerPool::choose_source` over the configured and discovered endpoints
together — on a timer, and immediately after the current peer lets a round down.

Where no reward set is derivable yet it falls back to length, and that is a
*liveness* choice rather than a security one, said plainly in the doc comment
because the difference matters: a node with no reward set that refused to sync would
never acquire one. What keeps it safe is that selection is not the only check —
every block still has to pass `SignerSet::verify` at execution, so a peer offering
an unsigned chain wins one round and then fails to have a single block accepted.

### What is left

1. **Driving the fetches from the inventory.** `assign_tenures` is done and unit
   tested — only claiming peers are asked, work is spread round-robin, and the order
   is sorted by peer key hash so it does not depend on who replied first — and
   `Swarm::tenure_claims` collects the claims. Nothing calls them yet: `catch_up`
   walks backwards from one peer's tip rather than taking a tenure work list, and
   turning it into a per-tenure parallel fetch is the next real change.
2. **Relay.** `encode_frame` takes a relayer list for it (appending ourselves changes
   the frame, so a relayed message is re-encoded and re-signed rather than forwarded
   verbatim), and pushed blocks and transactions are already offered to the caller by
   both the swarm and the listener. Both are counted and dropped in `nano-node`,
   because acting on one means putting it through staging and the authenticated
   selection boundary.
3. **Serving inventories and blocks.** The listener answers `GetNakamotoInv` from a
   `Service` implementation that currently returns `None`, so nano nacks every cycle.
   Wiring it to the executed chain is what makes nano useful to *other* nodes, and it
   is also the "persist enough authenticated block data to answer peer requests"
   item.
4. **Bulk history through a local source.** Untouched. Header backfill and accounting
   reconstruction still go through one client, and now that the pool has six peers
   the fix is to spread them.
5. **Deterministic integration against a stock node**, including restart and reorg.
   `p2p_inbound.rs` is the closest thing so far, and it is the reference codec rather
   than a reference node.

### Not affected by the pox-4/pox-5 cycle-keying finding

Checked, because it was worth checking: nothing in this slice reads a PoX contract or
a reward cycle. `ChainView` is Bitcoin heights and header hashes only, and
`GetNakamotoInv`/`assign_tenures` take a cycle's *first sortition consensus hash*
from the caller and never compute which cycle that is. When the inventory driver
lands it will have to derive that hash, and that is where the cycle-keyed rule will
matter.

### Tried and reverted

- **`&Swarm` across an await.** `PeerDb` holds a `rusqlite::Connection`, which is
  `Send` but not `Sync`, so a shared borrow made every future non-`Send` and
  unspawnable. Everything that awaits takes `&mut self`, which is also the honest
  signature: each of those calls changes what this node knows.
- **A `dial` helper on `&mut self`** that clippy correctly said never mutated
  through the reference. Inlined into the dialling loop, which removed the awkward
  borrow rather than annotating it.
- **A reachability test that re-parsed the URL itself** instead of calling the
  function under test. It passed and proved nothing; the function is free-standing
  now and the test calls it.
- **`is_due` doubling once too early**, so a peer's *first* failure cost it 60 s
  instead of 30. Caught by its own test.
