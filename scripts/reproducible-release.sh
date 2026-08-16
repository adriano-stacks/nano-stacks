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
scratch_root=${TMPDIR_OVERRIDE:-${TMPDIR:-"$HOME/.cache/nano-stacks/tmp"}}
mkdir -p "$scratch_root"
scratch=$(mktemp -d "$scratch_root/reproducible-release.XXXXXX")
published_store=${NANO_REPRODUCIBLE_STORE:-}
published_created=false

cleanup() {
  status=$?
  trap - EXIT
  if test "$status" -ne 0 && test "$published_created" = true && test -d "$published_store"; then
    if ! find "$published_store" -type d -exec chmod u+w {} + \
      || ! find "$published_store" -depth -delete; then
      echo "failed to remove rejected handoff store: $published_store" >&2
      status=1
    fi
  fi
  case "$scratch" in
    "$scratch_root"/reproducible-release.*)
      if test -d "$scratch"; then
        if ! find "$scratch" -type d -exec chmod u+w {} +; then
          echo "failed to make scratch directories removable: $scratch" >&2
          status=1
        elif ! find "$scratch" -depth -delete; then
          echo "failed to remove scratch directory: $scratch" >&2
          status=1
        fi
      fi
      ;;
    *)
      echo "refusing to remove unexpected scratch path $scratch" >&2
      status=1
      ;;
  esac
  exit "$status"
}
trap cleanup EXIT

if test -z "$published_store"; then
  echo "NANO_REPRODUCIBLE_STORE must name an absent persistent store root" >&2
  exit 1
fi
case "$published_store" in
  /*) ;;
  *)
    echo "NANO_REPRODUCIBLE_STORE must be absolute" >&2
    exit 1
    ;;
esac
if test -e "$published_store"; then
  echo "NANO_REPRODUCIBLE_STORE already exists: $published_store" >&2
  exit 1
fi
published_parent=$(dirname "$published_store")
mkdir -p "$published_parent"
if test "$(stat -c %d "$published_parent")" != "$(stat -c %d "$scratch")"; then
  echo "NANO_REPRODUCIBLE_STORE must be on the scratch filesystem" >&2
  exit 1
fi

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

# Qualification must consume one of the artifacts compared above, not a third
# build of the same derivation. Move the first verified rootless store to the
# caller-owned handoff path, then recheck both its NAR and readable file inventory.
mv "$scratch/first" "$published_store"
published_created=true
printf '%s\n' "$output" > "$published_store/output-path"
published_uri="local?root=$published_store"
published_hash=$(nix --store "$published_uri" path-info --json --json-format 1 "$output" \
  | jq -er --arg output "$output" '.[$output].narHash')
test "$published_hash" = "$nar_hash"
(
  cd "$published_store$output"
  find . -type f -print0 \
    | sort -z \
    | xargs -0 sha256sum
) > "$scratch/published.files"
diff -u "$scratch/first.files" "$scratch/published.files"

printf 'source revision  %s\n' "$revision"
printf 'Nix output       %s\n' "$output"
printf 'NAR hash         %s\n' "$nar_hash"
printf 'binary SHA-256   %s\n' "$binary_hash"
printf 'reproducibility  PASS: two independent stores are byte-identical\n'
printf 'qualification   PASS: verified NAR retained in %s\n' "$published_store"
