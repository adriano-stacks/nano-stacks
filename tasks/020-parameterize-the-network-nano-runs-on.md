---
id: "020"
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

- [ ] Carry the network and chain identifier as a value through `nano-vm`, not a
      constant.
- [ ] Resolve boot contract principals from that value instead of the literal
      testnet address.
- [ ] Take the network from configuration in `nano-chainstate`, `nano-rpc` and
      the fixture tooling.
- [ ] Cross-check the mainnet boot address and chain identifier against
      stacks-core in `nano-conformance`.

## Acceptance Criteria

- No production crate names a network, chain identifier or boot address literal.
- The captured fixture replay still reports depth 600/600 under an explicit
  testnet configuration.
- A conformance test asserts the mainnet boot principal and chain identifier
  match stacks-core.
