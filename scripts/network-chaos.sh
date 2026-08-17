#!/usr/bin/env bash
set -euo pipefail

export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}

cargo test --profile ci -p nano-p2p \
  session::tests::fragmented_bytes_cannot_restart_a_message_deadline -- --exact

cargo test --profile ci -p nano-sync \
  tests::a_tip_ahead_of_this_nodes_burn_view_is_still_followed -- --exact

cargo test --profile ci -p nano-conformance --test conformance \
  peer_equivocation:: -- --nocapture --test-threads=1

cargo test --profile ci -p nano-conformance --test conformance \
  replication_failover:: -- --nocapture --test-threads=1

for test in \
  catch_up_rounds::a_peer_that_throttles_the_descent_is_asked_again_next_round \
  catch_up_rounds::a_tip_that_moves_mid_round_is_followed \
  catch_up_rounds::a_restart_across_a_reward_cycle_reaches_the_same_state \
  inventory_schedule::a_peer_that_claimed_nothing_is_asked_only_for_absent_tenures \
  follow_path::a_bitcoin_reorganization_retracts_the_blocks_it_invalidated
do
  cargo test --profile ci -p nano-conformance --test conformance \
    "$test" -- --exact --nocapture --test-threads=1
done
