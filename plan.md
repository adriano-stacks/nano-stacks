# nano-stacks: implementation plan

## Context

Re-implement [stacks-core](https://github.com/stacks-network/stacks-core/) from scratch as **nano-stacks**: a Stacks node supporting **epoch 4.0 only** (no epoch 2.x/3.x legacy), which syncs, follows, **executes transactions**, mines and signs inside [hacknet](https://github.com/stacks-network/hacknet).

stacks-core is 724k LOC across 17 crates and carries a decade of legacy; a 4.0-only node drops most of it. Optimize for **maintainability and low LOC**: clean module separation, unit tests, good coverage.

**Requirements:**
- Interop node — joins a chain stacks-core miners produce, via trie-graph checkpoint import
- Reuse **clarity-wasm** as the VM; fix its epoch-4.0 gaps as part of this work
- **HTTP-only sync** (no binary p2p)
- **Embedded signer**
- Rust

## Consequences of interop

- **State roots require bit-exact MARF and bit-exact Clarity.** `state_index_root` is history-dependent three ways: back-pointer children hash to the *ancestor block hash*; the root is a Merkle skip-list over ancestor roots `root(N-1), root(N-2), root(N-4), …`; Node4/16/48 pointer arrays pack in *insertion order*. Every Clarity write must land with the same key, value and ordering.
- **clarity-wasm must be fixed.** `feat/clarity-wasm-develop` has no `Epoch40`/`Clarity6`; Clarity 4 ~85% (missing `secp256r1-verify`), Clarity 5 ~33%, Clarity 6 untracked; cost tables stop at Clarity 3; the PoX `SpecialCaseHandler` is unwired in its contract-call path, so `pox-5.stack-stx` updates maps without locking STX. W6 closes this.
- **Mining needs both** — an empty tenure still mutates nonces, balances, tenure height and the MARF height keys.
- **clarity-wasm's API is clarity-crate-typed** (`GlobalContext`, `ContractContext`, `ContractAnalysis`, `VmExecutionError`), and it pulls `clarity`/`clarity-types`/`stacks-common` as git deps. Those types are used at the VM boundary, including `clarity_types::Value` (de)serialization, which is consensus-critical and inseparable from the VM.
- **Checkpoint at or after the 4.0 boundary** ⇒ no epoch transition ever runs, so `initialize_epoch_2_05 … 3_4` are not needed. Boot contracts arrive as imported state.

---

# Ground truth strategy

Nothing gets built without an oracle to check it against. Two structural decisions make every milestone verifiable in minutes, offline:

**1. stacks-core as a dev-dependency.** `nano-conformance` takes `stackslib`, `stacks-codec`, `stacks-common`, `clarity`, `clarity-types` as **dev-dependencies only** — never in production crates. This turns nearly every consensus primitive into a pure-function differential test against the real implementation: hashes, c32, VRF, secp256k1, the tx codec, Clarity values, **the MARF itself**, sortition math, ATC, consensus hashes, header hashing, signer weight apportionment, StackerDB chunk signing, libsigner message codec, burnchain op classification. `clarity-wasm` already pulls most of this closure in, so the cost is near zero.

**2. Captured fixtures, replayed offline.** Capture a real 4.0 chain once into `nano-conformance/fixtures/`, then every replay test runs deterministically in CI with no docker:

| Fixture | Source | Ground truth it provides |
|---|---|---|
| `bitcoin/blocks/*.hex` | `bitcoin-cli getblock <hash> 0` | raw burnchain input |
| `sortition/snapshots.json` | dump `burnchain/sortition/marf.sqlite` `snapshots` table | per-burn-block `consensus_hash`, `sortition_hash`, winning txid, `total_burn`, pox payouts |
| `nakamoto/blocks/*.bin` | `/v3/blocks/:id` (consensus-serialized) | real blocks incl. `state_index_root` in each header |
| `events/new_block/*.json` | event-observer capture | **per-tx receipts**: status, cost, events — the receipt oracle |
| `chainstate/checkpoint-H/` | PCS export at the 4.0 boundary | starting state + published archival root |
| `stacker_set/cycle-N.json` | `/v3/stacker_set/:cycle` | reward set + weights |

**Oracle ladder**, cheapest first — a milestone uses the cheapest oracle that can falsify it:

1. **In-process stacks-core call** (pure functions, proptest) — no infra, milliseconds.
2. **Hardcoded vectors** lifted from stacks-core's own tests (pure data, clean-room safe).
3. **Offline fixture replay** — deterministic, CI-gated, no docker.
4. **Live hacknet RPC** comparison.
5. **Live interop** — our signature in their block; our block in their chain.

**Rules:** no component merges without its oracle test green; every milestone's test stays in CI as a regression gate; M1–M7 need no running infrastructure at all.

---

# Baseline and progress signal

## The walking skeleton comes first

Before any component is written, build the **whole pipeline as stubs** — burnchain ingest → sortition → block validation → execution → state root — wired end to end, with every stage returning `unimplemented!()` or a zero value. Point the replay harness at the fixtures and run it. It fails on block 1, immediately.

That failure *is* the baseline. From that moment every commit either moves the failure point later or doesn't, and the question "am I on track?" has a numeric answer instead of a vibe.

## The scoreboard

One command, `cargo xtask scoreboard`, runs every oracle and prints the state of the world. Run on every commit and in CI:

```
surface              oracle                     passing        first failure
──────────────────────────────────────────────────────────────────────────────
hashes               in-process proptest        10000/10000    —
crypto               in-process proptest        10000/10000    —
addresses            in-process proptest        10000/10000    —
codec                fixture round-trip          1240/1240     —
codec                proptest vs stacks-codec   10000/10000    —
burn ops             fixture blocks               412/412      —
sortition            snapshots.json               412/412      —
marf                 node byte vectors                5/5      —
marf                 lockstep fork/COW scripts  10000/10000    —
marf                 PCS import                       1/1      —
vm                   crosscheck words             487/512      secp256r1-verify
vm                   cost dimensions             9820/10000    read_count @ fold
envelope             fixture blocks               500/500      —
replay: state root   fixture block headers          87/500     block 88: state_index_root
replay: receipts     new_block events               87/500     —
──────────────────────────────────────────────────────────────────────────────
REPLAY DEPTH: 87 / 500 (17%)
```

## North-star metric: replay depth

**The height at which offline replay first diverges from the fixtures.** One integer, monotonically increasing, computed with zero infrastructure. It subsumes every other check — a MARF bug, a cost bug, a write-ordering bug, a wrong Clarity error all surface as the same number failing to move.

Report it with the **first-divergence field** (`consensus_hash`? `block_hash`? `state_index_root`? a receipt?), because that names which component is wrong. Divergence at block N with a matching state root but a mismatched receipt is a VM error-mapping bug; a mismatched state root with matching receipts is MARF or write ordering.

Two secondary counters, useful because replay depth stays at 0 until M9: **oracle coverage** per surface (the table above), and **surfaces green** (11/15).

## Critical path

`M0 → M7 (MARF) → M8 (VM) → M10 (replay)`.

Everything else — codec, addresses, crypto, burnchain ops, sortition, StackerDB — is fast, parallel, and independently verifiable. Schedule risk lives almost entirely in **MARF bit-exactness and the clarity-wasm rebase**. Read the scoreboard accordingly: green rows piling up in the top half while `marf` and `vm` stay red means the project is *not* moving, however good it looks.

**Halfway checkpoint:** M7b green (MARF + PCS import) and M8a/M8b green. If MARF lockstep is not passing at the halfway mark, M10 will not land.

## Tripwires

Pre-agreed, so nobody has to argue about sunk cost mid-flight.

| If… | Then |
|---|---|
| Conformance harness not compiling / fixtures not captured early | Stop. Do not start components — nothing downstream is verifiable without it. |
| clarity-wasm not compiling against develop+Epoch40 by ~¼ of budget | Point `nano-vm` at the **clarity interpreter** instead (same dependency closure, already Clarity-6-correct, costs-4/5 correct). M10 and the interop goal survive intact; only the clarity-wasm requirement slips, and it swaps back in later behind the same trait. **This is the highest-value fallback in the plan.** |
| MARF lockstep red by ~¼ of budget | Bisect with node byte vectors before lockstep scripts. The cause is nearly always one of the four named traps: Node48 `indexes` in the preimage, omitted empty slots, insertion-order packing, or the ancestor skip-list. |
| hacknet 4.0 not producing blocks | Capture fixtures from the live internal pox-5 testnet instead; no infra to stand up. |
| Cost dimensions red but everything else green | Ship it — costs only diverge near block limits. Log it, keep replay running, fix after M10. |

---

# Milestones

Each is hours, not days, and independently falsifiable. `W`n refers to the component specs below.

| M | Builds | Oracle | Pass condition |
|---|---|---|---|
| **M0** | walking skeleton (stubs wired end to end) + conformance harness + fixtures + scoreboard | — | `cargo xtask scoreboard` runs and reports **replay depth 0/N, first failure: block 1** |
| **M1** | hashes, `Uint256`, `BitVec` (W1) | in-process | proptest equality on random inputs, all types |
| **M2** | secp256k1, VRF (W1) | in-process | recovery matches incl. **deliberately high-S** sigs; VRF proof bytes equal |
| **M3** | c32/b58, `PoxAddress`, sBTC taproot (W1) | in-process | round-trip + cross-check, all version bytes and variants |
| **M4** | SIP-005 codec (W2) | in-process + fixtures | every fixture tx decodes and **re-encodes byte-identically**; txid + merkle root match |
| **M5** | burnchain op parsing (W3) | fixtures + in-process | for every fixture bitcoin block, op set == `stackslib`'s, field by field |
| **M6** | sortition (W4) | `snapshots.json` | replay burn range; every snapshot field matches, every burn block |
| **M7a** | MARF, fresh genesis (W5) | in-process lockstep | random insert/fork/COW scripts: root matches after **every** block |
| **M7b** | PCS checkpoint import (W5) | `checkpoint-H/` | root at H == published archival root; extending H+1 matches stacks-core |
| **M8a** | clarity-wasm rebased to Epoch40/Clarity6 (W6.1) | its own crosscheck suite | existing suite green on the new epoch/version |
| **M8b** | Clarity 6 words (W6.2) | interpreter crosscheck | each new word matches the interpreter on random inputs |
| **M8c** | costs-4/costs-5 (W6.3) | interpreter crosscheck | all five cost dimensions match exactly on random snippets |
| **M8d** | PoX special-case wiring (W6.4) | balance assertion | `pox-5.stack-stx` **moves locked STX**, not just map entries |
| **M8e** | backing store over `nano-marf` (W6.5) | boot contract deploy | all boot contracts deploy; state root stable across reopen |
| **M9** | envelope validation + reward sets (W8) | fixtures + `stacker_set` | same `block_hash`/`signer_signature_hash` per block; accepts exactly what the network accepted; reward set matches |
| **M10** | **full execution** (W7, W8) | block headers + `new_block` events | replay from checkpoint: **`state_index_root` matches every block**, and every tx receipt (status, cost, events) matches |
| **M11** | HTTP sync, live follow (W9, W12) | live hacknet | tip tracks across ≥2 reward cycles incl. a prepare phase and cycle rollover |
| **M12** | StackerDB + embedded signer (W10) | live interop | our signature lands in a stock miner's block |
| **M13** | miner (W11) | live interop | our block is signed by stock signers and accepted by stock nodes; chain advances through a nano-won sortition |

**M0 is the first thing built.** Nothing is verifiable before it exists. It also gates fixture capture, which needs a 4.0 chain — either hacknet-4.0 (W13, start in parallel immediately) or the live internal pox-5 testnet (`api.testnet-pox5.hiro.so`, chain id `0x80000005`, Esplora `mempool.testnet-pox5.hiro.so`), which requires standing nothing up.

**Dependencies:** M1 → M2/M3 → M4 → M5 → M6. M1 → M7a → M7b. M8a → M8b/M8c/M8d; M7b + M8e → M10. M4+M6+M7b+M8 → M9 → M10 → M11 → M12 → M13. W13 (hacknet 4.0) runs parallel from the start and blocks only M0's fixture capture and M11+.

**M10 is the milestone that matters.** Everything before it is a component check; M10 is the first end-to-end proof that nano-stacks computes the same chain state as stacks-core. Build the replay harness before the components it validates.

**Notes on specific oracles:**

- **M4** — generate random transactions using `stacks-codec`'s own types, serialize with it, decode with nano, compare field-by-field, re-encode, assert byte equality. Then do it in reverse. Fixture txs cover the shapes that actually occur.
- **M6** — hacknet's sortition DB `snapshots` table is literal ground truth for every burn block; no RPC needed. `index_root` in that table is the *sortition* MARF root, which is not consensus-critical — ignore it.
- **M7a** — template the fork/COW script generator on `index/test/marf.rs::marf_walk_cow_test`. Also lift the hardcoded `to_consensus_bytes` vectors for every node type from `index/test/node.rs`, and anchor on the mainnet 2.0 genesis Clarity root `9653c92b1ad726e2dc17862a3786f7438ab9239c16dd8e7aaba8b0b5c34b52af`.
- **M8c** — cost divergence is invisible until a block nears a limit, so assert per-snippet dimension equality rather than only block-level acceptance.
- **M10** — assert receipts, not just state roots. Clarity error identity is consensus-visible (it lands in receipts and post-condition outcomes) and is a hand-maintained mapping table in clarity-wasm, so a wrong error is a real divergence that a state-root check can mask.

---

# Architecture

Rust workspace, one crate per consensus concern. Each independently unit-testable; the binary is thin.

| Crate | Contents | ~LOC |
|---|---|---|
| `nano-primitives` | Sha512_256 hash newtypes, `Uint256`, `BitVec<N>`, hex/serde | 800 |
| `nano-crypto` | secp256k1 recoverable sigs, VRF (ed25519) | 600 |
| `nano-address` | c32/c32check, b58, `StacksAddress`, `PoxAddress` incl. `Addr32`/P2TR, sBTC taproot derivation | 700 |
| `nano-codec` | SIP-005: tx, auth, post-conditions, payloads, txid, tx merkle root | 2500 |
| `nano-burnchain` | `bitcoincore-rpc` + `rust-bitcoin` ingest, magic filter, OP_RETURN op parsing, burn DB | 1500 |
| `nano-sortition` | burn distribution, ATC, `SortitionHash`, `OpsHash`/`ConsensusHash`, snapshot chain, cycle math | 1800 |
| `nano-marf` | bit-exact MARF, no proofs, PCS checkpoint import | 3300 |
| `nano-vm` | `ClarityBackingStore`/`HeadersDB`/`BurnStateDB` over `nano-marf`; clarity-wasm driver; PoX special-case handler | 2500 |
| `nano-chainstate` | Nakamoto block/header types, signature hashes, signer-set verification, tenure rules, `append_block`, reward sets, staging | 3500 |
| `nano-sync` | HTTP tenure/block downloader, fork choice | 700 |
| `nano-stackerdb` | chunk format + signing, libsigner v0 `SignerMessage` codec | 1200 |
| `nano-signer` | embedded signer state machine, sortition/reorg checks | 1000 |
| `nano-miner` | bitcoin op construction, UTXO mgmt, block assembly, signer coordination | 2200 |
| `nano-rpc` | axum RPC subset + event dispatcher | 1200 |
| `nano-node` | config, wiring, event loop | 1000 |
| `nano-conformance` | **dev-only**: stacks-core oracles, fixtures, replay harness | — |

**~24k LOC** vs stacks-core's 724k.

**Omitted:** binary p2p (Nakamoto block download already runs over HTTP via `/v3/tenures/*`), Atlas/attachments, microblocks, cost estimation, shadow blocks, Bitcoin SPV/indexer (trust hacknet's bitcoind over RPC), stacks-core's mio HTTP stack, MARF merkle proofs (squashed MARFs can't serve them), `signers-voting`/WSTS (dead in 4.0), multi-output PoX payouts (waterfall pays one output), `at-block` (removed in 3.4 — `supports_at_block()` is `< Epoch34`).

---

# Component specs

## W1 — Primitives, crypto, addresses → M1, M2, M3

Hashes, all `Sha512_256` unless noted: `TrieHash`, `BlockHeaderHash`, `StacksBlockId`, `SortitionId`, `BurnchainHeaderHash` (32); `ConsensusHash` (20); `Hash160` (RIPEMD160∘SHA256); `Sha256Sum`, `Sha512Sum`. `TrieHash::EMPTY = c672b8d1ef56ed28ab87c3622c5114069bdd3ad7b8f9737498d0c01ecef0967a`.

`Uint256` — fixed-point arithmetic for ATC; never f64. `BitVec<N>` — `pox_treatment` is `BitVec<4000>`.

secp256k1: `MessageSignature` (65 bytes, recovery id first). **Two distinct rules:** signer signatures recover *without* low-S validation (`recover_to_pubkey_without_validating_low_s`, `stacks-common/src/util/secp256k1/native.rs`) — naive libsecp256k1 rejects sigs consensus accepts; *transaction* signatures reject high-S in 4.0 (`allows_tx_signatures_with_high_s()` is `< Epoch40`).

VRF: ed25519-based, `VRFProof` (80 bytes), `VRFPublicKey`, prove + verify. Drives sortition seeds.

Addresses: c32/c32check (Crockford base32 variant), b58, `StacksAddress{version, bytes: Hash160}`, `PoxAddress::{Standard, Addr20, Addr32}`. sBTC taproot: `sbtc_pox5_deposit_taproot_output_key(aggregate_pubkey, sbtc_recipient = .pox-5, POX_5_SBTC_DEPOSIT_MAX_FEE_SATS = 80_000)` → `PoxAddress::Addr32(P2TR)` (`stackslib/src/chainstate/stacks/sbtc.rs:95`).

## W2 — SIP-005 codec → M4

`StacksMessageCodec` trait: `consensus_serialize`/`consensus_deserialize`, big-endian, length-prefixed vectors with bounds.

`StacksTransaction{version, chain_id, auth, anchor_mode, post_condition_mode, post_conditions, payload}`. `chain_id` is a 4-byte big-endian field in the signing preimage.

Auth: `Standard`/`Sponsored`; `SinglesigSpendingCondition`, `MultisigSpendingCondition` (incl. order-independent). Post-conditions: STX/Fungible/NonFungible, SIP-040 forms, and 4.0's `Staking`/`Pox` (`supports_staking_post_conditions()`).

Payloads: `TokenTransfer`, `ContractCall`, `SmartContract(_, Option<ClarityVersion>)` (wire bytes 1–6; `6u8` at `stacks-codec/src/transaction.rs:2784`), `PoisonMicroblock` (parse/reject), `Coinbase(payload, Option<PrincipalData>, Option<VRFProof>)`, `TenureChange(TenureChangePayload)`.

`TenureChangePayload{tenure_consensus_hash, prev_tenure_consensus_hash, burn_view_consensus_hash, previous_tenure_end, previous_tenure_blocks, cause, pubkey_hash}`; `TenureChangeCause::{BlockFound=0, Extended=1, ExtendedRuntime=2, ExtendedReadCount=3, ExtendedReadLength=4, ExtendedWriteCount=5, ExtendedWriteLength=6}` (SIP-034 per-dimension extends).

`txid = Sha512_256(consensus_serialize(tx))`. Tx merkle root per stacks-core's tagged-node scheme.

## W3 — Burnchain ingest → M5

`bitcoincore-rpc` against hacknet's bitcoind (`hacknet:hacknet@bitcoin:18443`, `txindex=1`); `rust-bitcoin` for block/tx parsing. Magic bytes (`T3` in hacknet) filter at parse time: output 0 must be OP_RETURN starting with the 2-byte magic, else the tx is not a burnchain tx at all.

| Op | Byte | Payload |
|---|---|---|
`LeaderBlockCommit` | `[` 0x5b | block hash(32) ‖ new seed(32) ‖ parent blk(u32) ‖ parent txoff(u16) ‖ key blk(u32) ‖ key txoff(u16) ‖ `(memo[0]<<3)｜(burn_parent_modulus & 0b111)`(1) |
`LeaderKeyRegister` | `^` 0x5e | consensus hash(20, ignored) ‖ VRF pubkey(32) ‖ block-signing Hash160(20) ‖ memo |
`PreStx` | `p` 0x70 | marker; its output authenticates a later op |
`StackStx` | `x` 0x78 | ustx(u128) ‖ cycles(u8) ‖ [signer key(33)] ‖ [max_amount(u128)] ‖ [auth_id(u32)] |
`TransferStx` | `$` 0x24 | ustx(u128) ‖ memo(≤61) |
`DelegateStx` | `#` 0x23 | ustx(16) ‖ reward-addr-output-index opt(5) ‖ until-burn-height opt(9) |
`VoteForAggregateKey` | `v` 0x76 | signer_index(u16) ‖ agg key(33) ‖ round(u32) ‖ cycle(u64) = 47 bytes; vestigial in 4.0 but still executes a `.signers-voting` call |

`PreStx` pairing uses a `pre_stx_op_map` within `BURNCHAIN_TX_SEARCH_WINDOW = 6` blocks.

## W4 — Sortition → M6

Burn distribution over the 6-block mining commitment window → `BurnSamplePoint` ranges; ATC ("assumed total commit") anti-griefing fixed-point in `Uint256` (`chainstate/burn/atc.rs`); `SortitionHash` mixing and sampling; `select_winning_block`.

`OpsHash::from_txids`; `ConsensusHash::from_ops` mixes prior consensus hashes at power-of-2 offsets (`chainstate/burn/mod.rs:217-380`). Inputs are `burn_header_hash, opshash, total_burn, prev consensus hashes, pox_id` — not magic bytes or chain_id.

`BlockSnapshot` chain and snapshot DB. **The sortition MARF root is not consensus-critical** (it appears in no header field and is not an input to `ConsensusHash::from_ops`) — index it however is convenient. Same for the headers index.

Cycle math: reward length 20 / prepare 5 in hacknet; 4.0 uses `starts_reward_cycle_at_0()`. `first_pox_waterfall_block` = first block of the cycle *after* the one containing `pox_5_activation_height` (`burnchains/mod.rs:636`). `active_pox_contract_for_cycle` is **cycle-keyed, not tip-keyed**, so a prepare phase can't flip mid-way.

## W5 — MARF (bit-exact) → M7a, M7b

Source of truth: `stackslib/src/chainstate/stacks/index/{bits,node,trie,marf,storage}.rs`.

Nodes: `TrieNodeID::{Empty=0, Leaf=1, Node4=2, Node16=3, Node48=4, Node256=5}`. ID control bits: `0x80` = back-pointer (**consensus**); `0x40`/`0x20`/`0x10` are wire/storage only. Node4/16/48 hold `ptrs` packed in **insertion order** (first empty slot); Node256 is chr-indexed. Node48 also carries `indexes[256]`. Path compression: 1-byte length prefix, max 32; leaves store the whole remaining suffix.

`TriePtr{id, chr, ptr, back_block}`. Consensus preimage of an internal node:

```
id_byte (0x80 intact, 0x40/0x20/0x10 cleared)
‖ for each ptr in ptrs (fixed array order, INCLUDING empty slots):
      ptr.id(1) ‖ ptr.chr(1) ‖ (is_backptr ? ancestor StacksBlockId(32) : [0u8;32])
‖ len(path)(1) ‖ path
‖ then exactly ptrs.len() child hashes (32 each, same order):
      empty → TrieHash::EMPTY
      inline → that child's node/leaf hash
      backptr → the ancestor block's StacksBlockId
```

Traps: Node48's `indexes` array is **excluded** from the consensus preimage. Empty slots **are** emitted and **do** contribute `TrieHash::EMPTY`. `ptr.ptr` and `ptr.back_block` are never in the preimage — only `id`, `chr`, and the resolved 32-byte block hash, which is what makes storage swappable.

Leaf hash: `Sha512_256(0x01 ‖ len(path) ‖ path ‖ data[0..40])`.

Root: `root(N) = Sha512_256(content_hash(N) ‖ root(N-1) ‖ root(N-2) ‖ root(N-4) ‖ root(N-8) ‖ …)` while `2^j <= N`; at N=0 the root is the bare content hash. Ancestors resolve *through the MARF* via `__MARF_BLOCK_HEIGHT_TO_HASH`, so the fork you stand on determines them.

`MARFValue([u8;40])` = 32-byte value hash ‖ 8 zero bytes. `from_value(s) = Sha512_256(s)`; `From<u32>` is **little-endian** in bytes 0..4; `From<StacksBlockId>` is the 32 bytes. Key→path: `TrieHash::from_key(k) = Sha512_256(k)`, with empty string → `TrieHash::EMPTY`.

Reserved keys (ordinary leaves, part of the root): `__MARF_BLOCK_HASH_TO_HEIGHT::<hash>`, `__MARF_BLOCK_HEIGHT_TO_HASH::<height>`, `__MARF_BLOCK_HEIGHT_SELF`. `set_block_heights` inserts **5** keys for height > 0 (self, own height→hash, own hash→height, parent height→hash, parent hash→height), 3 at height 0. `insert_batch` runs the skip-list update **only on the last key**.

COW: `root_copy` / `node_child_copy` / `node_copy_update_ptrs` — turn inline children into back-pointers, and **preserve a non-zero `back_block`, only setting it when it is 0** (this rule is what makes checkpoint import hash-preserving).

Clarity key strings: `vm::{contract_id}::{StoreType}::{var}` (trip), `…::{key_value}` (quad), `vm-metadata::{StoreType}::{var}`, `vm-epoch::epoch-version`. The MARF stores only the 40-byte `MARFValue`; full value strings live in a side store keyed by `MARFValue.to_hex()`, not consensus-hashed beyond `Sha512_256(value) == MARFValue[0..32]`.

Storage is free-form (sqlite or sled): needs `(block_id → trie)`, `(block_id ↔ StacksBlockId)`, `(node ref → node + hash)`. Skip stacks-core's blob layout, mmap, pointer compression, `TrieNodePatch`/`MAX_PATCH_DEPTH`, caches, squash pipeline, and `proofs.rs`.

**PCS checkpoint import** (`contrib/marf-squash`, `index/squash.rs`): import the **trie node graph** at height H — not flat KV — plus `back_block` block-identity annotations on formerly-back-pointer children, a `(height → block_hash, archival_root_hash)` table for `0..=H` that the ancestor skip-list short-circuits into, and the value side store. Requires epoch 3.4+, which holds. PCS correctness is not verifiable in-protocol; the root comes from an out-of-band publication.

## W6 — clarity-wasm to epoch 4.0 → M8a–M8e

Fork `stx-labs/clarity-wasm` and its `feat/clarity-wasm-develop` stacks-core branch.

1. **Rebase onto develop** (Epoch40 + Clarity6): `clarity/src/vm/clarity_wasm.rs` is ~400 KB and divergent; `ClarityVersion` gains `Clarity6`, `StacksEpochId` gains `Epoch40`, `default_for_epoch(Epoch40) = Clarity6`.
2. **Clarity 6 words**: `verify-merkle-proof` (Bitcoin double-SHA256 inclusion, hardened against CVE-2012-2459 inflated-`tx-count` forgeries), `get-bitcoin-tx-output?` (SegWit-aware, returns output N + witness-stripped txid), `ed25519-verify`, `secp256k1-decompress?`, variadic `concat` (currently fixed-arity). Also Clarity 4's missing `secp256r1-verify` and the Clarity 5 gaps. `with-stacking` → `with-staking`.
3. **Costs**: add `clar4`/`clar5` cost modules mirroring `costs-4`/`costs-5`; `BLOCK_LIMIT_MAINNET_40` doubles `read_length` and `read_count` (write and runtime unchanged).
4. **PoX special cases**: wire `get_cc_special_cases_handler` into the `stdlib.contract_call` host path so `pox-locking`'s `handle_contract_call_special_cases` fires.
5. **Backing store**: implement `ClarityBackingStore` + `HeadersDB` + `BurnStateDB` over `nano-marf` and nano's headers/sortition DBs, replacing the dev-only `datastore.rs` (`developer-mode`-gated, full of `panic!`/`unreachable!`).
6. **wasmtime** off 15.0.0.
7. **Crosscheck**: clarity-wasm ships `crosscheck()`/`crosseval()` harnesses running a snippet through interpreter and wasm and asserting equality. Extend to replayed blocks; any divergence blocks. Expect open divergences around trait lists, contract-analysis types, and empty-buffer serialization.

## W7 — Boot contracts and PoX locking → M8e, M10

Embed boot `.clar` sources byte-identically (~11k lines): `pox`, `lockup`, `costs`, `cost-voting`, `bns`, `genesis`, plus `pox-2/3/4/5`, `costs-2/3/4`, `signers`, `signers-voting`, `sip-031`. `pox-5.clar` is 3,851 lines. Non-mainnet needs `make_pox_5_body`'s textual substitution of the sBTC token literal and the bond/pause admin principals (`node.pox_5_sbtc_contract`, `pox_5_sbtc_registry_contract`, `pox_5_bond_admin`, `pox_5_pause_admin`).

Importing at/after the 4.0 boundary means no epoch initializer runs; contracts arrive as state. `.cost-voting` is disabled in 4.0 (SIP-044).

Reimplement `pox-locking`'s native side effects (stacks-core: 6,790 LOC, `pox_5.rs` alone 2,743) — lock/unlock semantics intercepted on the contract-call boundary for pox-5 entrypoints.

## W8 — Nakamoto chainstate → M9, M10

`NakamotoBlockHeader{version, chain_length, burn_spent, consensus_hash, parent_block_id, tx_merkle_root, state_index_root, timestamp, miner_signature, signer_signature: Vec<MessageSignature>, pox_treatment: BitVec<4000>, problematic_txs: Vec<ProblematicTxMarker>}`.

`NAKAMOTO_BLOCK_VERSION_EPOCH_4 = 1`; `expected_version_for_epoch` rejects mismatches; `0x80` remains the shadow flag. `problematic_txs` (miner-flagged txs to skip during replay) is in the hash and signature preimages **for v1 only**.

```
signer_signature_hash = Sha512_256( version ‖ chain_length ‖ burn_spent ‖ consensus_hash
  ‖ parent_block_id ‖ tx_merkle_root ‖ state_index_root ‖ timestamp
  ‖ miner_signature ‖ pox_treatment [‖ problematic_txs if (version & 0x7f) >= 1] )
```
`miner_signature_hash` is the same preimage minus `miner_signature`. `block_hash() == signer_signature_hash()` — signatures are not committed to. `block_id = (consensus_hash, block_hash)`.

`verify_signer_signatures`: recover each sig without low-S validation, map to a `RewardSet` entry, **each pubkey usable once**, **strictly increasing signer index** (`enforces_strict_signature_order()` in 4.0), accumulate weight ≥ `compute_voting_weight_threshold(total) = ceil(total * 7 / 10)`.

Reward set: `RewardSet::Waterfall(WaterfallCycleSet{sbtc_address, signers: Vec<NakamotoSignerEntry>, pox_ustx_threshold})`. Derivation walks a pox-5 linked list — `get-signer-set-first-item-for-cycle` → `get-signer-set-next-item-for-cycle`, with `get-signer-info` (33-byte key) and `get-amount-delegated-for-signer` per node. Weights: `threshold = ceil(total / reward_slots)`, base `= floor(stacked / threshold)`, leftover slots by descending fractional remainder, ties by pubkey ascending, **final set sorted by `signing_key`** (drives the signer bitvec). Min to be a signer: `POX_5_SIGNER_SET_MIN_USTX = 50_000_000_000`. `sbtc_address` comes from the sBTC registry's `get-current-aggregate-pubkey` → taproot derivation (W1). Empty signer set is fatal. Persist to `nakamoto_reward_sets` / `preprocessed_reward_sets`.

`append_block`: `setup_block` (tenure height, burn-op processing, `check_and_handle_reward_start`, `check_and_handle_prepare_phase_start`) → transaction execution → `finish_block` (matured miner rewards — pure Rust over headers + balances, `process_stx_unlocks` reading `.lockup`, SIP-031 mint/transfer gated on `includes_sip_031()`) → **`state_index_root` check against the sealed MARF root**. `check_pox_bitvector` is a no-op under waterfall (`rewarded_addresses()` is `None`).

Tenure: `is_wellformed_tenure_start_block` / `is_wellformed_tenure_extend_block`, `validate_vrf_seed`, tenure DB. Staging blocks, `accept_block`, `process_next_nakamoto_block`. Coinbase in 4.0 returns to 1,000 STX (SIP-045).

## W9 — HTTP sync → M11

`GET /v3/tenures/info` for the tip → `GET /v3/tenures/:block_id` (streams a tenure backwards) → `GET /v3/blocks/:block_id`; `/v3/sortitions[/:query/:value]` and `/v3/tenures/fork_info/:start/:stop` for burn context and reorg detection. Trustless: everything validated locally. Fork choice on chain length with valid signature weight, against the burn view.

## W10 — StackerDB + embedded signer → M12

Chunk: `{slot_id, slot_version, data, sig}`, signed by the slot's writer key (stacks-core's `libstackerdb` is 325 LOC). Replication entirely over `GET/POST /v2/stackerdb/...`.

Contracts: `SP000000000000000000002Q6VF78.miners` — 2 slots, parity on `num_sortitions % 2`, writers are the block-signing Hash160s from the winners' leader-key registrations (`make_miners_stackerdb_config`). `.signers-{0,1}-{msg_id}` per message slot per cycle parity.

`MinerSlotID::{BlockProposal=0, BlockPushed=1}`; `MessageSlotID::{BlockResponse=1, StateMachineUpdate=6, BlockPreCommit=7}`. `SignerMessage::{BlockProposal, BlockResponse(Accepted(BlockAccepted)|Rejected(BlockRejection)), BlockPushed, StateMachineUpdate, BlockPreCommit, MockSignature, MockProposal, MockBlock}` with `RejectCode`/`RejectReason`.

Signer: read proposals from the miner slot, **fully validate** (envelope + execution + state root), run sortition-view and tenure-fork-info reorg checks, write `BlockResponse::Accepted{signature}`.

## W11 — Miner → M13

Bitcoin txs via `bitcoincore-rpc`: leader key register (`^`) and leader block commit (`[`), formats in W3. **Waterfall commits carry exactly one output equal to `sbtc_address`**; zero-amount rejected. UTXO selection from the configured wallet, fee bumping, `sendrawtransaction`.

hacknet wallet mechanics: pre-create the wallet with `descriptors=false` and import the miner address watch-only, mirroring `docker/bitcoin/miner.sh` — letting the node create its own wallet yields a descriptor wallet and breaks the setup.

Block assembly: tenure-change tx (or extend), coinbase with VRF proof, mempool txs under `BLOCK_LIMIT_MAINNET_40`, MARF seal → `state_index_root`, sign header. Coordination: write `BlockProposal` to `.miners`, accumulate `BlockResponse` weight to ≥70%, assemble `signer_signature` **in signer-index order**, push the block.

## W12 — RPC + event dispatcher → M11

Serve `/v2/info`, `/v2/pox` (incl. `pox_5_sbtc_contract`, `pox_5_sbtc_registry_contract`), `/v2/accounts/:principal`, `/v2/contracts/call-read/...`, `/v2/transactions`, `/v3/block_proposal` (auth header), `/v3/stacker_set/:cycle`, `/v3/sortitions`, `/v3/tenures/{info,:block_id,tip_metadata,fork_info}`, `/v3/blocks/:block_id`, `/v3/blocks/upload`, `/v2/stackerdb/...`. Event observer POSTs: `new_block`, `new_burn_block`, `stackerdb_chunks`, `proposal_response`, `mined_nakamoto_block`.

Makes nano-stacks usable by stock `stacks-signer`, the Hiro API, and hacknet's tooling, and lets `consensus-test/monitor.ts` compare it by adding a 4th URL. Also the source of our own `new_block` payloads, which makes nano's receipts diffable against stacks-core's.

## W13 — hacknet on epoch 4.0 → M0 fixtures, M11+

Add `STACKS_34_HEIGHT`/`STACKS_40_HEIGHT` anchors and `[[burnchain.epochs]] epoch_name = "3.4"/"4.0"` to `docker/stacks/stacks-miner_signer.toml` and the compose env block; pin images to stacks-core 4.0.1. Deploy `sbtc-token`/`sbtc-registry` stubs before the 4.0 boundary (pattern: `boot_to_epoch_4_0`, `stacks-node/src/tests/nakamoto_integrations.rs:1171`). Give `docker/stacker/stacking/stacking.ts` a pox-5 path — it hard-checks `contract_id.endsWith('.pox-4')` and would silently no-op, yielding an empty signer set, which is fatal.

Respect `validate_epochs`: `pox_5_activation_height == Epoch40.start`, in a reward phase at cycle offset > 1, and **3.0 and 4.0 must not share a reward cycle**. Rebuild the snapshot past activation: `MINE_INTERVAL_EPOCH3=10 PAUSE_HEIGHT=<n> make genesis && make snapshot`.

Add nano-stacks as an additive compose overlay on a free static IP (`.121`, host port `24443`); run it as a separate compose project joining `networks: default: {name: stacks, external: true}` so `make down` doesn't nuke it. Image needs bash+perl, a binary named `stacks-node` accepting `start --config`, config template at `/data/config.toml.in`, chainstate at `/data/chainstate`, clean SIGTERM. hacknet builds node images from a git branch, so local-source builds need their own Dockerfile.

Do **not** register nano wallets with the `bitcoin-miner` service: its on-demand trigger is `sum of confirmations across watched wallets == 0`, so joining that sum suppresses block production for the stacks-core chain. Fund nano miners with `sendtoaddress` from the existing `depositor` wallet. Burn blocks then arrive on the 30s timeout path.

---

# Risks

| Risk | Mitigation |
|---|---|
| clarity-wasm rebase across two epochs on a 400 KB divergent file | Longest lead time — start at M0 in parallel. Its own crosscheck suite is the gate (M8a), and the interpreter is in the tree as a fallback execution path. |
| Cost parity (no costs-4/5 today) — divergence invisible until a block nears a limit | M8c asserts per-snippet dimension equality against the interpreter, not just block acceptance. |
| MARF bit-exactness | M7a lockstep against stacks-core's own MARF, before anything depends on it. |
| Clarity error identity is consensus-visible and hand-mapped in clarity-wasm | M10 asserts receipts (status, cost, events), not just state roots. |
| PoX special-case handler wiring unverified | M8d is a dedicated balance assertion: `stack-stx` must move locked STX. |
| hacknet 4.0 on the critical path for fixtures | Parallel from M0. Fallback: capture fixtures from the live internal pox-5 testnet instead — no infra to stand up. |
| PCS correctness is not verifiable in-protocol | M7b checks the root at H against the published value; M10 then replays forward and matches stacks-core block-for-block. |
| Empty signer set is fatal; hacknet's stacker no-ops under pox-5 | W13; assert non-empty reward set at startup. |

