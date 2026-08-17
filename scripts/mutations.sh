#!/usr/bin/env bash
set -euo pipefail

export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}

common=(
  -j 1
  --timeout 180
  --build-timeout 600
  --colors never
  --no-shuffle
)

cargo mutants -p nano-chainstate \
  -f 'crates/nano-chainstate/src/authenticate.rs' \
  -F 'authenticate_block.*with Ok' \
  "${common[@]}" -- --lib

cargo mutants -p nano-sortition \
  -f 'crates/nano-sortition/src/lib.rs' \
  -F 'select_epoch4_winner' \
  "${common[@]}"

cargo mutants -p nano-marf \
  -f 'crates/nano-marf/src/lib.rs' \
  -F 'VersionedMarf::seal_to' \
  "${common[@]}"
