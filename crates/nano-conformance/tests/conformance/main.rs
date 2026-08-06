//! Every conformance test, in one binary.
//!
//! These were 28 separate files directly under `tests/`, which Cargo compiles as
//! 28 separate crates. Each one re-monomorphized and re-optimized the whole
//! `nano-*` + `clarity` + `wasmtime` generic surface into its own ~30 MB
//! executable: 139 s of the 353 s of CPU it took to rebuild the workspace's test
//! graph after a one-line change to `nano-vm`, and 39 % of the write/test loop.
//! Declared as modules of one target instead, the link happens once.
//!
//! What that costs: the suite no longer has per-process isolation. An `abort`, a
//! stack overflow or an out-of-memory kill in any test takes every other test
//! down with it, and process-global state is now shared — so nothing here may set
//! an environment variable, change the working directory, bind a fixed port or
//! write to a fixed path. Nothing does; the fixed temp path that
//! `release_dependencies` used was the last one, and it is a `tempfile::tempdir`
//! now. `env!("CARGO_MANIFEST_DIR")` is per-crate, not per-target, so every
//! fixture path still resolves.
//!
//! A test's name is now `<module>::<fn>`, so `cargo test -p nano-conformance
//! marf_lockstep` runs what `--test marf_lockstep` used to.

mod as_contract_codegen;
mod as_contract_sender;
mod binary_restart;
mod block_authentication;
mod block_height_keyword;
mod burn_spends;
mod coinbase_schedule;
mod engine_failure;
mod engine_state_roots;
mod event_delivery;
mod event_observer;
mod event_queue;
mod fork_retraction;
mod follow_path;
mod hacknet_replacement;
mod kill_during_import;
mod kill_during_replay;
mod mainnet_accounting;
mod mainnet_checkpoint;
mod mainnet_codec;
mod mainnet_codegen;
mod mainnet_envelope;
mod mainnet_sortition;
mod marf_lockstep;
mod mempool;
mod one_engine_in_the_artifact;
mod p2p_discovery;
mod p2p_inbound;
mod p2p_relay;
mod p2p_wire;
mod peer_equivocation;
mod pox_five_replay;
mod pre_checkpoint_headers;
mod pox_locking;
mod rejected_blocks;
mod release_dependencies;
mod restart;
mod signer_weight_enforcement;
mod tenure_continuity;
mod tenure_fee_maturity;
mod tenure_vrf_enforcement;
mod trie_diff;
mod wasm_builds_a_let_bound_placeholder;
mod wasm_is_the_engine;
mod wasm_match_binding_name;
mod wasm_nft_allowance;
mod wasm_response_fold;
mod wasm_trait_fold;
