---
id: "053"
title: "Pass the mainnet node release gate"
status: in-progress
priority: critical
effort: medium
type: improvement
group: mainnet
dependencies: ["027", "037", "049", "050", "051", "052", "054", "056", "057", "058", "060", "061", "062"]
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
- [ ] Catch up using a local Bitcoin source and multiple Stacks peers while
      recording every executed height and verified root.
- [ ] Restart during catch-up and at tip, then prove the same durable tip, root
      and tenure accounting are resumed.
- [ ] Inject failure and hard process termination at every block commit boundary
      and prove recovery exposes no partially committed block.
- [x] Retry a rejected block repeatedly and prove no durable or in-memory state
      changes before the accepted replacement arrives.
- [x] Remove and lie through one Stacks peer and prove neither event changes the
      canonical executed result.
- [ ] Exercise a Bitcoin reorganization and a Stacks fork switch.
- [ ] Run the stock signer/client-facing RPC and an event observer against the
      same executed chain.
- [ ] Hold mainnet tip for at least 24 hours across tenure and Bitcoin boundaries.
- [x] Publish the exact commands, versions, checkpoint provenance and resulting
      conformance report.

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
- The release node executes all Clarity work through clarity-wasm and has no
  interpreter path under any network, configuration, environment, role, build
  profile or failure condition. A compiler divergence cannot be hidden by a
  retry, crosscheck, fallback, healing step or emergency switch.

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
  and nothing handles it deliberately. Recorded for whoever owns `nano-marf`.

  So the binary-level restart needs either a state directory nothing is writing —
  a fresh import, which is hours — or a node stopped cleanly first, which the
  pristine run must not be. Neither is a code problem; both are wall-clock.
- **A Bitcoin reorganization and a Stacks fork switch** — [[026]] and
  `fork_retraction.rs` cover the retraction; the live event has not happened
  under a nano node.
- **A stock `stacks-signer` against the binary** — blocked on a pox-5 chain, not
  on nano: mainnet is on pox-4, so nano derives no waterfall reward set for the
  current cycle, no `signers-*` contract is configured, and no signer can hold a
  slot. Same blocker as [[050]]'s signer-weight check. See [[052]].
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
