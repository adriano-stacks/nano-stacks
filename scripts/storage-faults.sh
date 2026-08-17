#!/usr/bin/env bash
set -euo pipefail

export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}

cargo test --profile ci -p nano-conformance --test conformance \
  storage_faults::storage_failures_leave_only_a_complete_replay_prefix \
  -- --exact --nocapture --test-threads=1
