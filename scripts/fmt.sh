#!/usr/bin/env bash
set -euo pipefail

case "${1:-}" in
  "") rustfmt_args=() ;;
  --check) rustfmt_args=(-- --check) ;;
  *) echo "usage: scripts/fmt.sh [--check]" >&2; exit 2 ;;
esac

cargo fmt --all "${rustfmt_args[@]}"
cargo fmt --manifest-path vendor/clarity-wasm/Cargo.toml --all "${rustfmt_args[@]}"
