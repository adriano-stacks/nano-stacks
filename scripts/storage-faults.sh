#!/usr/bin/env bash
set -euo pipefail

export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}

helper=target/ci/fiu-nth.so
mkdir -p "$(dirname "$helper")"
read -r -a fiu_cflags <<<"$(pkg-config --cflags libfiu)"
read -r -a fiu_libs <<<"$(pkg-config --libs libfiu)"
cc -std=c11 -shared -fPIC -O2 -Wall -Wextra -Werror \
  "${fiu_cflags[@]}" scripts/fiu-nth.c "${fiu_libs[@]}" -o "$helper"
export NANO_FIU_NTH_PRELOAD=$PWD/$helper
export NANO_FIU_PRELOAD_DIR
NANO_FIU_PRELOAD_DIR=$(pkg-config --variable=libdir libfiu)

cargo test --profile ci -p nano-follower --lib \
  sortition::tests::every_saved_capture_rename_failure_preserves_or_refuses_the_generation \
  -- --exact --nocapture

cargo test --profile ci -p nano-conformance --test conformance \
  storage_faults:: -- --nocapture --test-threads=1
