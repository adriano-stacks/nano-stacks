#!/usr/bin/env bash
set -euo pipefail

workspace=$(git rev-parse --show-toplevel)
cd "$workspace"

if test -n "$(git status --porcelain=v1 --untracked-files=all)"; then
  echo "reproducible release requires a completely clean checkout" >&2
  exit 1
fi

revision=$(git rev-parse HEAD)
source="git+file://$workspace?rev=$revision"
scratch_root=${TMPDIR_OVERRIDE:-"$HOME/.cache/nano-stacks/tmp"}
mkdir -p "$scratch_root"
scratch=$(mktemp -d "$scratch_root/reproducible-release.XXXXXX")

cleanup() {
  status=$?
  trap - EXIT
  case "$scratch" in
    "$scratch_root"/reproducible-release.*) find "$scratch" -depth -delete ;;
    *) echo "refusing to remove unexpected scratch path $scratch" >&2 ;;
  esac
  exit "$status"
}
trap cleanup EXIT

build_one() {
  name=$1
  root="$scratch/$name"
  mkdir -p "$root"
  store="local?root=$root"
  output=$(nix --store "$store" build "$source#stacks-node" \
    --no-link --print-out-paths --cores 2 --max-jobs 1)
  test -n "$output"
  physical="$root$output"
  test -x "$physical/bin/stacks-node"
  nar_hash=$(nix --store "$store" path-info --json --json-format 1 "$output" \
    | jq -er --arg output "$output" '.[$output].narHash')
  (
    cd "$physical"
    find . -type f -print0 \
      | sort -z \
      | xargs -0 sha256sum
  ) > "$scratch/$name.files"
  printf '%s\n' "$output" > "$scratch/$name.output"
  printf '%s\n' "$nar_hash" > "$scratch/$name.nar-hash"
}

build_one first
build_one second

cmp "$scratch/first.output" "$scratch/second.output"
cmp "$scratch/first.nar-hash" "$scratch/second.nar-hash"
diff -u "$scratch/first.files" "$scratch/second.files"

output=$(<"$scratch/first.output")
nar_hash=$(<"$scratch/first.nar-hash")
binary_hash=$(awk '$2 == "./bin/stacks-node" { print $1 }' "$scratch/first.files")
test -n "$binary_hash"
printf 'source revision  %s\n' "$revision"
printf 'Nix output       %s\n' "$output"
printf 'NAR hash         %s\n' "$nar_hash"
printf 'binary SHA-256   %s\n' "$binary_hash"
printf 'reproducibility  PASS: two independent stores are byte-identical\n'
