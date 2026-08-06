# nano-stacks: implementation plan

## Context

Re-implement [stacks-core](https://github.com/stacks-network/stacks-core/) from
scratch as **nano-stacks**: a Stacks node supporting **epoch 4.0 only** (no
epoch 2.x/3.x transition machinery), which starts from an attested checkpoint,
syncs, follows and **executes mainnet**, and mines/signs inside
[hacknet](https://github.com/stacks-network/hacknet).

stacks-core is 724k LOC across 17 crates and carries a decade of legacy; a 4.0-only node drops most of it. Optimize for **maintainability and low LOC**: clean module separation, unit tests, good coverage.

**Current requirements:**

- **UNCHANGED — interop:** join and validate a chain produced by stacks-core
  from an attested trie-graph checkpoint.
- **STRENGTHENED — execution:** use **clarity-wasm as the only production
  execution engine** and fix its epoch-4.0 gaps. No network, role, build profile,
  configuration or failure may fall back to the interpreter.
- **CHANGED — transport:** join the Stacks P2P network for handshake, discovery,
  inventories and relay; fetch history from multiple discovered peers. Hosted
  HTTP APIs are optional bootstrap/diagnostic inputs, never liveness or consensus
  dependencies.
- **EXPANDED — checkpoint:** import every authenticated input required to extend
  the chain, not only the MARF graph: executed ledger, burn/sortition and tenure
  context, maturity accounting, leader-key history and compiler identity.
- **EXPANDED — roles:** interoperate with stock signers and clients through the
  node RPC and StackerDB surfaces, in addition to the embedded signer.
- **UNCHANGED — implementation:** Rust, simple crate boundaries and no production
  stacks-core implementation dependency beyond the Clarity frontend/ABI types
  clarity-wasm itself requires.

## Plan amendments discovered during implementation

This section is canonical. It records decisions that replace assumptions in the
original plan; later sections are updated to match it. Historical task notes may
still describe the assumption that led to a finding.

| Label | Original assumption | Current plan | Why it changed | Tasks |
|---|---|---|---|---|
| **CHANGED — P2P is required** | HTTP-only synchronization; omit binary P2P | Speak the Stacks P2P protocol for peer discovery, inventories, push/relay and fork candidates. Use discovered peers' data endpoints for bulk history, with bounded failover and per-tenure attribution. A release run has no Hiro or other hosted endpoint configured. | Hiro rate limits and single-endpoint failure made HTTP-only sync operationally incapable of sustaining mainnet. Multiple configured URLs were not independence while signer roles remained pinned to one client. | 054, 071 |
| **STRENGTHENED — WASM only** | Reuse clarity-wasm, with the interpreter available as a convenient diagnostic | The node binary has one execution engine: clarity-wasm. Compile, load, host and runtime failures reject without retry. Interpreter crosschecks exist only in separately built, rolled-back conformance tooling. | A fallback can hide a compiler consensus bug and seal state that no single production engine can reproduce. | 060, 067, 068 |
| **STRENGTHENED — no production stacks-core shortcuts** | Use stacks-core freely as an oracle and potentially reuse small native helpers | stacks-core remains a dev/conformance oracle. Production consensus behavior, including PoX locking, is nano-owned; reference codecs and helpers do not enter the release dependency graph. | The node must demonstrate an independent implementation rather than route difficult production cases through stacks-core. | 061, 062 |
| **EXPANDED — checkpoint completeness** | A trie graph, values and archival root were sufficient | A checkpoint must also carry the coherent executed ledger, block/tenure and burn/sortition history, maturity accounting, old leader-key registrations, chain identity and compiler identity. Missing or contradictory pieces cause typed startup refusal. | A valid MARF root alone could not reconstruct rewards, validate VRF commitments or resume proposals, and an incomplete fresh import diverged at the first tenure boundary. | 043, 048, 051, 057, 058, 065, 070 |
| **CHANGED — consensus is local** | Peer `/v3/sortitions` and tenure views could drive following | Bitcoin-derived sortitions, fork choice, reward-cycle context and executed state are local. Peers advertise and serve candidates but cannot choose the burn height or canonical fork. | Hosted or dishonest peers must not become consensus inputs, and peer views can lag or equivocate. | 027, 049, 050 |
| **STRENGTHENED — exact receipts and costs** | A cost mismatch could ship if roots stayed green | Every known result, error identity, cost, event and write differential blocks release, even if no current block hits it or the state root happens to match. Ignored semantic differentials are failed release gates. | Costs affect block admission and error identities land in receipts; root-only replay hid real VM bugs. | 023, 037, 060, 064, 067, 068 |
| **CHANGED — old contracts still matter** | `at-block` and other pre-4.0 behavior could be omitted because new epoch-4.0 deployments cannot use it | Imported contracts compile under their recorded deployment epoch, execute with current-epoch costs, and apply epoch-4.0 runtime refusals exactly. | Mainnet calls historical contracts whose stored analyses remain valid; compiling all of them as new 4.0 source changes receipts or refuses valid chain history. | 064, 066 |
| **EXPANDED — release evidence** | Hacknet replay and a live-follow demo established readiness | Release requires a clean attested mainnet checkpoint-to-contemporaneous-tip replay, no hosted API, no skipped required gate, restart/crash/reorg evidence, stock signer/client journeys and a sustained tip hold. Targeted reflink resumes remain diagnostic only. | Component and short live tests proved useful but did not prove the assembled node could start cleanly and remain live on mainnet. | 037, 052, 053, 054, 069, 070, 071 |

The task files under `tasks/` are the executable plan. This document describes
the architecture and gates; task 053 is the final release decision.

## Consequences of interop

- **State roots require bit-exact MARF and bit-exact Clarity.** `state_index_root` is history-dependent three ways: back-pointer children hash to the *ancestor block hash*; the root is a Merkle skip-list over ancestor roots `root(N-1), root(N-2), root(N-4), …`; Node4/16/48 pointer arrays pack in *insertion order*. Every Clarity write must land with the same key, value and ordering.
- **clarity-wasm must be fixed.** The initial branch lacked complete
  `Epoch40`/`Clarity6`, cost and PoX-host behavior. Mainnet replay has since
  exposed additional host/compiler gaps. W6 and tasks 060/067/068 close these in
  clarity-wasm and nano-owned host code; the interpreter never closes them for
  production.
- **Mining needs both** — an empty tenure still mutates nonces, balances, tenure height and the MARF height keys.
- **clarity-wasm's API is clarity-crate-typed** (`GlobalContext`, `ContractContext`, `ContractAnalysis`, `VmExecutionError`), and it pulls `clarity`/`clarity-types`/`stacks-common` as git deps. Those types are used at the VM boundary, including `clarity_types::Value` (de)serialization, which is consensus-critical and inseparable from the VM.
- **Checkpoint at or after the 4.0 boundary** ⇒ no epoch transition ever runs, so
  `initialize_epoch_2_05 … 3_4` are not needed. Boot contracts arrive as imported
  state, but their historical analyses and deployment epochs remain consensus
  inputs when those contracts are called.

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
| `chainstate/checkpoint-H/` | attested export at the 4.0 boundary | trie/value state, published root, coherent ledger, block/tenure and burn/sortition context, maturity accounting, leader keys, chain/compiler identity |
| `stacker_set/cycle-N.json` | `/v3/stacker_set/:cycle` | reward set + weights |

**Oracle ladder**, cheapest first — a milestone uses the cheapest oracle that can falsify it:

1. **In-process stacks-core call** (pure functions, proptest) — no infra, milliseconds.
2. **Hardcoded vectors** lifted from stacks-core's own tests (pure data, clean-room safe).
3. **Offline fixture replay** — deterministic, CI-gated, no docker.
4. **Live hacknet RPC** comparison.
5. **Live interop** — our signature in their block; our block in their chain.

**Rules:** no component merges without its oracle test green; every milestone's
test stays in CI as a regression gate; M1–M7 need no running infrastructure at
all. stacks-core/interpreter calls stay in dev-only conformance artifacts. A
known ignored semantic differential is recorded as open, not green.

---

# Baseline and progress signal

## The walking skeleton comes first

Before any component is written, build the **whole pipeline as stubs** — burnchain ingest → sortition → block validation → execution → state root — wired end to end, with every stage returning `unimplemented!()` or a zero value. Point the replay harness at the fixtures and run it. It fails on block 1, immediately.

That failure *is* the baseline. From that moment every commit either moves the failure point later or doesn't, and the question "am I on track?" has a numeric answer instead of a vibe.

## The scoreboard

One command, `cargo xtask scoreboard`, runs every oracle and prints the state of
the world. Run on every commit and in CI. The table below is the **original
target shape**, not a current result snapshot:

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

**EXPANDED during mainnet work:** report three frontiers separately:

1. the clean replay frontier from a newly initialized, attested checkpoint;
2. the targeted diagnostic frontier reached from reflinked/resumed state; and
3. the contemporaneous network tip.

Only the first can satisfy the release gate. A downloaded/followed height, a
targeted compiler-fix resume or a state directory missing ledger/sortition inputs
must never be presented as replay depth.

## Critical path

Original implementation path: `M0 → M7 (MARF) → M8 (VM) → M10 (replay)`.

**EXPANDED release path:** `M10 → M11 (P2P-backed follow) → M14 (clean mainnet
release gate)`, with M12 signer/client interop and the unresolved VM differentials
feeding M14 in parallel.

The original schedule risk was concentrated in MARF and clarity-wasm. Mainnet
work added three release-path risks: complete checkpoint continuation, P2P
independence for every role, and stock signer/client interoperability. Codec,
address and crypto components remain independently verifiable, but green unit
surfaces cannot substitute for the assembled M14 run.

**Original halfway checkpoint:** M7b green (MARF + PCS import) and M8a/M8b
green. The amendment is that M7b now includes coherent continuation metadata,
not only the archival trie root.

## Tripwires

Pre-agreed, so nobody has to argue about sunk cost mid-flight.

| If… | Then |
|---|---|
| Conformance harness not compiling / fixtures not captured early | Stop. Do not start components — nothing downstream is verifiable without it. |
| clarity-wasm not compiling against develop+Epoch40 by ~¼ of budget | Stop the production milestone and use the interpreter only in separate, rolled-back diagnostic tooling to localize the compiler gap. The node never substitutes the interpreter: clarity-wasm conformance is a release prerequisite. |
| MARF lockstep red by ~¼ of budget | Bisect with node byte vectors before lockstep scripts. The cause is nearly always one of the four named traps: Node48 `indexes` in the preimage, omitted empty slots, insertion-order packing, or the ancestor skip-list. |
| hacknet 4.0 not producing blocks | Capture fixtures from the live internal pox-5 testnet instead; no infra to stand up. |
| Any result, error, cost, event or write differential remains red or ignored | **CHANGED:** do not release. Keep replay moving for diagnosis, but close and unignore the differential before task 053 can pass. |

---

# Milestones

Each milestone is independently falsifiable. The original “hours, not days”
estimate did not include P2P, full mainnet checkpoint continuation or the release
hold; current effort and ownership live in taskmd. `W`n refers to the component
specs below.

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
| **M7b** | complete checkpoint import (W5) | `checkpoint-H/` | root at H matches the publication; ledger and continuation inputs are coherent; extending through a tenure boundary matches stacks-core |
| **M8a** | clarity-wasm rebased to Epoch40/Clarity6 (W6.1) | its own crosscheck suite | existing suite green on the new epoch/version |
| **M8b** | Clarity 6 words (W6.2) | interpreter crosscheck | each new word matches the interpreter on random inputs |
| **M8c** | costs-4/costs-5 (W6.3) | interpreter crosscheck | all five cost dimensions match exactly on random snippets |
| **M8d** | nano-owned PoX native effects (W6.4, W7) | balance assertion | `pox-5.stack-stx` **moves locked STX**, not just map entries, without calling stacks-core production helpers |
| **M8e** | backing store over `nano-marf` (W6.5) | boot contract deploy | all boot contracts deploy; state root stable across reopen |
| **M9** | envelope validation + reward sets (W8) | fixtures + `stacker_set` | same `block_hash`/`signer_signature_hash` per block; accepts exactly what the network accepted; reward set matches |
| **M10** | **full execution** (W7, W8) | block headers + `new_block` events | replay from checkpoint: **`state_index_root` matches every block**, and every tx receipt (status, cost, events) matches |
| **M11 — CHANGED** | P2P-backed sync and live follow (W9, W12) | live hacknet + mainnet | discovers independent peers, catches up with no hosted API configured, attributes served tenures and tracks across ≥2 reward cycles incl. prepare/rollover |
| **M12 — EXPANDED** | StackerDB + embedded/hosted signer (W10) | live interop | a stock signer accepts a proposal through nano, and nano's signature lands in a stock miner's block; replication survives loss of its initial peer |
| **M13** | miner (W11) | live interop | our block is signed by stock signers and accepted by stock nodes; chain advances through a nano-won sortition |
| **M14 — NEW** | mainnet release gate | attested checkpoint + live mainnet | clean clarity-wasm-only checkpoint-to-tip run, no hosted API or skipped gate, restart/reorg/crash recovery, stock signer/client journeys and ≥24 h at tip |

**M0 was the first build milestone.** Nothing was verifiable before it existed.
Fixture capture may use a hosted test service as an oracle, including the internal
PoX-5 testnet, but M11/M14 production evidence may not configure that service as
a synchronization or liveness dependency.

**Dependencies:** M1 → M2/M3 → M4 → M5 → M6. M1 → M7a → M7b. M8a → M8b/M8c/M8d; M7b + M8e → M10. M4+M6+M7b+M8 → M9 → M10. M10 → M11; M10 + M11 + M12 → M14. M13 remains the mining/production branch and depends on M12. W13 (hacknet 4.0) runs parallel from the start and blocks only M0's fixture capture and M11+.

**M10 remains the first milestone that matters:** everything before it is a
component check, and M10 first proves nano-stacks computes the same chain state
as stacks-core. **CHANGED:** M10 is necessary but no longer sufficient for
release. M14 proves that the assembled binary can start from a complete mainnet
checkpoint, obtain data without a hosted service and remain live.

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
| `nano-vm` | `ClarityBackingStore`/`HeadersDB`/`BurnStateDB` over `nano-marf`; clarity-wasm-only driver; nano-owned native-effect boundary | 2500 |
| `nano-chainstate` | Nakamoto block/header types, signature hashes, signer-set verification, tenure rules, `append_block`, reward sets, staging | 3500 |
| `nano-p2p` | Stacks handshake/framing, discovery, peer DB/scoring, inventories, inbound serving and transaction/block relay | **NEW** |
| `nano-sync` | inventory-driven peer scheduler, per-peer HTTP tenure/block acquisition, local fork choice and restartable catch-up | **CHANGED** |
| `nano-stackerdb` | chunk format + signing, libsigner v0 `SignerMessage` codec | 1200 |
| `nano-signer` | embedded signer state machine, sortition/reorg checks | 1000 |
| `nano-miner` | bitcoin op construction, UTXO mgmt, block assembly, signer coordination | 2200 |
| `nano-rpc` | axum RPC subset + event dispatcher | 1200 |
| `nano-node` | config, wiring, event loop | 1000 |
| `nano-conformance` | **dev-only**: stacks-core oracles, fixtures, replay harness | — |

**~24k LOC** was the original sizing target, not a current measurement. P2P,
mainnet checkpoint continuation and full RPC/signer interoperability expanded the
scope; maintainability and production-dependency boundaries remain the metric,
not preserving an obsolete LOC estimate.

**Omitted after amendments:** Atlas/attachments, microblock production, cost
estimation, shadow-block production, stacks-core's mio HTTP stack and MARF Merkle
proof serving from squashed state. Binary P2P is **no longer omitted**. Historical
`at-block` behavior is **not omitted** either: old contracts remain imported and
must return epoch-4.0's exact runtime refusal. Bitcoin access is still through a
configured local RPC source rather than a new SPV implementation. Legacy
signer-voting/WSTS machinery and pre-waterfall multi-output PoX behavior remain
out of scope, while any corresponding imported contract state is preserved.

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

**Checkpoint continuation bundle — EXPANDED:** the trie is necessary but not
sufficient. Bind it to the executed ledger and chain identity, retained Nakamoto
headers/tenure accounting, canonical burn snapshots and PoX state, maturity
window, historical leader-key registry, stored contract analyses/deployment
epochs and compiler identity. Startup verifies that these pieces describe one
tip and refuses an incomplete or mixed directory without mutation. The release
oracle extends a newly initialized bundle through the first tenure boundary and
then to tip; opening the archival root alone does not satisfy M7b or M14.

## W6 — clarity-wasm to epoch 4.0 → M8a–M8e

Fork `stx-labs/clarity-wasm` and its `feat/clarity-wasm-develop` stacks-core branch.

1. **Rebase onto develop** (Epoch40 + Clarity6): `clarity/src/vm/clarity_wasm.rs` is ~400 KB and divergent; `ClarityVersion` gains `Clarity6`, `StacksEpochId` gains `Epoch40`, `default_for_epoch(Epoch40) = Clarity6`.
2. **Clarity 6 words**: `verify-merkle-proof` (Bitcoin double-SHA256 inclusion, hardened against CVE-2012-2459 inflated-`tx-count` forgeries), `get-bitcoin-tx-output?` (SegWit-aware, returns output N + witness-stripped txid), `ed25519-verify`, `secp256k1-decompress?`, variadic `concat` (currently fixed-arity). Also Clarity 4's missing `secp256r1-verify` and the Clarity 5 gaps. `with-stacking` → `with-staking`.
3. **Costs**: add `clar4`/`clar5` cost modules mirroring `costs-4`/`costs-5`; `BLOCK_LIMIT_MAINNET_40` doubles `read_length` and `read_count` (write and runtime unchanged).
4. **PoX native effects — CHANGED**: implement the required lock/unlock and
   accounting effects at clarity-wasm's contract-call boundary in nano-owned
   production code. stacks-core's `pox-locking` implementation is an oracle, not
   a production helper.
5. **Backing store**: implement `ClarityBackingStore` + `HeadersDB` + `BurnStateDB` over `nano-marf` and nano's headers/sortition DBs, replacing the dev-only `datastore.rs` (`developer-mode`-gated, full of `panic!`/`unreachable!`).
6. **wasmtime** off 15.0.0.
7. **Crosscheck**: clarity-wasm ships `crosscheck()`/`crosseval()` harnesses
   running a snippet through interpreter and wasm and asserting equality. Extend
   this only in separately built, rolled-back conformance tooling; the production
   node cannot call it. Any divergence blocks release, including an ignored case.

## W7 — Boot contracts and PoX locking → M8e, M10

Embed boot `.clar` sources byte-identically (~11k lines): `pox`, `lockup`, `costs`, `cost-voting`, `bns`, `genesis`, plus `pox-2/3/4/5`, `costs-2/3/4`, `signers`, `signers-voting`, `sip-031`. `pox-5.clar` is 3,851 lines. Non-mainnet needs `make_pox_5_body`'s textual substitution of the sBTC token literal and the bond/pause admin principals (`node.pox_5_sbtc_contract`, `pox_5_sbtc_registry_contract`, `pox_5_bond_admin`, `pox_5_pause_admin`).

Importing at/after the 4.0 boundary means no epoch initializer runs; contracts arrive as state. `.cost-voting` is disabled in 4.0 (SIP-044).

Reimplement the consensus-relevant `pox-locking` effects in nano-owned code
(stacks-core is the differential oracle): lock/unlock semantics are intercepted
on the contract-call boundary for pox-5 entrypoints. No production feature or
error path may dispatch to stacks-core's handler.

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

## W9 — P2P-backed synchronization → M11 **(CHANGED)**

Join the Stacks P2P network directly: authenticated handshake/framing, network and
chain checks, neighbor discovery, bounded inbound/outbound sessions, durable peer
knowledge, Nakamoto inventories, push/relay and liveness. Inventories schedule a
bounded forward download and shortlist sources; no peer claim may exclude another
candidate or choose the canonical burn height.

Bulk block bytes may still travel over a discovered peer's advertised HTTP data
URL (`/v3/tenures/*`, `/v3/blocks/*`). That is the Stacks peer data plane, not a
reason to configure Hiro. Use a scored pool with timeouts, backoff, 429 recovery
and failover, and record the serving peer per tenure. `/v3/sortitions` and fork
metadata received from a peer are hints or compatibility surfaces; canonical
sortitions and fork choice come from the local Bitcoin/burnchain view, validated
signatures and locally executed state.

The release run starts with `peers = []` (no hosted Stacks API), discovers peers
from P2P seeds, reconstructs maturity/history, catches up from the attested
checkpoint and holds tip. Signer/StackerDB and proposal-recovery loops use the
same failover discipline; proving only the chain downloader independent is not
enough.

## W10 — StackerDB + embedded/hosted signer → M12 **(EXPANDED)**

Chunk: `{slot_id, slot_version, data, sig}`, signed by the slot's writer key
(stacks-core's `libstackerdb` is an oracle). Replication uses authenticated
`GET/POST /v2/stackerdb/...` against the discovered/scored peer pool, not one
`SyncClient` captured at startup.

Contracts: `SP000000000000000000002Q6VF78.miners` — 2 slots, parity on `num_sortitions % 2`, writers are the block-signing Hash160s from the winners' leader-key registrations (`make_miners_stackerdb_config`). `.signers-{0,1}-{msg_id}` per message slot per cycle parity.

`MinerSlotID::{BlockProposal=0, BlockPushed=1}`; `MessageSlotID::{BlockResponse=1, StateMachineUpdate=2, BlockPreCommit=3}` — the contract index a message travels on, which is not its payload type byte (`SignerMessageTypePrefix::{StateMachineUpdate=6, BlockPreCommit=7}`). `SignerMessage::{BlockProposal, BlockResponse(Accepted(BlockAccepted)|Rejected(BlockRejection)), BlockPushed, StateMachineUpdate, BlockPreCommit, MockSignature, MockProposal, MockBlock}` with `RejectCode`/`RejectReason`.

Signer: read proposals from the miner slot, **fully validate** (envelope +
execution + state root), run local sortition/reorg checks, write
`BlockResponse::Accepted{signature}`. The checkpoint carries historical
leader-key registrations needed to validate committed VRF seeds; missing context
is a typed refusal, never an RPC lookup that turns a serving peer into consensus.

## W11 — Miner → M13

Bitcoin txs via `bitcoincore-rpc`: leader key register (`^`) and leader block commit (`[`), formats in W3. **Waterfall commits carry exactly one output equal to `sbtc_address`**; zero-amount rejected. UTXO selection from the configured wallet, fee bumping, `sendrawtransaction`.

hacknet wallet mechanics: pre-create the wallet with `descriptors=false` and import the miner address watch-only, mirroring `docker/bitcoin/miner.sh` — letting the node create its own wallet yields a descriptor wallet and breaks the setup.

Block assembly: tenure-change tx (or extend), coinbase with VRF proof, mempool txs under `BLOCK_LIMIT_MAINNET_40`, MARF seal → `state_index_root`, sign header. Coordination: write `BlockProposal` to `.miners`, accumulate `BlockResponse` weight to ≥70%, assemble `signer_signature` **in signer-index order**, push the block.

## W12 — RPC + event dispatcher → M11, M12, M14 **(EXPANDED)**

Serve `/v2/info`, `/v2/pox` (incl. `pox_5_sbtc_contract`, `pox_5_sbtc_registry_contract`), `/v2/accounts/:principal`, `/v2/contracts/call-read/...`, `/v2/transactions`, `/v3/block_proposal` (auth header), `/v3/stacker_set/:cycle`, `/v3/sortitions`, `/v3/tenures/{info,:block_id,tip_metadata,fork_info}`, `/v3/blocks/:block_id`, `/v3/blocks/upload`, `/v2/stackerdb/...`. Event observer POSTs: `new_block`, `new_burn_block`, `stackerdb_chunks`, `proposal_response`, `mined_nakamoto_block`.

Makes nano-stacks usable by stock `stacks-signer`, ordinary Stacks clients and
hacknet's tooling, and lets `consensus-test/monitor.ts` compare it by adding a
fourth URL. Hiro-compatible response shapes are an interoperability target, not
a runtime dependency on Hiro's service. Every response and event is built from a
coherent executed snapshot; admission alone is not reported as mining or
execution. Account/read-only RPCs do not invoke the reference interpreter.

## W13 — hacknet on epoch 4.0 → M0 fixtures, M11+

Add `STACKS_34_HEIGHT`/`STACKS_40_HEIGHT` anchors and `[[burnchain.epochs]] epoch_name = "3.4"/"4.0"` to `docker/stacks/stacks-miner_signer.toml` and the compose env block; pin images to stacks-core 4.0.1. Deploy `sbtc-token`/`sbtc-registry` stubs before the 4.0 boundary (pattern: `boot_to_epoch_4_0`, `stacks-node/src/tests/nakamoto_integrations.rs:1171`). Give `docker/stacker/stacking/stacking.ts` a pox-5 path — it hard-checks `contract_id.endsWith('.pox-4')` and would silently no-op, yielding an empty signer set, which is fatal.

Respect `validate_epochs`: `pox_5_activation_height == Epoch40.start`, in a reward phase at cycle offset > 1, and **3.0 and 4.0 must not share a reward cycle**. Rebuild the snapshot past activation: `MINE_INTERVAL_EPOCH3=10 PAUSE_HEIGHT=<n> make genesis && make snapshot`.

Add nano-stacks as an additive compose overlay on a free static IP (`.121`, host port `24443`); run it as a separate compose project joining `networks: default: {name: stacks, external: true}` so `make down` doesn't nuke it. Image needs bash+perl, a binary named `stacks-node` accepting `start --config`, config template at `/data/config.toml.in`, chainstate at `/data/chainstate`, clean SIGTERM. hacknet builds node images from a git branch, so local-source builds need their own Dockerfile.

Do **not** register nano wallets with the `bitcoin-miner` service: its on-demand trigger is `sum of confirmations across watched wallets == 0`, so joining that sum suppresses block production for the stacks-core chain. Fund nano miners with `sendtoaddress` from the existing `depositor` wallet. Burn blocks then arrive on the 30s timeout path.

---

# Risks

| Risk | Mitigation |
|---|---|
| clarity-wasm rebase across two epochs on a 400 KB divergent file | Longest lead time — start at M0 in parallel. Its own crosscheck suite is the gate (M8a); the interpreter is a test oracle only and is never a node execution path. |
| Cost parity (no costs-4/5 today) — divergence invisible until a block nears a limit | M8c asserts per-snippet dimension equality against the interpreter, not just block acceptance. |
| MARF bit-exactness | M7a lockstep against stacks-core's own MARF, before anything depends on it. |
| Clarity error identity is consensus-visible and hand-mapped in clarity-wasm | M10 asserts receipts (status, cost, events), not just state roots. |
| Nano-owned PoX native effects diverge from stacks-core | M8d asserts balances, locks, maps, receipts and roots; production dependencies are checked so the reference helper cannot satisfy the gate. |
| hacknet 4.0 on the critical path for fixtures | Parallel from M0. Fallback: capture fixtures from the live internal pox-5 testnet instead — no infra to stand up. |
| PCS root is valid but continuation metadata is incomplete or mixed | M7b verifies one coherent bundle and extends it through a tenure boundary; startup refuses torn/incoherent state. M14 starts from a newly initialized copy. |
| Empty signer set is fatal; hacknet's stacker no-ops under pox-5 | W13; assert non-empty reward set at startup. |
| Hosted API or one peer remains load-bearing despite P2P discovery | M11/M14 run with no hosted endpoint, retain per-tenure and per-role serving-peer evidence, and remove the active peer during sync and signer replication. |
| A short or targeted replay is mistaken for release evidence | The scoreboard separates clean, targeted and network-tip frontiers; only the clean attested checkpoint-to-tip run satisfies M14. |
| Stock signer RPC compatibility works but proposal validation lacks old leader keys | The checkpoint continuation bundle carries the registry; M12 requires a stock signer to accept and sign a proposal through nano. |
