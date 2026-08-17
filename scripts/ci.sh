#!/usr/bin/env bash
set -euo pipefail

export CARGO_TERM_COLOR=always

gate=${1:-all}

run_gate() {
  echo "==> $1"
  case "$1" in
    toolchain)
      configured=$(sed -n 's/^channel = "\([^"]*\)"/\1/p' rust-toolchain.toml)
      test "$configured" = "1.97.1"
      test "$(rustc --version | awk '{print $2}')" = "1.97.1"
      test "$(cargo --version | awk '{print $2}')" = "1.97.0"
      test "$(cargo clippy --version | awk '{print $2}')" = "0.1.97"
      test "$(rustfmt --version | awk '{print $2}' | cut -d- -f1)" = "1.9.0"
      cargo metadata --no-deps --format-version 1 \
        | jq -e 'all(.workspace_members[]; contains("vendor/clarity-wasm") | not)' \
          >/dev/null
      git diff --exit-code -- flake.lock
      ;;
    workflow)
      actionlint
      shellcheck scripts/*.sh .githooks/*
      ;;
    formatting)
      scripts/fmt.sh --check
      ;;
    clippy)
      cargo clippy --release --workspace --all-targets -- -D warnings
      cargo clippy --manifest-path vendor/clarity-wasm/Cargo.toml \
        --release -p clar2wasm --all-targets -- -D warnings
      ;;
    scoreboard)
      cargo run --profile ci -p xtask -- scoreboard
      ;;
    fixtures)
      cargo run --profile ci -p xtask -- validate-fixtures
      ;;
    checkpoint-sample)
      cargo test --profile ci -p nano-node \
        checkpoint_bundle::tests::published_sample_rebuilds_byte_for_byte -- --exact
      ;;
    tests)
      cargo test --profile ci --workspace --tests
      cargo test --profile ci --workspace --doc
      cargo test --manifest-path vendor/clarity-wasm/Cargo.toml \
        --release -p clar2wasm --tests
      cargo test --manifest-path vendor/clarity-wasm/Cargo.toml \
        --release -p clar2wasm --doc
      ;;
    release)
      set +e
      cargo run --profile ci -p xtask -- release-report --no-gates
      status=$?
      set -e
      test "$status" -eq 2
      ;;
    release-integrity)
      cargo test --profile ci -p xtask --bin xtask \
        release_artifact::tests::tracked_staged_untracked_and_ignored_source_are_each_dirty \
        -- --exact
      cargo test --profile ci -p xtask --test release_report \
        an_artifact_from_another_revision_is_an_audit_failure -- --exact
      cargo test --profile ci -p xtask --bin xtask release_candidate::tests -- --nocapture
      ;;
    reproducible-release)
      scripts/reproducible-release.sh
      ;;
    locks)
      git diff --exit-code -- flake.lock Cargo.lock vendor/clarity-wasm/Cargo.lock
      ;;
    *)
      echo "unknown CI gate: $1" >&2
      exit 2
      ;;
  esac
}

case "$gate" in
  all)
    for name in toolchain workflow formatting clippy scoreboard fixtures checkpoint-sample tests release release-integrity locks; do
      run_gate "$name"
    done
    ;;
  # The gates cheap enough for a git hook: no compilation, no test run. The
  # hosted workflow runs `all`; a fast-gated commit is not release evidence.
  fast)
    for name in toolchain workflow formatting locks; do
      run_gate "$name"
    done
    ;;
  *)
    run_gate "$gate"
    ;;
esac
