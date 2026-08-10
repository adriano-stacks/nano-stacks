---
id: "078"
title: "Pin release builds and run mandatory gates in CI"
status: completed
priority: critical
effort: medium
type: chore
group: build
dependencies: ["032", "033"]
tags: ["build", "ci", "release", "reproducibility"]
created_at: 2026-08-07
completed_at: 2026-08-10
---

# Pin release builds and run mandatory gates in CI

## Objective

Make a clean checkout select one immutable toolchain and automatically run the
repository's required gates. A local cache or ignored lock file must not decide
which compiler produced release evidence.

## Tasks

- [x] Track `flake.lock` and pin `nixpkgs` to an immutable revision.
- [x] Pin an exact Rust toolchain and make the Nix shell and non-Nix setup select
      the same compiler, Cargo, Clippy and rustfmt versions.
- [x] Add repository-root CI for formatting, release workspace Clippy, the full
      conformance suite, the scoreboard and the release report's offline gates.
- [x] Require the scoreboard and release report commands to propagate failures;
      CI must not infer success by parsing a table.
- [x] Build the release artifact from the checked-out revision before hashing or
      inspecting it, then verify its embedded compiler identity.
- [x] Make `cargo fmt --all -- --check` clean for the workspace, including the
      vendored compiler sources the workspace owns.
- [x] Make release workspace Clippy warning-free and enforce warnings as errors.
      An exit status of zero with `items_after_test_module` or an unused import is
      not a passing lint gate under this repository's rules.
- [x] Separate ordinary offline CI from capture-backed release qualification.
      Every committed workflow job must have the inputs needed to become green;
      the release job must fail closed when its required capture is absent rather
      than making the default workflow permanently red.
- [x] Make CI reject every unowned ignored or conditionally skipped required test
      through [[085-eliminate-unaccounted-ignored-and-conditional-rele]].
- [x] Make `kill_during_import` deterministic in the complete release suite. It
      failed in the 2026-08-09 combined run and passed in isolation. Find the
      shared-state or timing cause, then retain a repeated-run regression.
- [x] Prove `nix develop` changes no tracked or ignored repository file in a clean
      checkout.

## Closed, 2026-08-10

The tracked Nix and Rust pins select rustc 1.97.1, Cargo 1.97.0, Clippy 0.1.97
and rustfmt 1.9.0. Root and vendored formatting checks pass. Release-profile
Clippy passes with `-D warnings` across every workspace target, including
benchmarks, and across every clar2wasm target. `actionlint` passes.

The offline workflow runs the bounded replay, fixture validation, full
conformance and unit suites, then requires the release report's explicit
non-qualifying exit status. Capture-backed qualification is a separate manual
self-hosted job with declared inputs. Release-report regressions prove that a
stale/missing compiler identity and a red scoreboard fail closed, while its
artifact is rebuilt before inspection. The ignored/conditional source inventory
is independently validated and names every owner.

`kill_during_import` now synchronizes on each persisted phase rather than racing
wall-clock sleeps; three consecutive exact runs each refused all 24 interrupted
imports. CI checks that `flake.lock`, the root `Cargo.lock`, and the vendored
`Cargo.lock` are byte-identical after all gates. Their hashes were unchanged by
the final local gate run.

## Historical audit, 2026-08-07

**Done.**

- `flake.lock` was **gitignored**, and `nixpkgs` pointed at the moving
  `nixos-unstable` branch -- so every `nix develop` re-resolved it and printed
  `updating lock file` on the way, and two clean checkouts a day apart could build
  release evidence with different compilers. The input is pinned to the revision the
  lock held and the lock is tracked. A second `nix develop` now prints nothing, which
  is the check for the last bullet as well.
- `.github/workflows/gates.yml` runs the offline gates on push and pull request:
  clippy over the release profile and every target, the bounded fixture replay, the
  fixture integrity check, the conformance suite, the unit tests and the release
  report. Nothing greps output -- [[075-make-the-consensus-scoreboard-an-authoritat]]
  made `scoreboard` and `release-report` propagate failure through their exit status,
  and a job that looked for a word would go green the moment the wording changed.
  The workflow also fails if a run rewrites `flake.lock`.

**Open, deliberately.** The formatting gate is present but `continue-on-error`, and
that is honest rather than convenient: the workspace has never been `cargo fmt`-clean
-- 86 files under `crates/`, 8 in the vendored compiler. Running it produces a
ninety-file mechanical commit, it reflowed a conformance test past clippy's
hundred-line limit on the attempt made here, and the vendored sources are being
edited concurrently under [[073]]. It has to land on a quiet tree as its own commit,
after which the `continue-on-error` comes off.

Still owed: building the artifact from the checked-out revision before hashing it,
and verifying its embedded compiler identity, which belongs with
[[074-make-the-release-report-readable-and-its-fixtures-b]]'s artifact bullets.

The audit found two more reasons the CI bullets are partial rather than done. The
release-report step is invoked without a capture even though that command explicitly
sets `NANO_REQUIRE_MAINNET=1` and says it is expected to fail without one, so an
ordinary checkout cannot finish the committed workflow. Clippy also exits zero
while emitting an unused `scoreboard_at` import and
`items_after_test_module`; warnings are reported but not enforced.

## Acceptance Criteria

- Two clean checkouts select the same Nix inputs and exact Rust toolchain without
  generating or rewriting a lock file.
- Root CI runs every required offline gate and blocks a deliberately introduced
  formatting, Clippy warning, replay or conformance failure.
- Ordinary offline CI has all required local inputs and can pass; capture-backed
  release qualification is a distinct mandatory job that cannot pass without its
  declared capture and services.
- Release artifact identity is tied to the checked-out source and compiler, not
  to a pre-existing `target/` entry.
- `cargo fmt --all -- --check` and release workspace Clippy both pass.

## Evidence that opened this task

The repository has no root CI configuration, ignores `flake.lock`, follows
`nixos-unstable`, and asks rustup for floating `stable`. Nix regenerated the
ignored lock during the audit, while `cargo fmt --all -- --check` failed across
vendored clarity-wasm and workspace tooling.

## The mainnet gates do run on this machine, 2026-08-07

`NANO_REQUIRE_MAINNET=1` turns every `skip_gate` into a failure, so a run that
claims the mainnet gates are green had to have run them. Run that way with no
inputs at all, the suite reports **22 failures, 21 of them "could not run"** —
which is the honest report and is what CI has been producing.

The inputs exist on this machine and were never wired up. With them:

```sh
CP=/home/aldur/mainnet-capture
ST=/home/aldur/mainnet-8716986/state/chainstate     # chain_id 1, no node on it, clean wal
NANO_MAINNET_CAPTURE=$CP \
NANO_MAINNET_RECEIPTS=/home/aldur/mainnet-receipts \
NANO_MAINNET_CHECKPOINT=$CP/chainstate/checkpoint-H \
NANO_MAINNET_MARF=$CP/chainstate/checkpoint-H/marf.sqlite \
NANO_MAINNET_ARCHIVE=/home/aldur/mainnet-chainstate/mainnet/chainstate/vm/index.sqlite \
NANO_MAINNET_STATE=$ST \
NANO_NODE_MARF=$ST/marf.sqlite \
NANO_NODE_CLARITY=$ST/clarity.sqlite \
NANO_MAINNET_JOURNAL=/home/aldur/mainnet-journal-8665602-8665607.txt \
NANO_MAINNET_BLOCKS=<concatenated capture/nakamoto/blocks/*.bin> \
NANO_REQUIRE_MAINNET=1 \
  cargo test --release -p nano-conformance --test conformance
```

**255 passed, 3 failed, 4 ignored**, from 236/22. What that buys is the gates
themselves, not the number: `mainnet_sortition` (five of them, deriving mainnet's
own window), `signer_weight_enforcement::every_captured_mainnet_block_authenticates`,
`mainnet_codec` decoding every captured block against stacks-core's decoder,
`tenure_continuity`, `burn_spends`, `mainnet_accounting` and
`mainnet_receipts::a_run_reproduces_the_frozen_mainnet_receipts` all ran against
real mainnet data rather than skipping.

`NANO_MAINNET_BLOCKS` wants a single concatenated stream and the capture stores
one file per block, which is why it read as a directory the first time.

### The seven that are left, and why supplying more variables would be dishonest

- `mainnet_checkpoint` ×3 and `write_journal::stacks_core_opens_a_mainnet_checkpoint_with_external_blobs`
  want `NANO_MAINNET_BLOCK`, `NANO_MAINNET_KEY`, `NANO_MAINNET_ROOT`. These are
  *parameterised investigations* — "tell me about this block, this key" — not
  fixed gates, and inventing values to make them run would be making a number go
  up.
- `trie_diff` wants `NANO_TRIE_PROOF`, `NANO_TRIE_STATE`, `NANO_TRIE_PARENT` and
  `NANO_TRIE_WRITES`: a specific recorded divergence, which is not one this
  machine holds.
- `write_journal::a_recorded_mainnet_journal_seals_the_chains_root` **ran** and
  failed on `UNIQUE constraint failed: marf_data.block_hash` — it writes the
  journal's block into a stacks-core MARF and the 56 GB archive already holds it.
  It needs a writable copy of the archive, not another variable.

`signer_weight_enforcement` had to be routed through `ChainState::open_existing`
first. It used `ChainState::open`, which would have created the directory on a
wrong path, adopted a network, appended an `engine_identity` row and left a WAL —
on an operator's real state. It also found that `/home/aldur/mainnet-pristine`
carries no `chain_identity` row at all, which the writable opener would have
silently adopted as mainnet.

## Tasks

- [x] Wire these variables into the release job so the mainnet gates run there,
      and fail the job when any required one is absent rather than skipping.
- [x] Decide which of the seven are gates and which are diagnostics, and stop
      counting the diagnostics as gates.

## Two gates were writing to the artifacts they were given

Found the hard way. `write_journal` is documented — in its own doc comment —
"Point it at a **copy**: stacks-core opens a MARF read-write, blob file and all."
Nothing enforced it, and a run here pointed `NANO_MAINNET_MARF` at
`mainnet-capture/chainstate/checkpoint-H/marf.sqlite`. stacks-core appended
**56,579 bytes** to the 229 GB `marf.sqlite.blobs` beside it before its
transaction rolled back on a `UNIQUE constraint`. The index was untouched
(mtime still 2026-07-31) and every byte it references is intact; what is there is
unreferenced tail. It has deliberately **not** been truncated — whether
stacks-core's next append uses the file length or the highest referenced offset
decides whether that is waste or self-correcting, and guessing on operator data
is what caused it.

Both halves are automatic now: `reflinked` makes a private copy with
`--reflink=always`, and `truncate_above` does the `DELETE FROM marf_data` the
test used to ask an operator to run by hand. Verified — the checkpoint's blobs
file is byte- and mtime-identical across a run that previously wrote to it. And
`a_recorded_mainnet_journal_seals_the_chains_root` **passes** now: nano's write
journal replayed into stacks-core's own MARF seals the same root.

`mainnet_checkpoint` had the same shape — `VersionedMarf::open`, which creates on
a wrong path and opens read-write — and `signer_weight_enforcement` had
`ChainState::open`. Both go through task 087's read-only openers now.

## And one near-miss worth recording

`every_checkpointed_contract_is_reachable_in_the_imported_trie` reported *"1 of
21 unreachable: SP4SZE…native-pool-v1"* against two independent nano states,
which reads exactly like a checkpoint-completeness defect. It is not one. Task
037 already records that `native-pool-v1` was **deployed at height 8,665,687**,
eighty-five blocks *above* the checkpoint at 8,665,600 — so it is legitimately
absent at the checkpoint anchor, and the gate needs a `NANO_MAINNET_BLOCK` that
postdates the deployment. Given the 8,716,986 state's own tip
(`63f2beda…`), all twenty-one are reachable and the gate passes.

Which surfaces a real limitation of the harness rather than of the node:
`write_journal::stacks_core_opens_a_mainnet_checkpoint_with_external_blobs` wants
`NANO_MAINNET_BLOCK` to be the *checkpoint anchor* while `mainnet_checkpoint`
wants one above it. One variable, two required values, so a single run cannot
satisfy both.

## What is left after all of that

- `trie_diff` wants `NANO_TRIE_PROOF`/`STATE`/`PARENT`/`WRITES`: a recorded trie
  divergence this machine does not hold.
- `p2p_discovery::nano_finds_mainnet_peers_to_fetch_from_without_a_hosted_api`
  reaches the live network and is not deterministic offline.
- `write_journal::stacks_core_opens_a_mainnet_checkpoint_with_external_blobs`,
  per the variable conflict above.
- `kill_during_import` is timing-sensitive: it needs three kills in four to land
  inside the import and gets fewer under parallel load. Seen failing once,
  green on three isolated re-runs and on clean full runs.
