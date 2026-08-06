---
id: "054"
title: "Join and synchronize over the Stacks P2P network"
status: completed
priority: critical
effort: large
type: feature
group: mainnet
dependencies: ["027"]
tags: ["mainnet", "p2p", "sync", "networking"]
created_at: 2026-08-02
completed_at: 2026-08-06
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
- [x] Exchange inventories and acquire Nakamoto blocks, tenures and required
      sortition data from multiple peers without making any one peer a
      consensus input.
- [x] Route bulk history consumers, including accounting reconstruction and
      missing historical-header acquisition, through a local or P2P-backed
      source so repair does not serialize thousands of requests through a
      hosted API rate limit.
- [x] Persist enough authenticated canonical block data to answer peer inventory
      and block requests after a restart.
- [x] Relay locally accepted transactions and blocks, and carry the signer and
      StackerDB messages required by the enabled node roles.
- [x] Feed all received data through the local burnchain, signer, miner, VRF,
      transaction and state-root checks before fork choice or relay.
- [x] Make peer disconnects, slow peers, duplicate inventory, invalid messages,
      ordinary forks and bounded network queues non-fatal and observable.
- [x] Interoperate with stock `stacks-node` peers in deterministic integration
      tests, including restart, reorganization and one malicious peer.
- [x] Document seed configuration, advertised/listen addresses, resource
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

## The third slice: peers were being isolated for working

Two items were named for this slice and both landed, but the thing worth reading
first is the bug the *running node* reported and the code could not have.

### 4 of 7 peers isolated, and none of them had done anything

The mainnet node's own log:

```
p2p: 7 connected (4 new, 0 lost, 1 isolated), 3 addresses learned
p2p: 115 unsolicited messages dropped
p2p: 3 connected (0 new, 0 lost, 4 isolated), 0 addresses learned
```

`tests/live_unsolicited.rs` was written to answer it, and it answers it flatly.
Across three stacks-core mainnet seeds, over several minutes each, **every single
unprompted message was an announcement**:

| seed | window | `StackerDBPushChunk` (25) | `Transaction` (13) | `NakamotoBlocks` (28) | requests |
|---|---|---|---|---|---|
| `seed.mainnet.hiro.so` | 60 s | 43 | 3 | 3 | **0** |
| `seed.mainnet.hiro.so` | 90 s | 67 | 0 | 2 | **0** |
| `cet.stacksnodes.org` | 45 s | 14 | 0 | 0 | **0** |
| `sgt.stacksnodes.org` | 45 s | 8 | 1 | 0 | **0** |

Identifier 25 is `StackerDBPushChunk` — mainnet's signer traffic, which is most of
what a mainnet peer says — and the aggregate rate is 0.2 to 0.8 messages a second
per peer. The message identifier is the whole of the finding, which is why the
census prints it: every unmodelled message shares the name `Unhandled`, and
identifier 25 (an announcement, safe to drop) reads exactly like identifier 5
(`GetBlocksInv`, a peer blocked on an answer) until you look.

So the peers were behaving perfectly. A session refused more than
`MAX_UNSOLICITED_PER_REQUEST = 32` interleaved messages before a reply and raised
`TooChatty`, which `is_protocol_fault` counted as misbehaviour and the swarm turned
into the peer table's longest penalty. The swarm reads a session once every fifty
seconds — `poll_interval_secs * 10` — so **any rate at all crosses a fixed count
given enough silence**: the hiro seed queued 59 messages in one gap. And because
the busiest peer crosses it first, nano was isolating the most useful half of the
network, worst first. That is the opposite of what scoring is for.

Two fixes, and they are different mistakes.

**Volume is not a fault.** A request is bounded by one overall deadline now instead
of a message count, so a peer that lets the deadline pass comes back as a `Timeout`
— a backoff, not an isolation, because a slow peer is nearly always a busy one.
`SessionError::TooChatty` is gone. The push buffer is bounded at 256 and sheds the
oldest, counted, because a dropped signer chunk costs timeliness and another peer
will push it again, while a queue that grows with a peer's output is memory the
peer chose.

**The session is read every round, not only when it wants something.** That is what
kept fifty seconds of relay traffic out of the kernel buffer in the first place, and
it is also the thing that makes relay possible at all. `Framed::drain` uses
`try_read`, so "the peer has nothing queued" and "the peer is slow" cannot be
confused.

Framing had to move onto an internal buffer for that, and the reason is worth
writing down: **`read_exact` is not cancel-safe.** A deadline expiring part-way
through a message consumes bytes and loses them, leaving the stream permanently out
of frame. That is survivable when the only response to a timeout is to throw the
session away, and fatal for a drain whose entire purpose is to stop and carry on
using the connection.

### The other half was a handler gap after all

The census found no requests, but it could not have: stacks-core advertises a
3600-second heartbeat, and a 60-second window sees none of it. A stock node does
send `Ping`, `Handshake`, `GetNeighbors` and `NatPunchRequest` on any conversation
regardless of who dialled — the protocol is symmetric once handshook — and nano was
reading those, counting them as "unsolicited" and never replying.

So an unsolicited message is now *answered* when the peer is waiting on one:
`Ping`, `NatPunchRequest` and `Handshake` from memory, and `GetNeighbors` and
`GetNakamotoInv` through the same `Service` the listener answers with, shared rather
than reimplemented. A mid-session `Handshake` updates the key the session
authenticates against — the peer proves possession by signing with it, and refusing
would isolate an honest peer for rotating a key — but deliberately **not**
`Session::remote`, so a peer cannot redirect nano's fetches mid-conversation by
advertising a new endpoint.

`tests/live_unsolicited.rs` now asserts the fix rather than only measuring: a
session that has read a mainnet peer across a full fifty-second gap must still be
usable, and must still answer `GetNeighbors` afterwards. `loopback.rs` gates both
halves offline — a peer that pushes 300 messages before answering is not isolated,
and a peer nano dialled gets its `Ping`, `GetNeighbors` and `Handshake` answered.

### Two more bugs that only fell out of testing the first

**`Swarm::retire` penalised into a `Round` it constructed and threw away.** A round
that lost every peer during an inventory exchange reported three connected and one
dropped. The round is the caller's now, and a dropped peer says why it went. This is
the one that made a genuinely failing test look like it passed.

**An inbound conversation was closed at its *read* deadline.** So nano hung up on
any stock node that had nothing to say for thirty seconds, which — against a
3600-second heartbeat — is most of the time. `InboundLimits` now separates how long
one read may take from how long a conversation may be silent (15 minutes, bounded so
a silently dead socket does not hold a slot forever). A node that drops its inbound
peers twice a minute is not a peer anyone keeps, and nothing offline would have
found it, because every offline test finishes inside the timeout.

### Bulk history: measured, not claimed

`catch_up`'s descent asked *one* client for every tenure. From the mainnet
checkpoint that is tens of thousands of blocks down one connection, so a hosted
API's rate limit **was** the catch-up speed — which is what joining the peer network
was for, and which stayed true for two slices after the transport landed because
nothing changed who the descent asked.

`nano_sync::TenureSource` is the work list. Consecutive tenures go to different
peers; a throttled peer is set aside for the round and its tenure asked of somebody
else; a peer that cannot serve one tenure costs a request rather than the descent.
Only when *every* peer has throttled does the round report itself rate limited, and
by then waiting genuinely is the only option.

`p2p_discovery.rs::bulk_history_comes_from_several_mainnet_peers`, against real
mainnet with no hosted API anywhere:

```
4 endpoints to fetch from: 3.122.176.89, 34.150.184.50, 52.77.118.154, 54.91.222.127
10 tenures, 414 blocks, over 4 peers
4 distinct peers served history
```

`served_by()` exists for that last line: a descent that reports one peer has not
been spread, whatever the pool holds. And the discovery gate itself now reaches
further than it did — eight endpoints rather than six, none isolated, twenty
unprompted messages collected in a round instead of counted and dropped.

### Driving from the inventory, and what an inventory is actually worth here

`Swarm::exchange_inventories` asks every peer about the cycle being walked and
publishes the endpoints of those claiming any of it; the descent puts those first.
A peer claiming none of the cycle has nothing to serve, and asking it for a tenure
to find that out is exactly the round trip an inventory exists to avoid.

**`assign_tenures` is still uncalled, and now there is a reason rather than a
todo.** It schedules a *forward* download driven by bit indices — given a set of
wanted tenures, spread them over the peers that claim them. Nano's downloader walks
parent links backwards from a tip, so it always knows the single tenure it wants
next and there is no set to spread. Making `assign_tenures` the scheduler means
rewriting the downloader to be inventory-driven and forward, which is a larger
change than this slice and should be its own. The shortlist is the part of an
inventory that applies to the downloader that exists.

### The cycle-keyed rule, applied

Naming a cycle needs the consensus hash of its *first sortition*, and it is derived
locally: `SortitionTracker::consensus_hash_at` indexes the history the tracker
already keeps — one hash per burn block, ending at the tip, because
`ConsensusHash::from_ops` mixes the hashes behind it and none may be skipped — so a
height maps to an index by subtraction. On the running mainnet node that history is
**294,442 entries**, which is every burn block since the chain began.

The boundary comes from `RewardCycleSchedule::starts_at`, which is waterfall-aware:
offset 0 once the waterfall is on, offset 1 before it. A node that decided from
where its tip happened to sit would move the boundary part-way through a prepare
phase and name a cycle no peer recognises. And it is local on purpose — a cycle
identifier taken from a peer would make that peer's view of the burnchain the thing
nano's own requests are keyed on.

### Serving inventories, and the honest bound on it

`Service::tenure_inventory` returned `None`, so nano nacked every cycle and no stock
node could sync *from* it. It now answers from the two local sources above: the
derived history places the cycle, the executed ledger says which of its tenures nano
ran and will serve.

A bit is set only where both hold, so the vector says **less** than the node knows
rather than more. That direction is chosen: an unset bit means "do not ask me" and
costs a peer nothing, while a set bit nano could not honour costs that peer a failed
fetch.

It says much less. `ChainLedger`'s executed suffix reaches `REORG_REACH = 256`
blocks back, so the honest answer covers the recent end of a 2,100-block cycle and
nothing older. A complete answer needs a durable consensus-hash-to-tenure index —
`block_header` in the side store is keyed by block id, so there is no way to ask
"did I run the tenure at this consensus hash" without a scan — and that is a
chainstate change, not this one. Partial and truthful still beats the nack it
replaces, because a nack tells a peer to give up on nano for the whole cycle instead
of fetching the tenures nano has.

It is answered from the snapshot the follow loop publishes, never by asking the
executor: `Service` is synchronous by design, and a reply that could take the
executor's lock is a reply that lets one inbound peer stall the loop that executes
blocks.

### The node was advertising a stale view for the life of the process

Found while wiring the above, and it had been true since the transport landed.
`start_transport` runs *before* the chainstate exists — that is its whole point,
since it is the way in that does not need a configured HTTP peer — so it was passed
`None` for the executor and `advertised_view` returned its deliberately-old fallback
every round, forever. A stale view is one no peer will walk toward.

`Advertised` is the handle the follow loop writes each round and the peer-facing
loops read: the Bitcoin height this node executed under, the cycle to ask
inventories about, and the inventory to serve. All three are nano's own answers,
because repeating a peer's claim back at the network is how a gossip hint becomes a
consensus input.

### What is left

1. **Relay.** Still counted and dropped. `encode_frame` takes a relayer list for it,
   and pushed blocks and transactions are now *collected* every round rather than
   left in the socket — which was the prerequisite — but acting on one means putting
   it through staging and the authenticated selection boundary, and doing it from the
   discovery loop would be the one place in the crate that trusted a peer.
2. **A complete served inventory**, which needs the consensus-hash-to-tenure index
   above.
3. **An inventory-driven forward downloader**, which is what would make
   `assign_tenures` the scheduler.
4. **Deterministic integration against a stock node**, including restart and reorg.
   `p2p_inbound.rs` is still the reference *codec* on the other end rather than a
   reference node.
5. **Restart-durable inventory**: the served vector is derived per round from the
   executed ledger, so it is correct after a restart but no larger.

### Tried and reverted

- **A drain that used `readable()` with a zero timeout** to check whether bytes were
  waiting. Readiness can be reported spuriously, and a spurious wake followed by a
  `read_exact` is a blocking wait inside a call whose whole contract is not to wait.
  `try_read` into an owned buffer has no such edge.
- **A 16 KB stack array per read.** Clippy's `large_futures` measured sixteen
  kilobytes per session future, which is real memory once eight of them nest inside a
  swarm round. Reads land straight in the buffer instead, which also removes a copy.
- **Keeping `TooChatty` but raising the cap.** Any fixed count is crossed by any rate
  given enough silence, so a larger number would have moved the bug rather than fixed
  it. The bound that means something is time.
- **Calling `prefer` every round.** It resets the round-robin cursor and the learned
  throttles, so applying it unconditionally left the descent asking the same first
  peer forever. It runs when the shortlist *changes*.

## The fourth slice: a pushed block goes where a followed one goes

Every remaining item landed. The one that gates the rest is relay, and the reason it
could not be done in the previous three slices is that acting on a pushed block means
having something to put it through — and the boundary
[[050-authenticate-every-followed-nakamoto-block]] built did not exist when the
transport did.

### Relay, and why it lives in the follow loop

`check_relayed` runs where the chainstate is, which is the whole design. Every push
goes through `ChainState::authenticate_block`, and it is the **same call**
`/v3/blocks/upload` goes through — not a second implementation of the same rules,
because a node that admits from a peer what it would refuse from its own API is
forkable through whichever of the two is laxer. What passes is staged, and from that
point it is indistinguishable from a block nano fetched itself: the same `Staging`
store, the same executor, the same state root check.

`nano-p2p` still decides nothing. `relay::Relay` is two bounded queues — pushes in,
acceptances out — and the crate's contribution is the *bound*, not a verdict. A push
lands there with no more claim attached than "this peer said so", and the peer's key
hash is carried only so the item is not sent back to the peer that sent it.

**A push now reaches the same place from both directions, and it used to not.** The
listener called `Service::offer_*`; the swarm buffered into `take_pushed` and the node
counted it. On a node behind a NAT most connections are the ones nano opened, so most
relay traffic was on the half that only got counted. `Framed::keep_push` hands to the
service when there is one.

**A rejected push is not a penalty.** A block can fail because *this* node cannot yet
derive the cycle's reward set, or has not executed the tenure it builds on. Scoring a
peer for that would repeat the third slice's bug exactly — nano isolating the peers
that were working hardest — so a rejection costs one authentication and a log line.

Relayed transactions are admitted on nano's own rules against nano's own executed
accounts rather than the sending peer's answer about them, and the mempool and executor
locks are taken in the order `/v2/transactions` takes them, because two loops taking
the same pair in opposite orders is a deadlock waiting for load.

**What nano puts on the wire.** A relayed message is re-encoded and re-signed, because
the relayer list is inside the frame the signature covers. Nano names itself and nothing
else there: an upstream list is a stranger's claim about which other nodes have seen the
item, nano cannot check any of it, and republishing it signed by nano would be passing
it on as ours. `relay::relayed_by` closes the loop — an item already naming this node has
come back round and is dropped rather than re-checked.

**Signer and StackerDB messages are deliberately not carried over p2p.** Identifiers
21..=25 stay `Unhandled`. Nano replicates StackerDB over `GET`/`POST /v2/stackerdb/...`,
which is the same replication by the same rules over the transport nano's block fetching
already uses; a second path would be two implementations of one thing.

### What was being dropped, measured on the running node

The mainnet node's own log, on the code before this slice:

```
p2p: 55 messages peers sent unprompted, 55 of them pushed data
p2p: 39 messages peers sent unprompted, 39 of them pushed data
p2p: 103 messages peers sent unprompted, 103 of them pushed data
p2p: 8 connected (1 new, 1 lost, 0 isolated), 0 addresses learned, 4 claiming this cycle
p2p: 189 messages peers sent unprompted, 189 of them pushed data
```

39 to 189 pushed items per round, all discarded, across eight peers with none isolated
— so the transport and the scoring were doing their jobs and everything they collected
was being thrown away. That is the volume the boundary now sees. It also sizes the
queue: 1024 is between five and twenty-five rounds of mainnet's output at the old
fifty-second interval, and on the new one-second-per-tick clock the same traffic
arrives in tenths.

### The discovery loop got a second clock

Relay shares the peer-facing task, because both need `&mut Swarm` and a swarm holds a
`rusqlite::Connection` that is `Send` but not `Sync`. So the loop ticks on the node's own
`poll_interval_secs` and walks neighbours every tenth tick. Two things wanted the shorter
clock: relay, which is not relay if it is minutes late, and reading each peer's socket,
which is what keeps a mainnet peer's 0.2–0.8 messages a second out of the receive buffer.

### The served inventory, and the reorg the test found

`tenure_inventory` was truthful-but-partial because `ChainLedger`'s executed suffix
reaches `REORG_REACH = 256` blocks back, so a 2,100-tenure cycle was answered at its
recent end and nowhere else, and a restart made it no larger.

`nano_p2p::ServedTenures` closes it by accumulating: each round's window is folded into
one row per cycle, unioned rather than replaced, because the window slides forward and a
replacement would make nano forget a tenure it really did run. It is deliberately **not**
a chainstate change — nothing in it is read by execution, nothing in it can change what
nano accepts, and a file whose worst failure is a peer asking somebody else does not
belong in the store whose worst failure is a fork. So the consensus-hash-to-tenure index
the third slice asked for is still not needed.

Writing the reorganization test found a real hole. A row was keyed by the consensus hash
naming the cycle, and a reorg across the cycle boundary *renames* the cycle — so the old
row would have survived forever and nano would have gone on telling peers it had tenures
on a fork it had abandoned. Rows are keyed by the cycle's **first burn height** now: a
reorg replaces the row and the abandoned claims go with it, and asking for the old name
is nacked, which is the honest answer because nano no longer knows that cycle.

### Restart and reorganization, with the reference implementation reading the answers

`p2p_relay.rs`, four tests, all offline and deterministic:

| Test | What is on the other end |
|---|---|
| a block stacks-core pushes reaches the authenticated boundary | `stackslib` signs and serialises `NakamotoBlocks` over a real socket |
| a block already accepted is not offered again | same, twice |
| a restarted nano still answers the inventory it had | `NakamotoInvData::has_ith_tenure`, after the process that learned it is gone |
| a reorganized nano stops claiming the fork it left | the same, plus a `Nack(NoSuchBurnchainBlock)` for the abandoned name |

The first one is the whole relay claim in one path: the bytes that come off the relay
queue are asserted byte-identical to the captured block, the block passes
`authenticate_block`, a hollowed one does not, and then the replay harness executes *that
same block* to the state root its header commits to. The malicious peer was already
gated — `loopback.rs` isolates one announcing key A and signing with B, and one answering
on another network.

Conformance is **183 passed, 2 ignored**, from 179.

### Bulk history, and a check that was missing

`rebuild-accounting` walked back from the tip a block at a time through one client with an
eight-attempt retry. From the mainnet checkpoint that is thousands of requests down one
connection, so a hosted API's rate limit was the repair's speed — one run was left going
for 1h45m. It takes a comma-separated peer list now and spreads the walk with the same
`TenureSource` the descent uses, forgiving throttles between attempts because a walk this
long outlives any notion of "the round".

Spreading a fetch over strangers needs one thing to be true, and it was not:
`SyncClient::block` did not check that the block it got back was the block it asked for.
Any peer in a pool could have substituted a block at any step and the caller would have
carried on from the substitute. A block is content-addressed, so the fix is a comparison —
and it is what makes "which peer answered" irrelevant to the answer.

### Documentation

`docs/joining-the-peer-network.md`: the shortest configuration that works (two lines, on
mainnet), what each `p2p_seeds` spelling means including `[]` as "HTTP only" said out
loud, why `p2p_bind` is optional and what a node without it gives up, why `p2p_address`
matters behind NAT and why a wrong one is worse than none, why `rpc_bind` is what makes
nano *fetchable* at all, the private-address rule and why it is a comparison rather than a
switch, a table of every resource bound and what each one bounds, the away-versus-wrong
scoring policy, the three files under `working_dir` with what deleting each one costs,
relay in both directions, the HTTP fallback and why nothing about it is trusted, and how
to read the five `p2p:` log lines.

### What is left, and it is small

1. **A miner's own block is not relayed from the miner.** `nano-miner` uploads over HTTP.
   Announcing it on the relay queue as well is a one-line change in `miner.rs`, which this
   slice did not own.
2. **An inventory-driven forward downloader**, which is what would make `assign_tenures`
   the scheduler. Still a rewrite of the downloader rather than a wiring change, and still
   its own task.
3. **A live stock `stacks-node` on the other end of a whole sync.** `p2p_relay.rs` and
   `p2p_inbound.rs` put the reference *codec* on the other end over a real socket, in both
   directions and for every message that matters. What stands between that and a live
   stock node is the stock node's own scheduler.

### Tried and reverted

- **`Relay::offer` and `announce` returning whether the item was new.** Clippy wanted
  `#[must_use]` on both and no caller in `nano-node` uses the answer, so it would have
  been `let _ =` at every real call site to satisfy a lint. They return nothing, and the
  tests assert the observable effect — which is the better assertion anyway.
- **Copying the upstream relayer list into what nano forwards.** It is a stranger's claim
  about third parties that nano has no way to check, and signing it would be republishing
  it as ours. Only loop prevention needs the list, and for that the sender's own entry is
  the only one that matters.
- **Keying the served inventory by the cycle's consensus hash**, which is how the wire
  names a cycle and so the obvious choice. A reorganization renames the cycle, and the row
  would have outlived the fork it described.
- **Encoding a relayed frame once and signing it per peer**, to avoid cloning the block
  eight times. Each message carries its own sequence number and so its own signature; the
  saving is one copy of a block per peer against a second signing path nothing else in the
  crate needs.

## The miner announces its own block now

The last gap the relay work left. A miner that pushes its block to one HTTP peer
depends on that peer to spread it — which is the dependency the whole p2p effort
exists to remove — and nano relays *everybody else's* blocks, so not relaying its
own was the one hole in that.

`announce` rather than `offer`, and the distinction is the point: `offer` is the
inbound queue, whose contents have to survive `ChainState::authenticate_block`
before anything happens to them, while a block this node mined was assembled on
its own tip and had its state root sealed here. There is nothing to authenticate it
against that it did not already pass.
