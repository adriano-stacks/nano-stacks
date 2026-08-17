#!/usr/bin/env bash
set -euo pipefail

target=${1:-}
seconds=${2:-900}
sanitizer=${NANO_FUZZ_SANITIZER:-none}

case "$target" in
  p2p_frame_and_protocol)
    seed_directory=crates/nano-adversarial/corpus/p2p
    max_length=65536
    ;;
  p2p_session_state)
    seed_directory=crates/nano-adversarial/corpus/p2p-session
    max_length=256
    ;;
  transaction_and_block_codecs)
    seed_directory=crates/nano-adversarial/corpus/codecs
    max_length=65536
    ;;
  transaction_and_block_differential)
    seed_directory=crates/nano-adversarial/corpus/codecs
    max_length=65536
    ;;
  signer_and_stackerdb_codecs)
    seed_directory=crates/nano-adversarial/corpus/signer-stackerdb
    max_length=65536
    ;;
  checkpoint_manifests)
    seed_directory=crates/nano-adversarial/corpus/checkpoint
    max_length=65536
    ;;
  checkpoint_import)
    seed_directory=crates/nano-adversarial/corpus/checkpoint-import
    max_length=133
    ;;
  marf_operations)
    seed_directory=crates/nano-adversarial/corpus/marf
    max_length=32768
    ;;
  clarity_wasm_abi)
    seed_directory=crates/nano-adversarial/corpus/clarity-wasm
    max_length=4096
    ;;
  clarity_result_and_cost_differential)
    seed_directory=crates/nano-adversarial/corpus/clarity-differential
    max_length=50
    ;;
  clarity_refusal_differential)
    seed_directory=crates/nano-adversarial/corpus/clarity-refusal
    max_length=1
    ;;
  *)
    echo "unknown fuzz target: $target" >&2
    exit 2
    ;;
esac

if ! [[ $seconds =~ ^[1-9][0-9]*$ ]]; then
  echo "duration must be a positive number of seconds" >&2
  exit 2
fi

temporary_parent=${NANO_FUZZ_TMP:-${TMPDIR:-/tmp}}
retention_directory=${NANO_FUZZ_RETENTION_DIRECTORY:-fuzz/corpus/$target}
mkdir -p "$temporary_parent"
work_directory=$(mktemp -d "$temporary_parent/nano-fuzz-$target.XXXXXX")
# Invoked by the EXIT trap below.
# shellcheck disable=SC2329
cleanup() {
  status=$?
  trap - EXIT
  find "$work_directory" -type d -exec chmod u+w {} + || status=1
  rm -rf -- "$work_directory" || status=1
  exit "$status"
}
trap cleanup EXIT

corpus_directory=$work_directory/corpus
artifact_directory=$work_directory/artifacts
mkdir -p "$corpus_directory" "$artifact_directory"
cp "$seed_directory"/* "$corpus_directory"/
find "fuzz/corpus/$target" -maxdepth 1 -type f ! -name .gitkeep \
  -exec cp {} "$corpus_directory"/ \;

libfuzzer_budget=(-max_total_time="$seconds")
if test -n "${NANO_FUZZ_RUNS:-}"; then
  libfuzzer_budget=(-runs="$NANO_FUZZ_RUNS")
fi

set +e
cargo fuzz run --fuzz-dir fuzz --sanitizer "$sanitizer" --codegen-units 16 \
  "$target" "$corpus_directory" -- \
  "${libfuzzer_budget[@]}" \
  -artifact_prefix="$artifact_directory/" \
  -max_len="$max_length" \
  -rss_limit_mb=4096 \
  -timeout=30
status=$?
set -e

if test "$status" -eq 0; then
  exit 0
fi

artifact=$(find "$artifact_directory" -maxdepth 1 -type f -printf '%T@ %p\n' \
  | sort -nr | head -1 | cut -d' ' -f2-)
if test -z "$artifact"; then
  echo "fuzz target failed without a reproducible input" >&2
  exit "$status"
fi

candidate=$work_directory/candidate
cp "$artifact" "$candidate"
set +e
cargo fuzz tmin --fuzz-dir fuzz --sanitizer "$sanitizer" --codegen-units 16 \
  "$target" "$candidate" -- \
  -artifact_prefix="$artifact_directory/" \
  -timeout=30
set -e

smallest=$(find "$artifact_directory" "$work_directory" -maxdepth 1 \
  -type f \( -name 'candidate*' -o -path "$artifact_directory/*" \) \
  -printf '%s %p\n' | sort -n | head -1 | cut -d' ' -f2-)
digest=$(sha256sum "$smallest" | cut -d' ' -f1)
mkdir -p "$retention_directory"
destination=$retention_directory/finding-$digest
cp "$smallest" "$destination"
echo "retained reproducible input at $destination" >&2
exit "$status"
