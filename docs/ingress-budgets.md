# Ingress budgets

This is the inventory for Task 132. It covers bytes or tasks whose amount or
lifetime can be influenced by an RPC client, a P2P peer, or an HTTP sync peer.
Limits are enforced before admission or while streaming; a limit mentioned only
after decoding is not counted here.

## Listeners and request work

| Surface | Global | Per address | Time and buffer bounds | Source |
| --- | ---: | ---: | --- | --- |
| Public RPC TCP | 256 connections | 16 | 10 s header, 64 KiB HTTP/1 buffer, 15 min connection | `nano-rpc/src/server.rs` |
| Prometheus TCP | 16 connections | 4 | 10 s header, 64 KiB HTTP/1 buffer, 30 s connection | `nano-rpc/src/server.rs` |
| RPC handlers | 128 active | route-specific below | 10 s body idle; 5–60 s handler deadline | `nano-rpc/src/limits.rs` |
| Read-only Clarity | 4 workers | route concurrency 4 | 60 s request; Epoch 4 read-only cost ceiling | `nano-rpc/src/lib.rs`, `chain.rs` |
| Inbound P2P | 64 conversations | 4 | 30 s I/O, 15 min idle, 4,096 messages | `nano-node/src/runtime.rs`, `nano-p2p/src/inbound.rs` |
| Outbound P2P | 8 sessions | one session per peer | 4 dials/round, 15 s operation deadline | `nano-p2p/src/swarm.rs` |

The public router has 24 route registrations and 24 `limits::guard` wrappers.
The classes below are `(body bytes, concurrency, requests/second, deadline)`:

| Class | Budget | Routes |
| --- | --- | --- |
| Cheap read | `0, 64, 512, 5 s` | node info, sync status |
| State read | `0, 16, 128, 15 s` | PoX, account, sortition, signer set, tenure metadata, StackerDB metadata |
| Archive read | `0, 16, 64, 30 s` | one block or StackerDB chunk |
| Large response | `0, 2, 8, 60 s` | raw tenure, tenure fork information |
| Event stream | `0, 64, 64, 5 s` | `/events`; its connection remains under the TCP lifetime |
| Read-only call | `4 MiB + 4 KiB, 4, 16, 60 s` | Clarity call-read |
| Transaction | `2 MiB, 16, 64, 30 s` | transaction submission |
| Mempool query | `128 KiB, 8, 32, 30 s` | mempool synchronization query |
| StackerDB upload | `4 MiB + 1 KiB, 8, 32, 30 s` | signed chunk upload |
| Block upload | `4 MiB, 4, 16, 60 s` | both block-upload spellings |
| Block proposal | `8 MiB + 4 KiB, 4, 16, 60 s` | authenticated proposal |

Decoded request limits are independent: Clarity arguments total 2 MiB, a
transaction is 2 MiB, block/chunk hex is checked before allocating its decoded
4 MiB/2 MiB form, a mempool query contains at most 8,192 tags and 128 KiB, and a
proposal's embedded block is at most 4 MiB.

One accepted proposal creates a verdict waiter after returning HTTP 202. There
is one validator consumer and the proposal queue retains two entries, so at most
three verdict waiters exist. Client cancellation cannot multiply the four
read-only workers: their semaphore permits live inside `spawn_blocking` until the
VM call finishes.

## P2P wire and decoded collections

The fixed preamble is decoded first. Its advertised payload length is validated
and the complete wire size is reserved before reading or allocating the payload.

| Object | Bound |
| --- | ---: |
| One wire message | 16,778,054 bytes |
| Frames held across the node | 134,224,432 bytes (8 maximum frames) |
| Frames held for one address | 33,556,108 bytes (2 maximum frames) |
| Relayers in one message | 16 |
| Neighbours in one message | 128 |
| Blocks in one pushed-blocks message | 32, within the wire-byte limit |
| Contract identifiers in one announcement | 256 |
| Advertised data URL | 128 bytes |
| Nakamoto inventory | 2,100 bits |
| Pushes retained by one outbound session | 256 items and one maximum frame |
| Pushes retained by all 8 outbound sessions | 134,224,432 bytes |

Inbound pushes are offered synchronously into the separately bounded relay
queues; inbound conversations have no retained push buffer. The frame permit
follows a decoded message until it is processed or shed.

## Queues, caches, and retained peer data

| Owner | Item limit | Byte limit | Full behavior |
| --- | ---: | ---: | --- |
| RPC block uploads | 8 | 16 MiB | 503 before acknowledgement |
| RPC proposals | 2 | 4 MiB | 503 before acknowledgement |
| RPC StackerDB replication | 256 | 16 MiB | 503 before replica/event mutation |
| RPC transaction relay | 256 | 16 MiB | 503 before mempool mutation |
| P2P offered relay | 1,024 | 32 MiB | shed oldest, count saturation |
| P2P announcing relay | 1,024 | 32 MiB | shed oldest, count saturation |
| One configured event observer | 1,024 | 32 MiB | omit event, preserve sequence gap |
| Shared local SSE slot | 1 | 32 MiB | omit event, preserve sequence gap |
| Mempool | 8,192 | 64 MiB canonical transactions | 503 before insertion/relay |
| StackerDB replica set | 16,384 chunks | 64 MiB payload | 503 before slot mutation/relay |
| Peer block cache, all roles/endpoints | 4,096 | 64 MiB canonical blocks | LRU eviction |
| Peer sortition cache, all roles/endpoints | 4,096 | 4 MiB represented JSON | LRU eviction |
| Followed tenure history | 16 tenures | 640 MiB canonical blocks | oldest eviction; 64 MiB per tenure |

The peer database retains 4,096 addresses. Relay de-duplication remembers 4,096
fixed 32-byte identifiers. The archive retains 20,000 locally executed blocks on
disk; a client-selected raw tenure stops at 64 MiB while SQLite rows are read.
`fork_info` shares one 64 MiB raw budget across at most ten entries before hex
expansion, keeping the JSON response within the client's 129 MiB limit, and only
two such responses may be built concurrently.

All queue/store/cache limits above publish current bytes/items, limits, drops or
saturations where applicable through `NodeMetrics`. RPC route and connection,
metrics connection, P2P frame and inbound-session admission publish the same
information directly from the accounting object that enforces the limit.

## Responses read from peers

| Client | Response class | Bound |
| --- | --- | ---: |
| `SyncClient` | JSON control | 4 MiB |
| `SyncClient` | block | 4 MiB |
| `SyncClient` | raw tenure | 64 MiB |
| `SyncClient` | hex tenure JSON | 129 MiB |
| `SyncClient` | mempool page plus cursor | 8 MiB + 32 bytes; 64 pages, processed one at a time |
| `SyncClient` | upload acknowledgement | 64 KiB |
| `StackerDbClient` | raw chunk | 2 MiB |
| `StackerDbClient` | slot metadata | 8 MiB |
| `StackerDbClient` | upload acknowledgement | 64 KiB |
| `BitcoinRestSource` | hash/height text | 128 bytes |
| `BitcoinRestSource` | raw Bitcoin block | 4,000,000 bytes |

Each HTTP reader rejects an oversized declared length before growth and enforces
the same limit while streaming an unknown/chunked body. JSON and consensus
decoding begin only after the bounded body is complete.

## Channel and task audit

Production node ingress uses `nano_queue` (Tokio bounded channel plus byte
reservation), bounded Tokio broadcasts, fixed role tasks, and bounded connection
`JoinSet`s. It contains no Tokio/std unbounded channel constructor. The one
`sync_channel(0)` in `nano-node` is test-only. The std unbounded channels in
`nano-tui` are intentionally outside this inventory: that binary is an operator
client with locally generated commands and is never linked into or fed by the
node's RPC/P2P listeners.

Configured event observers create one bounded queue and one drain task per URL;
their number comes from local configuration, not a network request. Node role
tasks are a fixed set. Public and metrics connection tasks are bounded by their
connection slots, inbound P2P tasks by 64/4 address slots, and outbound P2P tasks
by the eight-session swarm.
