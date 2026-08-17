#!/usr/bin/env bash
set -euo pipefail

mode=${1:?"usage: scripts/sanitizers.sh miri|address|undefined|thread"}
host=$(rustc -vV | sed -n 's/^host: //p')

export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR="${NANO_SANITIZER_TARGET_ROOT:-target/sanitizers}/$mode"
export RUST_BACKTRACE=1

case "$mode" in
  miri)
    export PROPTEST_CASES=${PROPTEST_CASES:-32}
    export PROPTEST_DISABLE_FAILURE_PERSISTENCE=1
    export MIRIFLAGS="${MIRIFLAGS:-} -Zmiri-env-set=PROPTEST_CASES=$PROPTEST_CASES -Zmiri-env-set=PROPTEST_DISABLE_FAILURE_PERSISTENCE=1"
    cargo miri setup
    cargo miri test -p nano-primitives --lib
    ;;
  address)
    export ASAN_OPTIONS=${ASAN_OPTIONS:-detect_leaks=0:halt_on_error=1}
    export RUSTFLAGS="-Zsanitizer=address -Cforce-frame-pointers=yes"
    cargo test --target "$host" -p nano-wasm-cache --lib
    ;;
  undefined)
    export CC=clang
    export CXX=clang++
    export CFLAGS="${CFLAGS:-} -fsanitize=undefined -fno-sanitize-recover=all"
    export CXXFLAGS="${CXXFLAGS:-} -fsanitize=undefined -fno-sanitize-recover=all"
    export RUSTFLAGS="-Clinker=clang -Clink-arg=-fsanitize=undefined -Cforce-frame-pointers=yes"
    export UBSAN_OPTIONS=${UBSAN_OPTIONS:-halt_on_error=1:print_stacktrace=1}
    cargo test --target "$host" -p nano-crypto -p nano-marf --lib
    ;;
  thread)
    export RUSTFLAGS="-Zsanitizer=thread -Cforce-frame-pointers=yes"
    export TSAN_OPTIONS=${TSAN_OPTIONS:-halt_on_error=1}
    cargo test -Zbuild-std --target "$host" -p nano-queue --lib
    ;;
  *)
    echo "unknown sanitizer mode: $mode" >&2
    exit 2
    ;;
esac
