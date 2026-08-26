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

mod allowance_principal;
mod as_contract_codegen;
mod as_contract_sender;
mod at_block_refusal;
mod binary_restart;
mod block_authentication;
mod block_height_keyword;
mod block_info_tenure_height;
mod burn_spends;
mod catch_up_rounds;
mod coinbase_schedule;
mod derived_reward_set;
mod engine_failure;
mod engine_state_roots;
mod epoch4_profile;
mod epoch4_shadow;
mod event_delivery;
mod event_observer;
mod event_queue;
mod execution_stall;
mod follow_path;
mod fork_retraction;
mod hacknet_replacement;
mod hosted_signer;
mod incoherent_state;
mod inventory_schedule;
mod kill_during_import;
mod kill_during_replay;
mod mainnet_accounting;
mod mainnet_canonical_cost;
mod mainnet_checkpoint;
mod mainnet_codec;
mod mainnet_codegen;
mod mainnet_codegen_effects;
mod mainnet_divergence;
mod mainnet_envelope;
mod mainnet_filter_cost;
mod mainnet_receipts;
mod mainnet_sortition;
mod map_over_a_string;
mod marf_lockstep;
mod mempool;
mod one_engine_in_the_artifact;
mod p2p_discovery;
mod p2p_inbound;
mod p2p_relay;
mod p2p_wire;
mod packaged_follower;
mod peer_equivocation;
mod pox_boundary;
mod pox_five_replay;
mod pox_locking;
mod pre_checkpoint_headers;
mod proposal_failover;
mod rejected_blocks;
mod release_dependencies;
mod release_inventory;
mod replication_failover;
mod restart;
mod signer_weight_enforcement;
mod stock_arity_deployment;
mod storage_faults;
mod submitted_transaction;
mod tenure_block_reward;
mod tenure_continuity;
mod tenure_fee_maturity;
mod tenure_vrf_enforcement;
mod trait_equality;
mod trie_diff;
mod wasm_builds_a_let_bound_placeholder;
mod wasm_is_the_engine;
mod wasm_match_binding_name;
mod wasm_nft_allowance;
mod wasm_response_fold;
mod wasm_trait_fold;
mod write_journal;
