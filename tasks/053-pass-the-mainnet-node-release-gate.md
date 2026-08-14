---
id: "053"
title: "Pass the mainnet node release gate"
status: in-progress
priority: critical
effort: medium
type: improvement
group: mainnet
dependencies: ["011", "027", "037", "049", "050", "051", "052", "054", "056", "057", "058", "060", "061", "062", "064", "067", "068", "069", "070", "071", "073", "074", "075", "076", "077", "078", "079", "082", "083", "084", "085", "086", "087", "088", "093", "096", "097", "098", "106", "142"]
tags: ["mainnet", "conformance", "release"]
created_at: 2026-08-02
---

# Pass the mainnet node release gate

## Objective

No component result or peer-facing height is enough to call nano a mainnet node.
Exercise the assembled binary from a fresh, attested checkpoint through catch-up
and steady state, with evidence tied to the durable executed chain.

## Tasks

- [x] Bootstrap a clean state directory from an attested mainnet checkpoint.
- [x] Build and inspect the production node artifact to prove clarity-wasm is
      its only execution path. Interpreter fallback, crosscheck, healing and
      engine-selection code must be absent, not merely disabled for the run.
- [x] Force clarity-wasm compilation, module-load and runtime failures through
      the production boundary and prove each rejects without committing state
      or invoking the interpreter.
- [x] Catch up using a local Bitcoin source and multiple Stacks peers while
      recording every executed height and verified root — through the binary, over
      the captured chain and its own Bitcoin blocks; `NANO_TRACE_ROOTS` is how a
      mainnet run records the same thing.
- [x] Restart during catch-up and at tip, then prove the same durable tip, root
      and tenure accounting are resumed.
- [x] Inject failure and hard process termination at every block commit boundary
      and prove recovery exposes no partially committed block.
- [x] Retry a rejected block repeatedly and prove no durable or in-memory state
      changes before the accepted replacement arrives.
- [x] Remove and lie through one Stacks peer and prove neither event changes the
      canonical executed result.
- [x] Exercise a Bitcoin reorganization and a Stacks fork switch.
- [ ] Repeat the pristine checkpoint-to-tip catch-up with every Hiro and other
      hosted Stacks HTTP endpoint absent from configuration and the selected
      peer set. Retain the discovered endpoints and per-tenure serving peer so
      the run proves distribution rather than merely having several peers open.
- [x] Eliminate compiler-chosen historical epochs under [[064-compile-a-contract-under-the-epoch-it-was-deployed-]]
      and prove every rebuilt historical contract uses semantic epoch data from
      chain state.
- [x] Run an event observer against the executed chain and retain delivered
      block, burn-block and proposal-response payloads.
- [x] Run a stock signer and a valid client transaction end to end against the
      same executed chain: the signer accepts and signs a proposal validated by
      nano, and the submitted transaction appears in an accepted block and its
      `new_block` event.
- [x] Run the stock signer/client-facing RPC and an event observer against the
      same executed chain far enough to validate request and payload shapes.
      This checked item does not imply proposal acceptance or transaction mining.
- [x] Close every known clarity-wasm semantic differential, including
      [[067-reject-contract-call-through-a-constant-while-depl]] and
      [[068-resolve-asymmetric-tuple-least-supertype-semantics]]; an ignored
      crosscheck is a failed release gate. Also evaluate the 8 mainnet
      contracts that failed clar2wasm compilation in [[073]]'s margin sweep
      (2026-08-07): 3× `Not implemented` (`amm-swap003`, two `.pool`), 4×
      duck-typing buffer errors (`gated-pages*`), 1× `Tuples fields should be
      typed` (`trajan-endorsement-alpha`) — confirm each against the
      production `compile_under` path first; some may be sweep-harness
      artifacts.
- [x] Reproduce and close the PoX-5 follower root mismatch under
      [[069-resolve-the-pox-5-follower-state-root-divergence]] before using that
      signer run as interoperability evidence.
- [ ] Run the signer-set and signer-weight mainnet gates in reward cycle 141 or
      later; `NANO_REQUIRE_MAINNET` must report that they ran rather than skipped.
- [ ] Complete the dedicated mainnet tip hold under [[106]].
- [x] Publish the exact commands, versions, checkpoint provenance and resulting
      conformance report.

### Audit regressions opened 2026-08-07

- [x] Restore the bounded replay and make red scoreboard output fail under
      [[075-make-the-consensus-scoreboard-an-authoritative-gat]].
- [ ] Remove fail-open block authentication under
      [[076-refuse-blocks-when-consensus-authentication-inputs]].
- [x] Remove peer-derived consensus context under
      [[077-remove-peer-derived-consensus-execution-fallbacks]].
- [x] Make release evidence reproducible and mandatory under [[078]].
- [x] Remove residual MARF storage panics under [[079]].
- [x] Make the release report fail closed under [[074]]. The recaptured fixture
      carries authenticated history, missing or inconsistent history is rejected
      before replay and artifact evidence, and the checked-in release report gate
      is green against the authenticated capture.
- [ ] Cross reward-cycle boundaries using only locally derived consensus under
      [[082-cross-a-reward-cycle-boundary-with-a-locally-derive]].
- [x] Refuse contradictory or missing checkpoint winner-seed evidence under
      [[083-refuse-an-unrecoverable-checkpoint-winner-seed-bef]].
- [x] Eliminate network-valid WebAssembly function-type arity refusals under
      [[084-eliminate-wasm-function-type-arity-refusals-for-ne]].
- [ ] Account for and execute every required ignored or conditional gate under
      [[085-eliminate-unaccounted-ignored-and-conditional-rele]].
- [x] Execute the captured mainnet block at 8,708,126 and close its corrupted
      trait-principal ABI path under
      [[086-execute-mainnet-block-8708126-without-corrupting-i]].
- [x] Make read-only diagnostic evidence non-mutating and fail closed on a wrong
      state path under
      [[087-make-read-only-state-diagnostics-refuse-absent-or]].
- [x] Cross a same-sortition Stacks fork without mixing staged siblings or
      leaving restart state behind under [[096]].
- [x] Mirror recursive trait argument casting and refusal costs under [[097]].
- [x] Execute the 8,724,865 nested trait-reference call under [[098]].

### Audit coverage map

Every gap from the repeated 2026-08-07 audit has an open release dependency:

| Gap | Owning task |
|---|---|
| reward-cycle rollover stops local sortition derivation | [[082]] |
| missing signer, tenure, leader-key or VRF evidence accepts | [[076]] |
| asymmetric tuple runtime-value semantics | [[068]] |
| contradictory checkpoint winner seed samples against zero | [[083]] |
| valid-source locals, arity and unclassified mainnet compile failures | [[073]], [[084]] |
| MARF/storage failure becomes key absence | [[079]] |
| peer consensus execution fallback and adversarial proof | [[077]] |
| scoreboard result does not fully control release status | [[075]], [[074]] |
| stale artifact, misleading engine/differential report | [[074]], [[078]] |
| ignored, skipped or missing-input tests | [[085]] |
| mainnet block 8,708,126 failed principal reconstruction, then passed after rebuild without a causal proof | [[086]], [[060]] |
| read-only xtask inspection creates an empty state on a wrong path | [[087]], [[074]] |
| floating toolchain, formatting and Clippy warnings | [[078]] |
| no-hosted catch-up, stock signer/client and 24-hour tip hold | this task, [[054]] |

## Acceptance Criteria

- Offline mainnet replay and receipt gates are green before the live run starts.
- Every required mainnet test reports that it actually ran; a missing fixture or
  environment variable cannot be reported as a passing conformance gate.
- The executed tip, not the followed tip, remains within the documented sync
  bound and survives restart.
- Every accepted block passed local burnchain, signer, miner, VRF and state-root
  validation.
- Peer failure, peer equivocation and ordinary reorganization do not stall or
  fork the node.
- RPC responses and events describe the same durable executed state.
- Synchronization, propagation and consensus inputs do not require Hiro or any
  other hosted Stacks HTTP API.
- The no-hosted-API claim is demonstrated by a complete checkpoint-to-tip run,
  not only discovery, a short tenure sample or a run that also configured Hiro.
- The clean replay begins from a newly initialized, internally complete state
  directory. A reflinked divergence parent, targeted resume or checkpoint import
  missing its ledger or saved sortitions is diagnostic evidence only.
- No contract's semantic epoch is selected by trying compiler versions or
  epochs until one accepts it.
- The release node executes all Clarity work through clarity-wasm and has no
  interpreter path under any network, configuration, environment, role, build
  profile or failure condition. A compiler divergence cannot be hidden by a
  retry, crosscheck, fallback, healing step or emergency switch.
- Every known semantic differential is closed and unignored, whether or not the
  current mainnet window is known to exercise it.
- Signer-facing replication and proposal recovery continue after the initially
  selected peer is removed; chain synchronization alone is not the full
  no-hosted-API claim.

## A skipped gate can no longer report itself green

Most mainnet tests need a capture or a node's state directory, and skipping when
those are absent is right for a working tree. It is exactly wrong for a release
gate: **a suite where every mainnet test skipped looks identical to one where
every mainnet test passed**, and that difference is the whole question this task
asks.

`nano_conformance::skip_gate` is now what every one of them calls instead of
printing and returning. It prints the same thing normally, and panics when
`NANO_REQUIRE_MAINNET` is set — so a run that claims the mainnet gates are green
had to actually run them.

Demonstrated both ways, which is the point:

```
NANO_REQUIRE_MAINNET=1                          -> FAILED. 0 passed; 2 failed
NANO_REQUIRE_MAINNET=1 NANO_MAINNET_CAPTURE=... -> ok. 2 passed
(neither set)                                   -> ok, skipped
```

Sixty-two assertions across fourteen files route through it now, and
`cargo xtask release-report` counts them so the size of that conditional surface
is a number in the report rather than something a reader has to take on trust.

## The report is a command

`cargo xtask release-report [--capture <dir>] [--state <dir>] [--no-gates]`.

A document would be a claim; this is a measurement, and it makes exactly the
distinction above rather than describing it. It runs the conformance suite under
`NANO_REQUIRE_MAINNET=1`, so a green run is *by construction* one in which every
gate executed, and it separates a gate that **could not run** from one that ran
and failed — by reading `skip_gate`'s own panic message out of libtest's failure
blocks and grouping by the reason given. Without that split the report would say
the same thing about a missing environment variable as about a wrong state root.

Six sections: revision and toolchain, engines, artifact, checkpoint provenance,
scoreboard, and the gates. The one that took a decision is **engines**:
clarity-wasm is vendored in-tree rather than pinned as a git dependency, so its
revision is the *tree hash* of `vendor/clarity-wasm` — a content hash of exactly
the source that was compiled, which the repository's commit id is not. That is
[[060]]'s "record the clarity-wasm and compiler revisions", for the report half.

```
engines
  clarity-wasm         tree df143ad15d0bacebbdc660b2cd64dd07b4b28a76
  clarity-wasm change  a84adb08 fix(clar2wasm): an FT allowance need not name an existing token either
  wasmtime             15.0.1
  clarity              0.0.1 (stacks-core efc34a07a225c4b950ab9404a1652aa5e14affaf)
  stackslib            0.0.1 (stacks-core efc34a07a225c4b950ab9404a1652aa5e14affaf)
  interpreter          not linked into the artifact; see the gates below

checkpoint provenance
  state directory      /home/aldur/mainnet-wasm/state/chainstate
  format               stacks-core-marf-sqlite-v2
  stacks_height        8665600
  source_state_id      a87338900f279efc1b1df130004238cac8e09a2a4244fea39436fc66afae932d
  state_index_root     67596465d4a6642ad6fcec1df57c6ef758fcdb0003c7ed7f952e3ced1d7f44ec
  first_bitcoin_height 960231
  attesting_block_id   a87338900f279efc1b1df130004238cac8e09a2a4244fea39436fc66afae932d
  signer_weight        2708 against a threshold of 2599
```

That is also the first item on this task: the pristine run's state directory
descends from a checkpoint whose root a signed header endorsed at 2,708 weight
against a 2,599 threshold, and the report reads that back off disk rather than
being told it.

### An `inputs` section, and a rule about filling it in

Most mainnet gates take their fixtures from the environment, so a report printing
only its command line would describe a different run from the one it made. Every
`NANO_*` variable the run was given is printed, and `run_gate` inherits them —
which is how an operator hands the suite more fixtures without the report knowing
their names.

Filling that in has a rule, learned by breaking it. Handing the suite everything
this machine has took the run from *15 could not run, 0 failed* to *8 could not
run, **4 failed***, and none of the four was a defect: `NANO_NODE_MARF` pointed at
a state directory at 8,666,584 while `NANO_MAINNET_BLOCK` named the checkpoint's
state id at 8,665,600, so `every_checkpointed_contract_is_reachable_in_the_imported_trie`
reported one of twenty-one contracts unreachable and
`stacks_core_finds_the_contract_nano_cannot` — a *diagnostic*, not a gate — failed
reading a blob.

**A gate handed the wrong fixture is worse than one handed none**: it reports a
failure that means nothing, which is the same dishonesty as a skipped gate
reporting green, pointing the other way. So the report is run with the four inputs
whose pairing is unambiguous, and the rest are left absent and reported absent.

## The inputs that pair, measured

The report's rule -- "a gate handed the wrong fixture is worse than one handed
none" -- was re-learned the expensive way and is now written down as a map rather
than a warning. On this machine, with the fixtures that exist:

```
NANO_MAINNET_STATE       /home/aldur/mainnet-tip/state          46,626 executed from 8,665,600
NANO_MAINNET_ANCHOR      8665600
NANO_MAINNET_CAPTURE     /home/aldur/mainnet-capture            100 blocks from 8,665,601
NANO_MAINNET_RECEIPTS    /home/aldur/mainnet-receipts           5,112 nano new_block payloads
NANO_MAINNET_CHECKPOINT  .../mainnet-capture/chainstate/checkpoint-H
NANO_MAINNET_MARF        .../checkpoint-H/marf.sqlite           the *checkpoint's*, not a node's
NANO_MAINNET_BLOCK       a873389…932d                           the id that MARF was taken at
NANO_MAINNET_ROOT        6759646…f44ec
```

That set answers **247 passed, 0 failed** under `NANO_REQUIRE_MAINNET`, up from 230
before the fixtures were supplied at all.

Three inputs this machine has must **not** be handed over, and what each wrongly
reports is the reason to write them down:

| Input | Why not | What it reports |
|---|---|---|
| `NANO_MAINNET_MARF` = a *nano* state's `marf.sqlite` | the variable means the checkpoint's MARF; a nano state has no external blobs to open | `stacks_core_finds_the_contract_nano_cannot` and `stacks_core_opens_a_mainnet_checkpoint_with_external_blobs` fail on a blob read |
| `NANO_NODE_MARF` = `/home/aldur/mainnet-pristine` with `NANO_MAINNET_BLOCK` at the checkpoint | pristine has executed one block *above* the checkpoint, so its tip is not that id | `every_checkpointed_contract_is_reachable_in_the_imported_trie`: "1 of 21 unreachable: SP4SZE…native-pool-v1" |
| `NANO_MAINNET_JOURNAL` = `mainnet-journal-8665602-8665607.txt` with the checkpoint's MARF | the journal's parent is `df4decb0…`, not the block named, and the MARF already holds it | `UNIQUE constraint failed: marf_data.block_hash` |

`NANO_MAINNET_ARCHIVE=/home/aldur/mainnet-chainstate/mainnet` is a fourth: the
archive answers `disk I/O error` on open, so `pre_checkpoint_headers` fails for the
archive's sake and not nano's. Extract it again before using it.

None of the four is a defect in the node, and all four look exactly like one.

## The artifact, not the dependency graph

`wasm_is_the_engine` asks the sources and `cargo tree` whether an interpreter path
exists. That is the right question for a working tree and the wrong one for a
release: `cargo tree` describes an intent, and a `#[cfg]`, a monomorphization or a
trait object can put code into an executable that no line of source appears to
call. `one_engine_in_the_artifact` asks the binary.

Four questions, and the third decides.

1. **No interpreter entry point in the symbol table.** `clarity::vm::eval_all`,
   `clarity::vm::execute`, `initialize_versioned_contract`,
   `execute_transaction`, `execute_in_env`, `Environment::execute_contract` —
   none present. `OwnedEnvironment`'s methods are asserted as the *exact set*
   `{stx_transfer, new_cost_limited, commit}`: the native transfer path, which
   evaluates no Clarity and is the one stacks-core takes, and nothing else. An
   exact set rather than a deny-list, because a deny-list only refuses what
   somebody already thought of.
2. **No retired switch in the string data**, checked against
   `NANO_DUMP_REFUSED_WASM` being there, so an absent switch cannot pass as an
   absent string table.
3. **The interpreter's evaluator cannot be entered.** Its leaves *are* in the
   image and cannot not be: `clarity` is one rlib whose frontend, ABI types and
   cost machinery clarity-wasm consumes, and the linker keeps whole code
   generation units — so `clarity::vm::eval`, `apply` and the `special_*`
   builtins are all there as bytes. What matters is whether anything reaches
   them, and the disassembly says nothing does. `lookup_reserved_functions`,
   `clarity::vm::lookup_function` and `clarity::vm::apply` — every route from a
   function name to an interpreted implementation — have **zero reference sites
   in the whole executable**, no call and no address taken, and `eval`'s three
   references come from `special_let` and `special_map`, which are inside that
   same unreachable region.
4. **No configuration can select an engine.** `engine`, `interpreter`,
   `interpreter_fallback` and `crosscheck` are each refused by the shipped
   binary through `check-config`, against a baseline configuration asserted to be
   otherwise acceptable.

A fourth witness was tried and taken back out: `DefineFunctionsParsed::try_parse`
is how `eval_all` recognizes a top-level definition, but also how
`ArithmeticOnlyChecker` recognizes one, and its single reference site in the image
comes from the analyzer. The three that stay are witnesses precisely because the
analyzer has no use for them.

`objdump` is streamed and filtered rather than collected — 4.8 million lines, five
seconds, a handful wanted.

## All three ways the engine can refuse, forced

`engine_failure.rs`. None of the three needed a compiler bug, which matters: a
gate exercisable only while a divergence is open stops working when somebody fixes
one.

| class | forced by |
|---|---|
| compile refusal | a source naming a function that does not resolve |
| module-load refusal | a `let` with 60,000 bindings |
| runtime trap | `(- u0 u1)`, after a `var-set` |

The module-load case took finding. wasmtime's own limits are the only way to make
`clar2wasm` emit a module the runtime refuses without breaking `clar2wasm`, and
most are out of reach: a function's parameters are capped at **256 by Clarity's
analyzer**, far below wasm's 1,000, and a module cannot be planted as bytes
because contract metadata is written once. Locals are reachable, because a `let`
binding becomes one and nothing between the source and the validator counts them.
It comes back as `loadable`'s own message: `compiles to a module that will not
load: too many locals: locals exceed maximum (at offset 0x16d5)`.

Each class is forced on **both** paths into a refusal, not only the deploy. A
compiler gap on a running node has the contract already in state — the 8,668,161
shape — so `plant` writes one through the same three database writes a deployment
makes, and the call into it finds something indistinguishable from a deployed
contract. Each is repeated twenty times, because a node retries a block it cannot
execute for as long as it runs, and each is measured against the *pending* root so
"left nothing behind" is measured rather than inferred.

**The positive control is what makes the rest mean anything.** The interpreter
compiles nothing, so it deploys and runs the 60,000-binding contract without
complaint. Same source, same state, one engine answers and the other refuses — a
manufactured compiler gap — and nano's answer is no. Without it every assertion
in the file is consistent with a boundary that refuses everything.

### A compiler gap at a call is invisible in the sealed root

Found while writing the above, and it decides which gate catches a compiler gap.

A compile refusal at a **call** is reported as a failed transaction rather than as
a refusal to execute the block, deliberately: a deployment naming a function that
does not exist is an ordinary failed mainnet transaction and has to stay one. But
a failed transaction writes nothing, so it seals the root an untouched block
seals — and the root a legitimate `ArithmeticUnderflow` seals. **The state-root
check cannot tell a compiler gap from an abort the network also made.**

Receipts can. That is the argument for the receipt gate being non-optional, and
for `nano-vm` eventually telling a refusal at a call — which can only ever be a
gap, because the network accepted the contract once — from one at a deploy, which
the network makes too. Recorded on [[060]].

## Peer failure and equivocation

`peer_equivocation.rs`, four fake HTTP peers on loopback over `fixtures/mainnet`'s
real blocks and cycle-140 reward set. Offline, no capture, no environment
variable, so this gate cannot skip itself.

- A peer offering the tip with `chain_length` bumped by a thousand **loses** to
  the honest one. Length is what a naive fork choice compares and this lie is
  genuinely more attractive on it — asserted, so the test cannot pass because the
  liar offered something worse. The signatures cover `chain_length`, so they
  recover to keys the reward set does not hold.
- **A node left with only the liar follows nothing.** The half that a "the honest
  peer wins" test cannot reach, and the one the acceptance criterion turns on:
  stalling is visible and recoverable, following is a fork.
- Distrusting either of two honest peers leaves the same canonical tip. A fork
  choice that depended on which peer answered would reorganize when a peer
  restarts.
- A peer answering `/v3/blocks/:id` with a *different* block changes nothing.
  Recorded because `SyncClient::block` does not check that what came back is what
  was asked for — worth knowing rather than assuming away. It is not exploitable:
  whatever comes back is still weighed and still compared on length, so a
  substitute can only be a block the network *did* sign, and offering a lower one
  makes the liar less attractive.

What this does not cover is the same thing happening to a running node across a
Bitcoin reorganization, which is the "exercise a Bitcoin reorganization" item and
needs the live run.

### The run

```
$ export NANO_MAINNET_CAPTURE=/home/aldur/mainnet-capture
$ export NANO_MAINNET_CHECKPOINT=$NANO_MAINNET_CAPTURE/chainstate/checkpoint-H
$ export NANO_MAINNET_BLOCKS=/tmp/nano-mainnet-blocks.bin      # cat capture/nakamoto/blocks/*.bin
$ export NANO_MAINNET_ARCHIVE=/home/aldur/mainnet-chainstate/mainnet/chainstate/vm/index.sqlite
$ cargo xtask release-report --state /home/aldur/mainnet-wasm/state
```

```
gates
  Each command below also inherits every variable under `inputs`.

  cargo test --release -p nano-rpc -p nano-node
    pass   ok. 17 passed; 0 failed …; ok. 23 passed; 0 failed; 1 ignored …

  NANO_REQUIRE_MAINNET=1 NANO_MAINNET_CAPTURE=… cargo test --release -p nano-conformance --test conformance
    FAIL   FAILED. 164 passed; 12 failed; 2 ignored; finished in 64.46s
           12 gate(s) could not run, so the run is not evidence for them:
             1 × NANO_MAINNET_MARF and NANO_MAINNET_BLOCK are needed
             2 × NANO_MAINNET_STATE and NANO_MAINNET_CAPTURE name a state directory and a capture
             1 × NANO_NODE_MARF, NANO_NODE_CLARITY and NANO_MAINNET_BLOCK are needed
             1 × NANO_NODE_MARF, NANO_NODE_CLARITY, NANO_MAINNET_BLOCK and NANO_MAINNET_KEY are needed
             1 × NANO_P2P_MAINNET must be set to dial mainnet
             1 × NANO_TRIE_PROOF, NANO_TRIE_STATE, NANO_TRIE_PARENT and NANO_TRIE_WRITES are needed
             5 × the capture has no leader key for the winning commitment

  cargo clippy --release --workspace --all-targets
    pass   Finished `release` profile [optimized] target(s) in 22.24s
```

**164 passed, 12 failed, and every one of the twelve is a gate that could not run
— none ran and failed.** That is the whole point of the exercise: the run is
evidence for 164 assertions and explicitly not evidence for twelve, and the twelve
say why. The exit status is non-zero, which is correct: a release cannot be cut on
a run with twelve unrun gates.

Five of the twelve are not an environment variable at all but the capture's own
content — `/home/aldur/mainnet-capture` holds no leader-key registration for the
commitment that won its tenure, because the key was registered before the
burnchain window the capture covers. `p2p_discovery` and `trie_diff` are live and
diagnostic respectively. The remaining five want a state directory paired with the
block identifier it stands on, which this machine has separately and not together.

## What is proved, what is staged, and what needs wall-clock

The distinction this task exists to make, applied to itself.

**Proved, offline, in CI:**

- the artifact has no interpreter entry point and no edge into the interpreter it
  does contain (`one_engine_in_the_artifact`, four tests)
- all three engine-refusal classes reject, answer nothing and leave no state, on
  both the deploy and the call path, twenty retries each, with a positive control
  (`engine_failure`, six tests)
- a rejected block retried twenty-five times leaves the ledger and the accounting
  byte-identical (`rejected_blocks`, and `nano-chainstate`'s own witness that
  bites on fees)
- peer removal and peer equivocation change nothing the node follows
  (`peer_equivocation`, four tests)
- the checkpoint the pristine run stands on is attested, and its provenance is
  read back off disk by the report
- the report itself, and the fact that a skipped gate cannot report itself green

**Set up and waiting on wall-clock or on another chain:**

- **Holding mainnet tip for 24 hours** — not attempted, not claimed. The pristine
  run is at 8,679,076 with no state-root mismatch and roughly 30k blocks staged
  ahead of it; it is a catch-up, not a tip-hold.
- **Restart during catch-up and at tip, and hard kill at every commit boundary**
  — `restart.rs` and `kill_during_replay.rs` prove the invariants at the library
  level, including twenty `SIGKILL`s a run, and `kill_during_import.rs` proves the
  same across an import. What the gate asks for and they do not give is the
  *assembled binary* rather than the library.

  Attempted this session and abandoned for a reason worth writing down. The
  obvious cheap route is a `cp --reflink=always` of the pristine run's 45 GB state
  directory and a second node started on the copy. **That copy is not a
  snapshot.** A reflink copy is per-file atomic and says nothing across files, and
  a running node's `marf.sqlite`, `clarity.sqlite`, `staging.sqlite` and
  `accounting.json` are four files it writes in sequence — so the copy caught a
  ledger that had reached 8,679,483 and a trie that had not:

  ```
  thread 'main' panicked at crates/nano-marf/src/lib.rs:1155:
  trie storage: Storage("trie storage is missing block 8679483")
  ```

  That is the copy's fault, not the node's, and it is the *right* thing to notice
  — but it **panics** where it should name the inconsistency and refuse, which is
  a difference between a node that can be diagnosed and one that cannot. A hard
  kill of the node's own process leaves a consistent directory, which is what
  `kill_during_replay` proves; a torn copy of somebody else's is a separate case
  and nothing handles it deliberately. Tracked separately in
  [[065-reject-inconsistent-state-directories-without-pani]]; it does not
  invalidate the real crash-recovery evidence in [[057]].

  So the binary-level restart needs either a state directory nothing is writing —
  a fresh import, which is hours — or a node stopped cleanly first, which the
  pristine run must not be. Neither is a code problem; both are wall-clock.
- **A Bitcoin reorganization and a Stacks fork switch** — [[026]] and
  `fork_retraction.rs` cover the retraction; the live event has not happened
  under a nano node.
- **A stock `stacks-signer` against the binary** — cycle 140 was prepared under
  pox-4, so nano derives no waterfall reward set for that cycle even though
  Epoch 4.0 and pox-5 are active. Pox-5's first mainnet reward cycle is 141;
  run the gate there and close the remaining signer-facing fields in [[052]].
- **Recording every executed height and verified root during catch-up** — the run
  logs a root every 500 blocks, not every block.

## The panic on a torn state directory: what the obvious fix is not

The release-gate run found that a reflink copy of a *running* node's state
directory is not a snapshot — reflink is atomic per file, not across a directory —
and that opening one makes `nano-marf` **panic** inside the trie walk rather than
naming the inconsistency. An operator holding a torn copy deserves the sentence.

The obvious fix was tried and is wrong. A check at open — "refuse a state whose
side store names a block the trie never sealed" — was written, and
`a_crash_between_the_two_boundaries_leaves_the_parent_and_its_ledger` and
`a_kill_between_the_two_durability_boundaries_leaves_the_complete_parent` both
failed against it immediately. That is the point: **a crash between the ledger
write and the MARF seal leaves exactly the same shape**, and it is the shape
recovery exists for. The torn copy and the legitimately half-committed state are
indistinguishable by that comparison, so refusing one refuses the other, and
refusing the other is refusing to recover from a kill.

Reverted rather than left in, and this is why. The real fix is at the other end:
`nano-marf` has seventeen `expect("trie storage")` sites on the read path, and a
lookup for a node the trie does not hold has to return an error naming the block
and the node rather than unwinding. That is an API change across `nano-vm` and
`nano-chainstate` and belongs on its own, not smuggled into a release gate.

Recorded here rather than fixed because the wrong fix is cheap to reach for and
the tests that refute it are not obvious ones to run.

## The binary, killed nine times

`binary_restart.rs`. The item this task kept deferring for want of "a state
directory nothing is writing" — and the answer was to *make* one rather than to
copy somebody's: the captured 340-block chain served as two loopback Stacks peers,
its Bitcoin blocks served as a fake Esplora on the two endpoints
`BitcoinRestSource` actually reads, and a fresh state directory imported from
`fixtures/chainstate/checkpoint-H`. It stands up in about a second and the whole
test runs in 46, offline, with no capture and no environment variable — so this
gate cannot skip itself either.

Nine `SIGKILL`s. Each run waits for a height **above** the one the run before it
reached, which is what makes it failure injection at every block commit boundary
rather than nine kills at the same one: every kill interrupts work that process
did. `max_sync_blocks = 1`, so a round is a block.

Four assertions after each kill, and they are four different claims:

| claim | why it is separate |
|---|---|
| the state directory **opens** | a torn write recovery cannot read is a node that never comes back, which is what "no partially committed block" means in practice |
| its tip is a block of the chain the peers serve | never a block nobody signed, never a half-written one |
| the tip never goes backwards | a restart that lost a block would look like progress on the next round |
| the tip is at most one block behind the height the node reported | the parent-or-child shape a crash between the ledger write and the MARF seal leaves, which is the shape recovery exists for |

Then a tenth start reaches the served tip. That is the root check for every block
below it, because the executor refuses a block whose state root differs from its
header — reaching the tip is not evidence *about* the roots, it is the roots
having matched.

The node's own account of the run, which is the part worth reading:

```
executed 2 blocks, 461 to 463, 9 staged, state root 2cb30d9f…
derived the reward set for cycle 18 from this node's own state: 3 signers, 30 of weight
resuming …/chainstate from the state on disk, sealed at block adce5336… of height 463
recovered the ledger committed with block adce5336…: 3 executed blocks to walk back
  over, tenure 121 starting at height 462, parent tenure proof present
fetching history from 2 peers: http://127.0.0.1:40171/, http://127.0.0.1:33309/
following http://127.0.0.1:33309/ now, of 2 peers known
```

Ten process starts, the checkpoint imported once and resumed nine times, the
ledger recovered *with its tenure start heights and parent tenure proof* every
time, and the reward set derived from the node's own state at each start. Both
peers in the pool, and the first round re-weighing them rather than keeping
whichever answered first.

**What it does not cover**, said plainly: a kill during the *checkpoint import*
(`kill_during_import` has that at the library level), a state directory somebody
else is writing (still unhandled, and still the panic recorded above), and a
mainnet-scale state, where the import is hours rather than a second and the
recovery walks a ledger of 8.6 million blocks rather than eleven.

## A Bitcoin reorganization and a Stacks fork switch, through the round

Both now run end to end, in `follow_path.rs`, over the same offline chain and
through `CheckpointExecutor::catch_up` — the loop a running node runs.

**The Bitcoin reorganization.** `check_burnchain` asks the burnchain once a round
whether the block behind its sortition chain's tip still holds. One request when
nothing moved; the walk of `find_fork` only when the answer differs. Then
`retract_above`, `BitcoinSource::invalidate_from` (new on the trait — the node
could not reach either source's own copy through the trait it holds them behind,
which is why [[049]] recorded this as unwired), `ChainState::retract`, staging
cleared, and the executor stood on the surviving block. The test moves the block
the last executed tenure was elected in: one sortition retracted, two Stacks
blocks given back, the surviving chain a prefix of the one executed.

**The Stacks fork switch.** A peer whose burn view parted from this node's, served
over HTTP with a `fork_info` of its own. The round fetches its blocks, executes
none, and takes that as the question: `/v3/tenures/fork_info` against the tenures
this node executed, standing on the last block of the one they agree about.

Writing them found the bug that would have made both look like they worked:
**neither the fork switch nor a retraction moved the executor's own tip.** The
ledger rewound and the executor kept standing on the block it had just abandoned,
so nothing staged was ever its child and no round after a switch executed anything
— the stall the switch exists to remove, one step further along. It is
`stand_on_block` now, which fetches the surviving block and *checks its identity*
rather than trusting the answer.

Neither of these is the live event, and this is the distinction this task exists to
make: what is proved is that the node's own machinery notices and recovers, on real
blocks and a real burnchain source, offline and in CI. A mainnet reorganization
under a running nano node has still not happened.

## Two peers, one of them serving a coherent wrong chain

The other half of [[027]], and the case `peer_equivocation` could not reach: not a
malformed message but a **coherent alternative history** — well-formed blocks,
linking to each other, real transactions, real Merkle roots, real state roots,
eleven blocks longer than the honest peer's, and belonging to no chain the reward
set signed.

It is refused twice over, and the control is what makes that mean something: on
length alone the liar wins (asserted), weighed against the set `.signers` records
the honest peer wins, the follow path executes none of what the liar serves, and
the same checkpoint follows the honest peer to its tip. The round *fails* rather
than quietly executing nothing, which is what sets `peer_failed` and makes the
next round weigh the pool again.

### Recording every executed height, and where the record comes from

`NANO_TRACE_ROOTS=1` makes every executed block name itself: its height, the burn
block it stood on, its identifier and the root its header commits to. A switch and
not the default, because a mainnet catch-up would print thirty thousand lines an
operator has to read past to find the one that matters — a round already reports
the height it reached and the root it sealed.

The root printed is the **header's**, deliberately. The seal has already refused
the block if the two differed, so the line is the verified root rather than a
second opinion computed at printing time, which would invite a reader to believe
the check happens there.

`binary_restart` asserts one such line for every block between the anchor and the
tip, read out of the node's own log rather than the test's bookkeeping: what the
gate asks to be recorded is what the node said.

## What is still open, and what each one waits on

- **The no-hosted checkpoint-to-tip replay** — the clean replay and P2P-only
  discovery gates are individually green, but the final release run must still
  bind them to the same current artifact and retain its serving-peer history.
- **Holding mainnet tip for 24 hours** — not attempted and not claimed. It needs
  wall-clock evidence from the current release candidate under [[106]].
- **The mainnet signer gates and full infrastructure inventory** — mainnet is now
  in cycle 141, but every required conditional and ignored gate must run together
  under [[085]] rather than being inferred from the cycle number.
- **The whole post-rollover cycle** — the live node crossed 140 to 141 correctly;
  [[082]] remains open until the comparison covers the complete following cycle.

## The client-facing surface, driven by a stock signer

Run on Hacknet at epoch 4.0 with PoX-5 active, because that is where a waterfall
reward set exists and therefore where a stock signer can hold a slot at all. nano
was the *node* half of participant 3 and a stock `stacks-signer 4.0.1` was the
signer half, with nano's RPC as its only node:

```
hacknet/harness.sh host 3
hacknet/harness.sh verify-hosted
```

What ran and what it reported is written up in full in
[[052-wire-the-complete-rpc-and-event-surface-into-the-n]] under "The three halves,
on a pox-5 chain". The short account:

- the stock signer registered for the reward cycle **nano derived from its own
  pox-5 state**, held its slot, and had 32 chunks taken from it over nano's own
  `POST /v2/stackerdb/…/chunks` — asserted from the events nano dispatched for
  them, not from nano's replica, because nano also pulls its peer's chunks into
  that replica;
- it drove eleven distinct routes, listed there, recorded by nano itself under
  `NANO_TRACE_RPC=1`;
- seven shape defects were found by pointing the real binary at nano and fixing
  what it refused. No shim was added; each was nano answering with something
  stacks-core's own reader rejects.
- an observer received 632 `new_block`, 109 `new_burn_block` and 32
  `stackerdb_chunks` events, and for every block both nano's observer and the stock
  nodes' were told about, the per-transaction receipts — status, result and
  execution cost — are identical;
- a transaction posted to nano's `/v2/transactions` was admitted (`200`), and is now
  relayed onward rather than kept in a pool only nano's own miner reads.

`tests/conformance/hosted_signer.rs` is the gate, through `skip_gate`, so a run
without the environment cannot report itself green.

Those two historical blockers are now closed. On 2026-08-13, commit `f787569e`
ran the same `hacknet/harness.sh verify-hosted` gate against a fresh observer
window. The hosted stock signer accepted block
`baf79d318118c60b6a8df6da3d3302a6109e9d7b6be84380bdc0587f2e92f90b` through
nano. A transaction posted to nano (`8637fe84…25ae`) was relayed, mined and
delivered to nano's observer. The observer received 113 blocks, one burn block
and 921 StackerDB events, and every transaction receipt in all 113 blocks shared
with stacks-core matched on status, result and all five execution-cost fields.
The exact module result was `3 passed; 0 failed`.

This closes the stock-signer/client task item. It remains Hacknet evidence, not a
substitute for the mainnet hold or the complete release-qualification run.

## Two gates that cannot pass until the chain crosses cycle 141

Running the suite with every mainnet input supplied — the capture, a state directory,
the observer's receipts, the checkpoint — turned every skipping gate green except two,
and the two say something about the chain rather than about nano:

`signer_weight_enforcement::mainnet_blocks_pass_the_check_against_mainnet_state` and
`the_mainnet_state_carries_the_signer_set_mainnet_published` both need the state to
record a signer set for the cycle it stands in. **Mainnet cycle 140 has none and
cannot**: it was stacked under pox-4, so the block that wrote its `.signers` entries
is below the checkpoint. `check_signer_signatures` reports that absence and accepts,
deliberately, because rejecting would refuse every block of the chain the network is
on — and every block replayed so far is inside cycle 140. Cycle 141 opens at burn
962,150, about 880 Bitcoin blocks past the tip this was written at.

They `skip_gate` with that reason now instead of failing on an assertion that reads
like nano's fault. `NANO_REQUIRE_MAINNET` still turns them into failures, which is the
point: a release run may not claim these green while the chain it stands on cannot
answer them.

What the same run *did* close, with the inputs supplied rather than absent: the
receipt slice reproduced against the observer's 4,982 payloads, the checkpoint's
maturity window, the envelope and weight checks against captured mainnet blocks, and
the sortition window field for field.
