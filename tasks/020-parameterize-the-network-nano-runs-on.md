---
id: "020"
group: build
title: "Parameterize the network nano runs on"
status: completed
priority: critical
effort: medium
type: improvement
dependencies: []
tags: ["mainnet", "consensus"]
created_at: 2026-07-30
completed_at: 2026-07-30
---

# Parameterize the network nano runs on

## Objective

nano is not configured for testnet, it is compiled for it. The network a node
executes against has to be an input, because every one of these constants is
consensus-visible on mainnet.

The hardcoded sites:

- `mainnet: false` at all eighteen `GlobalContext::new` calls in
  `crates/nano-vm/src/lib.rs`
- `CHAIN_ID_TESTNET` on the production evaluation paths
  (`crates/nano-vm/src/lib.rs:29`)
- `boot_code_id(name, false)` in `crates/nano-chainstate/src/signers.rs:192`
- `BOOT_ADDRESS = "ST000000000000000000002AMW42H"` in
  `crates/nano-chainstate/src/lib.rs:74`, and the same literal at
  `crates/nano-chainstate/src/lib.rs:1022`, `:1056`,
  `crates/nano-rpc/src/lib.rs:144`, `xtask/src/main.rs:60`

The flag decides boot contract principals, the address version byte inside every
serialized principal, `(chain-id)`, and PoX locking behaviour. A mainnet node
built this way computes a different state root on its first block.

## Tasks

- [x] Carry the network and chain identifier as a value through `nano-vm`, not a
      constant.
- [x] Resolve boot contract principals from that value instead of the literal
      testnet address.
- [x] Take the network from configuration in `nano-chainstate`, `nano-rpc` and
      the fixture tooling.
- [x] Cross-check the mainnet boot address and chain identifier against
      stacks-core in `nano-conformance`.

## Acceptance Criteria

- No production crate names a network, chain identifier or boot address literal.
- The captured fixture replay still reports depth 600/600 under an explicit
  testnet configuration.
- A conformance test asserts the mainnet boot principal and chain identifier
  match stacks-core.

## Note, 2026-08-06

Three of the four items were already done, by the mainnet work rather than by
this task, which is why the file said `completed` with nothing ticked. What was
actually still open was narrower than the list and worse than it looked.

Already there, checked rather than assumed:

- `nano_primitives::Network` is a `(mainnet, chain_id)` value with
  `boot_address()` and `boot_contract_id()`. `Vm`, `MarfStore` and `ChainState`
  take one at construction and hand `network.is_mainnet()` and
  `network.chain_id()` to every `GlobalContext::new`; no `mainnet: false` or
  `CHAIN_ID_TESTNET` remains on a production path.
- Boot principals resolve through `network.boot_contract_id(name)` or
  `boot_code_id(name, network.is_mainnet())`. `nano-chainstate` has no
  `BOOT_ADDRESS`; the surviving literals in that file are all inside its
  `#[cfg(test)]` module.
- The cross-check exists and is two tests, not one:
  `network_identity_matches_stacks_core` asserts both networks' chain identifier
  against `stacks_common::consts::CHAIN_ID_{MAINNET,TESTNET}` and both boot
  addresses against `clarity::boot_util::boot_code_addr`, plus `pox-5`'s
  qualified identifier against `boot_code_id`;
  `only_the_mainnet_chain_identifier_is_mainnet` pins that only `0x00000001`
  reads as mainnet, hacknet's `0x80000005` included.

What was open, and what it cost:

- **A state directory did not record its chain, so the network was purely the
  caller's assertion.** `MarfStore::open(network, dir)` took the argument and
  believed it. Opening testnet state as mainnet did not fail — it executed, with
  a different boot address inside every principal written from then on and a
  different `(chain-id)`. `chain_identity` in the side store now holds it, and
  `open` refuses a mismatch. A state written before the row existed is adopted by
  its first open rather than refused, because the alternative is importing a
  380 GB checkpoint again for a fact its own files imply.
- `RpcState::new()` defaulted to `Network::MAINNET` and relied on a builder
  `.on(network)` to correct it. It now takes the network as an argument and the
  builder and the `Default` impl are gone, so there is no longer a way to
  construct one without saying which chain — the type system, not a test.
- `xtask` named `Network::MAINNET` at eleven state-opening sites. Every one of
  those commands opens somebody's chainstate, so pointing any of them at a
  hacknet directory read it as mainnet. They read `nano_vm::recorded_network`
  now, with `NANO_NETWORK` for a state that predates the row.

Pins: `a_state_directory_refuses_to_be_opened_as_another_chain` covers all three
mismatch shapes, including the near miss that matters — hacknet against public
testnet, where the boot address is identical and only the identifier differs —
and `a_state_that_names_no_chain_is_claimed_by_the_first_open` covers the
adoption path. Both fail with `reconcile_network` removed.

What this does not prove. The acceptance criterion "no production crate names a
network literal" is not enforced by a test, because the three sites that remain
are the ones that should: `NodeConfig::network` mapping a configured
`NetworkName` to a `Network`, `nano-p2p`'s named `mainnet()`/`testnet()`
constructors, and `stored_network` rebuilding a `Network` from the row it just
read. A grep gate over those would need an allowlist as long as its findings.
Nor is the "depth 600/600 under an explicit testnet configuration" criterion
re-measured here; the captured replay ran unchanged, but this task did not move
it.
