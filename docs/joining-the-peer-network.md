# Joining the Stacks peer network

nano speaks the binary Stacks p2p protocol directly. On mainnet it joins from
stacks-core's own published bootstrap nodes with no configuration at all, and no
hosted Stacks API is involved in synchronization, propagation or any consensus
input.

This document is the operator's half: which knobs there are, what each one costs
if it is wrong, and what to do when the peer table or the served inventory goes
bad.

## The shortest configuration that works

```toml
[node]
working_dir = "/var/lib/nano"
network = "mainnet"
```

`node.peers` may now be empty. That is the point of the transport: a node needs
*a* way into the network, and requiring an HTTP peer specifically is what made a
hosted API load bearing. A configuration that names **neither** `node.peers` nor
`node.p2p_seeds` is still refused, because a node with no way in does nothing and
should say so at startup rather than at the first poll.

## Seeds

```toml
p2p_seeds = [
  "02196f00…93@seed.mainnet.hiro.so:20444",
  "cet.stacksnodes.org:20444",
]
```

| Value | Meaning |
|---|---|
| omitted, `network = "mainnet"` | stacks-core's four published bootstrap nodes |
| omitted, any other network | no seeds — the transport does not start |
| `p2p_seeds = []` | the transport is off. This is how a configuration says *HTTP only* out loud |
| a list | exactly those, as `<33-byte hex key>@<host>:<port>` or plain `<host>:<port>` |

The key is a label. A session learns the peer's key from its handshake and
authenticates every later message against **that**, so a configured key that turns
out to be wrong changes nothing about what nano accepts — treating a config file
as authority over evidence would be the wrong way round. It is kept because it is
what operators paste.

A hostname is resolved at startup and every address it resolves to is recorded, so
a round-robin seed contributes all of its endpoints.

Seeds are a starting point, never an authority. The first successful handshake
yields a `Neighbors` reply, and from then on the peer table is nano's own. A node
that has run before prefers what it remembers.

## Addresses

```toml
p2p_bind    = "0.0.0.0:20444"     # where to listen for inbound peers
p2p_address = "203.0.113.9:20444" # what to tell peers to dial back on
rpc_bind    = "0.0.0.0:20443"     # the HTTP endpoint peers are told about
```

**`p2p_bind` is optional and a node without it still syncs.** What it cannot do is
get into anybody else's peer table, which is the difference between using the
network and being part of it. A node behind NAT with no port forward should leave
it out rather than bind a port nothing can reach.

**`p2p_address` matters behind NAT.** Left out, the bind address is advertised; an
unroutable bind address is advertised as the any-net address, which peers read as
"I do not know my own address" rather than as a lie. Advertising an address that is
wrong is worse than advertising none: a peer that records it wastes one of its
connection slots on it and eventually stops offering nano to its own neighbours.

**`rpc_bind` is what makes nano fetchable.** In Nakamoto there is no p2p message
for requesting a block — stacks-core downloads blocks and tenures over HTTP to each
peer's own RPC endpoint — so the `data_url` in nano's handshake is the whole of how
another node syncs *from* nano. nano only advertises the RPC service bit when
`rpc_bind` is set to a routable address, because a peer that records an endpoint and
finds nothing listening has spent a connection on us.

### Private addresses need no switch

An endpoint a peer advertises is accepted if it is private and the peer is private,
or public and the peer is public. Mainnet really does advertise
`http://10.0.1.37:20443` — a load-balanced node naming the address it sees itself at
behind its own NAT — and a node that fetched from it would be dialling its own
network. Hacknet keeps working with no configuration change, because there both ends
are private. stacks-core's equivalent is a switch (`connection_opts.private_neighbors`),
and a switch is a thing to get wrong.

## Resource limits

None of these is configurable, and that is deliberate: every one of them is a bound
on what a *peer* can make this node spend, so it has to be nano's choice and not a
number in a file somebody copied.

| Bound | Value | What it bounds |
|---|---|---|
| outbound sessions | 8 | connections nano opens. stacks-core's default is 16; the number that matters is that it is greater than one |
| dials per round | 4 | so a table of four thousand addresses with no reachable peer still ends a round |
| request deadline | 15 s | one request, including whatever the peer pushes while answering |
| inbound conversations | 64 | tasks and sockets a flood of connections can cost |
| inbound read deadline | 30 s | one read or write on an inbound conversation |
| inbound idle | 15 min | how long an inbound conversation may be *silent*. Distinct from the read deadline, because stacks-core advertises a 3600-second heartbeat and closing at the read deadline meant nano hung up on quiet stock nodes twice a minute |
| messages per conversation | 4096 | a peer that wants to keep talking reconnects, which costs it a handshake |
| buffered pushes per peer | 256 | announcements held for the node while it is busy; the oldest are shed and counted |
| relay queue | 1024 each way | pushed blocks and transactions waiting to be checked, and accepted ones waiting to go out |
| relay memory | 4096 items | so each accepted block or transaction is relayed once |
| known peers | 4096 | a neighbour walk learns 128 addresses a reply, so an unbounded table is a peer-supplied disk write |
| frame length | ~16 MB | the protocol's own maximum, checked in the preamble before a byte of body is read |

Message volume is **not** bounded by a count, and the reason is worth knowing
because the opposite was a real bug. A mainnet peer relays 0.2 to 0.8 messages a
second — mostly signer chunks — so any fixed count of unsolicited messages is
crossed by any rate given enough silence, and nano was isolating its *busiest*
peers first. A request is bounded by one overall deadline instead. A peer that lets
the deadline pass is slow, which is nearly always a peer that is busy, and gets a
backoff rather than an isolation.

## What gets a peer set aside, and for how long

Two kinds of failure, and the difference is the whole policy.

**Away.** A peer that times out, refuses a connection or hangs up has almost
certainly restarted. Its session is dropped, the peer table gives it 30 seconds
doubling to an hour, and it stays in the table — the reason is far more often a
deployment than dishonesty, and forgetting it would leave a small network with
nowhere to go.

**Wrong.** A peer that sends a malformed message, signs with a key other than the
one it announced, contradicts itself about its own Bitcoin view, or answers on
another network is *isolated*: the longest penalty the table can express. Not a
permanent ban, on purpose — a malformed message is more often a version skew than
malice, and a node that bans permanently on protocol errors bans the network one
deployment at a time. What isolation buys is that a peer serving garbage stops
occupying one of eight session slots.

**Pushing a block nano rejects is neither.** A block can fail authentication
because *this* node cannot yet derive the cycle's reward set or has not executed
the tenure it builds on, and scoring a peer for that would isolate the peers doing
the most work. A rejected push costs one authentication and is logged.

A handshake clears a peer's failure count, so a peer that comes back is forgiven by
the same code path as any other. There is no second kind of forgiveness to get
wrong.

## Files under `working_dir`

| File | What it holds | Safe to delete? |
|---|---|---|
| `p2p-seed` | the seed this node's p2p identity is derived from | **No, not casually** — see below |
| `peers.sqlite` | every peer this node knows and how well that has gone | Yes |
| `served.sqlite` | which tenures nano will tell peers it has | Yes |

### `p2p-seed`

Peers remember a node by its key hash. A node that re-keyed every start would be a
new stranger to the whole network each time — including to the tables that had it
on a backoff, which is the half that would make restarting a way to launder a
reputation. Deleting the file gives the node a new identity: harmless once, and a
habit that makes nano look like an unstable peer to everyone.

The file holds the *seed*, not the key, so it stores the thing that regenerates the
identity rather than a second copy of the secret. Treat it as one.

Two nodes must not share it. A peer that sees one key arrive from two addresses
treats the second as a connection cycle and drops it, so two nodes with one identity
are worse off than one node.

### `peers.sqlite`

Delete it and the node relearns the network from its seeds within a round or two.
What is lost is the backoffs and isolations it had recorded, so a peer that had been
misbehaving gets a clean slate — which is why deleting it is a diagnostic step and
not a routine one.

If it is corrupt the node reports that it cannot open the peer table and carries on
without the transport, falling back to whatever `node.peers` holds. That is a
liveness decision: a broken file is not a reason to refuse to run, and a node with a
configured HTTP peer still syncs. Delete the file and restart to recover.

### `served.sqlite`

What nano tells a peer it has, accumulated a round at a time so it survives a
restart. Deleting it costs nano nothing and costs its peers a little: they will be
nacked for cycles nano could have served until the executed window has walked them
again. It is never read by execution and can never change what nano accepts.

## Relay

nano relays what it has locally accepted, in both directions, and nothing else.

A pushed block goes onto a bounded queue with no more claim attached than "this
peer said so". The follow loop drains it and puts every one through
`ChainState::authenticate_block` — the same call `/v3/blocks/upload` goes through,
deliberately, because a node that admits from a peer what it would refuse from its
own API is forkable through whichever of the two is laxer. What passes is staged,
and from there it is indistinguishable from a block nano fetched itself, state root
check included. What fails is dropped.

A relayed transaction is admitted on nano's own rules against nano's own executed
accounts, not on the sending peer's answer about them.

What nano then pushes back out is what passed, to every connected peer except the
one that sent it, once each. A relayed message is re-encoded and re-signed rather
than forwarded verbatim, because the relayer list is inside the frame the signature
covers — and nano names **itself and nothing else** there. An upstream relayer list
is a stranger's claim about which other nodes have seen the item; nano cannot check
any of it, and republishing it signed by nano would be passing it on as ours. An
item whose relayer list already names this node has been round the loop and is
dropped rather than checked again.

nano carries no signer or StackerDB messages over p2p. Identifiers 21 through 25 are
recognised and discarded, because nano replicates StackerDB over
`GET`/`POST /v2/stackerdb/...`, which is the same replication by the same rules over
the transport the rest of nano's block fetching already uses.

## The HTTP fallback

`node.peers` is an operator-selected bootstrap and diagnostic source. It is weighed
alongside the endpoints discovery finds, through the same `PeerPool::choose_source`,
so naming one does not make it authoritative — it makes it one candidate among
however many peers nano has found. A configured peer that stalls or falls behind
costs one round.

Two things to know if you point it at a hosted API:

* **Nothing about it is trusted.** Every block still passes the same
  authentication and the same state root check. What a hosted API can do is be
  slow, be down, or be on a fork; what it cannot do is change what nano accepts.
* **Bulk history is spread.** Catch-up from a checkpoint is tens of thousands of
  blocks, and sent down one connection a rate limit *is* the catch-up speed.
  Consecutive tenures go to different peers, a throttled peer is set aside for the
  round, and only when every peer has throttled does a round report itself rate
  limited. `cargo xtask rebuild-accounting` takes a comma-separated peer list for
  the same reason.

To take hosted APIs out of the picture entirely, leave `node.peers` out. On mainnet
that is the default path.

## Reading the log

```
p2p: 8 peers connected, 58 known, 6 endpoints to fetch from
p2p: 8 connected (4 new, 0 lost, 0 isolated), 2 addresses learned, 5 claiming this cycle
p2p: 40 messages peers sent unprompted, 12 of them for a role nano serves over HTTP
p2p: relayed 3 accepted items to peers in 21 pushes
peers pushed 3 blocks this node accepted and 0 it refused, and 7 transactions it will mine
```

* **`connected` well below 8 and `isolated` above 0** — the peers nano can reach
  are misbehaving, or nano is. Check the `dropping peer … after …` lines: the error
  names which.
* **`endpoints to fetch from` far below `connected`** — the peers nano found do not
  serve RPC, or advertise endpoints this node cannot reach. Normal behind NAT among
  private peers; suspicious on mainnet.
* **`claiming this cycle` at 0 with peers connected** — nano is asking about a cycle
  its peers do not recognise, which usually means its own burnchain view is behind.
* **`0 it refused` never moving while `accepted` does** — healthy. The reverse, all
  refused, means nano cannot yet derive the reward set for the tenures being pushed;
  it clears once the cycle is reconstructed.
