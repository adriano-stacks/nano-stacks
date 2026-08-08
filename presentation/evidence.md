# Presentation evidence

This file records the evidence for the deck. Times are UTC. The snapshot date is
8 August 2026.

## Scope

- The Git snapshot for nano-stacks is `eac1f89dd277cd2dde93df5ddce97ee88c840e45`.
- The comparison snapshot for stacks-core is
  `efc34a07a225c4b950ab9404a1652aa5e14affaf`, the revision pinned in
  nano-stacks manifests and `Cargo.lock`.
- Dirty files in both repositories are not part of the code measurement.
- The task count excludes task 095, which tracks this presentation.
- Session metrics stop at `2026-08-08T15:17:53Z`. This is the start of the
  presentation session.

## Repository history

The first commit is `ac0f5aa2`, at `2026-07-27T17:18:58Z`. The snapshot commit is
`eac1f89d`, at `2026-08-08T15:39:07Z`.

The snapshot has 981 commits. It has 939 non-merge commits and 42 merge commits.
The Git author field names Claude on 734 commits and Codex on 247 commits. These
labels do not measure research, review, or uncommitted work.

| Date | Commits |
| --- | ---: |
| 27 July | 99 |
| 28 July | 185 |
| 29 July | 36 |
| 30 July | 117 |
| 31 July | 8 |
| 1 August | 15 |
| 2 August | 37 |
| 3 August | 94 |
| 4 August | 42 |
| 5 August | 100 |
| 6 August | 115 |
| 7 August | 114 |
| 8 August | 19 |

The largest subject groups are 224 `feat`, 180 `fix`, 173 `docs`, 75 `test`, and
45 `tasks` commits. Subjects also use component names. Thus, these groups do not
cover all work types.

The plan and task records show this sequence:

1. The first commits built primitives, codecs, MARF, and the walking node.
2. Hacknet work added the VM, signer, miner, RPC, and full-tenure mining paths.
3. Mainnet replay exposed incomplete costs, checkpoints, and continuation data.
4. HTTP limits made P2P a product requirement.
5. Mainnet state made local sortition and atomic adoption release requirements.
6. Live runs found storage, liveness, WASM, and evidence defects.
7. The task audit reopened claims that did not meet their acceptance criteria.

The architecture changes are in `plan.md`, lines 33-48. Important implementation
commits are:

| Commit | Result |
| --- | --- |
| `a9eabc20` | The interpreter no longer executes mainnet work. |
| `d95a616b` | The interpreter is outside the node artifact. |
| `65ceadaa` | Nano owns the PoX lock boundary. |
| `4a2e1ec3` | The node derives sortitions from Bitcoin. |
| `3785c1b3` | The node completes a mainnet P2P handshake. |
| `3b54dc24` | The node discovers, serves, and holds several peers. |
| `aeaf8ef3` | Rejected-block rollback is structural. |
| `eef1dfe4` | The ledger commit and block seal share one transaction. |
| `92f659cd` | Cargo uses at most four jobs. |
| `0cc14f60` | The compiler reuses scoped locals and measures them. |
| `f0d91c38` | The compiler frees locals after their last use. |
| `23196b51` | The compiler spills wide scopes to frame memory. |
| `d06bcdd6` | Four unfinished mainnet tasks reopen. |
| `a7181ac8` | Task 084 corrects a false boot-contract claim. |

## Code measurement

`tools/prod_loc.py` uses the Rust Tree-sitter grammar. It removes whole test,
bench, and fuzz paths. It also removes syntax items after `#[cfg(test)]`,
`#[test]`, `#[bench]`, `#[rstest]`, and equivalent direct test attributes. It
then removes blank and comment-only lines.

The nano scope has 19 product crates and the vendored `clar2wasm` library. It
does not have `nano-oracle`, `nano-conformance`, `xtask`, or the compiler command
line programs. The stacks-core scope has 13 product workspace crates. It does
not have contrib tools.

| Source | Rust files | Production lines | Test lines removed |
| --- | ---: | ---: | ---: |
| nano-stacks | 165 | 65,372 | 37,752 |
| stacks-core | 734 | 221,771 | 309,153 |
| Total | 899 | 287,143 | 346,905 |

Vendored `clar2wasm` has 29,815 production lines. Nano-owned product code has
35,557 production lines. Thus:

- stacks-core has 3.392 times the measured nano repository code;
- nano has 70.52% less measured repository code;
- stacks-core has 6.237 times the nano-owned code;
- nano-owned code is 83.97% less than the measured stacks-core code.

The largest nano-owned crate is `nano-node`, with 7,019 production lines.
Tree-sitter recovered from one nano file and two stacks-core production files.
The recovery points do not touch a test boundary.

This metric is not a dependency closure. Nano uses pinned Clarity frontend and
value types from stacks-core at the VM boundary. The metric measures source in
the named repository scopes.

The wrapper creates clean temporary Git archives at the two named commits and
measures both archives.

```sh
loc_python=$(nix build --no-link --print-out-paths --impure --expr \
  'import ./presentation/tools/loc.nix' | tail -1)
"$loc_python/bin/python" presentation/tools/measure_loc.py
```

The 8 August verification reproduced 65,372 nano lines and 221,771 stacks-core
lines at the two revisions above. Focused attribute checks covered plain tests,
qualified test macros with arguments, rstest, test-only `any` and `all`, and
production `not(test)` branches.

## WASM evidence

The product has one execution engine. Tasks 060 and 067 record the refusal
paths. Compile, module-load, host, and runtime failures do not start an
interpreter retry.

The snapshot has 102 commits that touch `vendor/clarity-wasm/clar2wasm`.

Task 073 records the compiler scale result:

- 137,332 of 137,340 imported contracts compiled in the first sweep;
- the highest live local count was 16,505;
- the Wasmtime limit was 50,000;
- eight contracts did not have a production-path verdict, so the task reopened.

Task 084 records the first sweep error: it compiled each module, but it did not
load the module into Wasmtime. A later sweep compiled and loaded 146,273 of
146,280 contracts. Seven refused. Two deployed user contracts reach the WASM
function arity limit. Other refusals have separate causes.

Task 093 classifies the first eight compile failures. Seven now load. The last
one belongs to task 068.

Task 086 records a run that started from block 8,708,125 and advanced to
8,708,625. It crossed the previously failing block 8,708,126. This is progress,
but it is not a causal conformance proof. The exact result, receipt, costs,
events, writes, and state root are still open acceptance items.

## Release state

Task 053 has 31 checklist items. Twelve are complete and 19 are open. The main
open conditions are:

- a clean checkpoint-to-tip run with no Hiro or other hosted API;
- compilation under each contract's recorded deployment epoch;
- stock signer acceptance, signature, and block inclusion;
- all known WASM semantic and module-load cases;
- the PoX-5 follower mismatch;
- reward-cycle 141 signer gates;
- a continuous 24-hour hold at tip;
- fail-closed scoreboard, report, and CI gates;
- no residual storage panic;
- local consensus across a reward-cycle boundary;
- a causal proof for block 8,708,126.

At the verification snapshot on 8 August, taskmd reported 94 tasks. Task 095 was
in progress during verification. After it is excluded, there are 93 project
tasks: 67 complete, 25 in progress, and one pending. Task 053 has 12 complete
and 19 open checklist items.

## Token measurement

`tools/session_metrics.py` reads local vendor logs. It includes direct
nano-stacks work. It excludes unrelated stacks-core security audits. The result
is recorded token traffic. It is not a bill, a cost estimate, or a count of
unique text.

| Vendor | Sessions | Counter coverage | Output | Total traffic |
| --- | ---: | --- | ---: | ---: |
| Claude | 13 | 69 contributing logs | 11,977,465 | 7,360,549,573 |
| Kimi | 1 | 14 agents | 598,599 | 467,835,323 |
| Codex | 14 | 9 sessions with counters | 3,351,018 | 1,550,439,639 |
| Total | 28 | vendor fields are not identical | 15,927,082 | 9,378,824,535 |

Claude records repeat one response for each content block and across resumed
sessions. The script keeps the last usage record for each request ID. It uses
the message ID only when the request ID is absent. It removes 12,617 repeat
usage rows. Its input parts are 46,566 uncached, 54,938,911 cache creation, and
7,293,586,631 cache read tokens.

The Kimi row covers the direct nano-stacks session and its 14 agents. Its input
parts are 3,131,764 other-input and 464,104,960 cache-read tokens.

The Codex row covers rollout sessions whose recorded working directory is this
repository. The script reads the last cumulative counter in each session. Nine
of 14 sessions have a usable counter. Its input field is 1,547,088,621 tokens.
Of these, 1,519,301,888 are marked as cached. The output field includes reasoning
output. The reasoning subset is 1,256,299 tokens.

Run:

```sh
python3 presentation/tools/session_metrics.py
```

The 8 August verification reproduced `9,378,824,535` combined recorded tokens.
The script parses local JSON records but selects and emits only timestamps,
repository identity, request/session identity, and vendor usage counters. It
does not select or emit message, prompt, tool, environment, or credential
bodies.

## Deck verification

The deck has no remote or embedded asset URI. The verifier loads it with DNS
disabled, exercises button, keyboard, and hash navigation, and checks every
slide for horizontal and vertical overflow. It also prints the deck and checks
the page count and aspect ratio.

```sh
nix shell nixpkgs#chromium nixpkgs#poppler-utils --command \
  python3 presentation/tools/verify_deck.py
```

The 8 August verification passed all 23 slides at 1280×720, 1366×768, and
1920×1080. The printed result had 23 pages at 960×540 points. `taskmd validate
--strict` accepted all 94 task records; with task 095 excluded, the board counts
were 67 complete, 25 in progress, and one pending.

## Human continuation ledger

This classification has two strict rules. A continuation prompt directly tells
an agent to continue, work, or work faster. An explicit goal command contains a
literal `/goal` command or instruction. The list does not infer mood.

There are 16 direct continuation or speed prompts:

| UTC time | Vendor | Exact user text |
| --- | --- | --- |
| 2026-07-31 16:43:40.472 | Claude | `what happened? you were running a goal and the session just crashed?` |
| 2026-08-05 14:09:28.902 | Claude | `but so what's going on? why aren't you working? are you slacking off? chop chop! we have stuff to do! you aren't done yet!` |
| 2026-08-05 14:13:00.479 | Claude | `yeah but why just one agent? why not more?` |
| 2026-08-05 15:18:33.505 | Claude | `you know you can use worktrees to split the work rather than being idle right?` |
| 2026-08-05 16:34:21.828 | Claude | `not good enough, make it go faster!` |
| 2026-08-05 16:42:46.348 | Claude | `you gotta be faster!!!` |
| 2026-08-05 17:03:59.437 | Claude | `continue, faster!!! you might also wanna compress your context right now` |
| 2026-08-05 17:09:43.400 | Claude | `faster!!!` |
| 2026-08-05 17:13:05.651 | Claude | `FASTER` |
| 2026-08-06 08:30:21.884 | Claude | `what what? continue!` |
| 2026-08-07 08:04:14.590 | Codex | `the agent is stopping again. evaluate the full thing, what's left, etc, and I'll assign things to a new agent` |
| 2026-08-07 08:36:49.387 | Codex | `continue please` |
| 2026-08-07 08:40:12.162 | Codex | `continue please` |
| 2026-08-07 09:43:14.379 | Claude | `fuck you have been oom again!` |
| 2026-08-07 12:21:55.480 | Claude | `why you haven't started? i gave you a goal: /goal don't stop until you can sync a mainnet nano-stacks and keep it synced. task 073 is in another agents' hands` |
| 2026-08-07 14:32:13.746 | Kimi | `continue` |

There are also 16 explicit goal commands. One is the initial Codex instruction
at `2026-07-27T16:09:25.992Z`. Fifteen are Claude `/goal` command records:

```text
2026-07-28T19:18:26.417Z
2026-07-29T12:33:17.461Z
2026-07-30T06:32:15.266Z
2026-07-31T16:50:34.628Z
2026-08-02T13:32:05.354Z
2026-08-05T08:35:32.332Z
2026-08-05T08:45:18.350Z
2026-08-05T18:53:24.456Z
2026-08-05T18:54:19.611Z
2026-08-06T07:58:01.312Z
2026-08-06T16:22:18.882Z
2026-08-06T18:46:58.992Z
2026-08-07T08:22:18.102Z
2026-08-07T14:38:16.477Z
2026-08-07T19:58:23.523Z
```

## Direct correction ledger

These user prompts correct a design, task, or evidence choice. Exact language is
kept here. The deck uses STE-normalized text.

| UTC time | Vendor | Correction |
| --- | --- | --- |
| 2026-07-28 09:15:09.445 | Codex | `i don't know what that means but the instructions are clear: you need to add what's missing to clarity wasm and use it here` |
| 2026-07-29 16:05:53.775 | Claude | `well no, /goal you need to mine every possible block within a tenure, implement tenure extend, and also fix that clar2wasm issue` |
| 2026-08-04 10:30:40.644 | Codex | `wait what? but it needs to use clarity wasm!!!` |
| 2026-08-05 17:33:36.546 | Claude | `who told you to create open issues and/or docs? we have tasks, and that's what you should be using you dumb` |
| 2026-08-05 17:34:12.952 | Claude | The user sent the same taskmd correction again. |
| 2026-08-05 17:34:38.324 | Claude | `so fix your mess!` |
| 2026-08-07 07:37:12.396 | Claude | `Do not treat elapsed-time requirements as blockers: keep live runs active while addressing the other workstreams.` |
| 2026-08-07 08:23:57.244 | Codex | `mmm but the chain itself has size limits on contracts, runtime, etc, so does there exist a contract that would deploy on the interprter but it is too big for wasmtime to execute?` |

## Agent error ledger

This ledger records explicit reversals in the plan, tasks, transcript, or commit
history. It does not label every normal implementation defect as agent
confusion. That would be subjective and not reproducible.

| Prior agent choice or claim | Evidence of failure | Correction |
| --- | --- | --- |
| HTTP can be the main sync path. | Hiro rate limits and one-source failure stopped sustained sync. See the plan amendment and task 054. | Implement P2P discovery, service, and failover. Commits `3785c1b3` and `3b54dc24`. |
| The interpreter can heal or retry WASM work. | A second engine can hide consensus divergence. See tasks 059 and 060 and the user corrections above. | Remove interpreter execution from the product. Commits `a9eabc20` and `d95a616b`. |
| Production can reuse stacks-core PoX locking. | The product boundary was not independent. See task 061. | Add a nano-owned PoX boundary. Commit `65ceadaa`. |
| A peer can supply sortition execution context. | A peer could choose a lagging or false view. See tasks 049 and 077. | Derive the view from local Bitcoin state. Commit `4a2e1ec3`. |
| A trie graph is a complete checkpoint. | Rewards, tenure proof, leader keys, and continuation state were absent. See the plan amendment and tasks 048, 051, and 057. | Bind the complete continuation history to the checkpoint. |
| A failed block can retry without other state changes. | One rejected block ran 1,417 times. Each run added the same tenure fee. See task 056. | Make block adoption and the ledger atomic. Commits `aeaf8ef3` and `eef1dfe4`. |
| The first contract sweep was a production-path verdict. | It called compile but did not load the module. See task 084, lines 74-104. | Add a compile-and-load sweep. It found seven refusals in the later state. |
| Task 073 was complete. | Eight mainnet contracts had no production-path verdict, and the arity result was open. See task 073, lines 151-160. | Reopen the task. Task 093 fixed seven of the first eight failures. |
| Tasks 064, 069, 082, and 083 were complete. | Their acceptance items were still open. | Reopen all four. Commit `d06bcdd6`. |
| A deployed arity failure was a boot contract. | The principal was a user contract with the same name. See task 084, lines 106-123. | Correct the record. Commit `a7181ac8`. |
| More parallel Rust builds will always be faster. | Concurrent rustc work exhausted CPU and memory. The session stopped. | Cap Cargo at four jobs and keep temporary state off tmpfs. Commit `92f659cd`. |
| Ad hoc issues and documents can replace task records. | The user gave the taskmd correction twice and then asked for cleanup. | Move work back to taskmd records. |
| A restarted run proves block 8,708,126 is fixed. | The run advanced, but the causal change and exact differential are not proved. See task 086. | Keep task 086 open and require the pre-fix/post-fix proof. |
| A successful tool run can write to the artifact that it checks. | The gate changed its own input and made the evidence stale. | Make the gate read-only. Commit `099a8c1a`. |

## Visual source

The cover image is `assets/modern-node.png`. The built-in image generation tool
created it. Its SHA-256 hash is:

```text
0d052a4b33d2ab131402d32e122641c9eb57cd97b933cfe518971cbc57a949f4
```

The generation prompt was:

```text
Use case: stylized-concept
Asset type: 16:9 presentation cover background
Primary request: A modern Stacks blockchain node shown as a compact cutaway machine, built from clean modular layers. It receives Bitcoin blocks from the left, exchanges peer-to-peer network signals around it, executes a glowing WebAssembly core at the center, persists an authenticated checkpoint below, and connects to signer and miner roles on the right.
Scene/backdrop: Very dark near-black field with generous negative space in the upper-left for a title.
Subject: One compact, precise technical machine with visible modular layers and thin data paths. The WebAssembly core is the visual focus.
Style/medium: Editorial technical illustration, restrained 3D/isometric forms mixed with fine schematic lines, high-end conference keynote aesthetic, not a literal server rack.
Composition/framing: Wide 16:9. Machine sits in the lower-right two-thirds. Data enters from left and exits right. Keep upper-left quiet and dark.
Lighting/mood: Crisp, confident, rigorous. Subtle cyan, warm amber, and electric violet accents on matte graphite.
Color palette: Near-black, graphite, off-white, cyan, amber, violet. No bright green.
Materials/textures: Matte metal, smoked glass, faint circuit traces.
Text: none.
Constraints: No words, no letters, no logos, no brand marks, no numbers, no UI screenshots. Keep paths clear and architecture legible.
Avoid: Cyberpunk city, humanoid robots, coins, cryptocurrency clichés, stacks of coins, generic cloud icons, clutter, neon overload, gradients that reduce text contrast, watermark.
```
