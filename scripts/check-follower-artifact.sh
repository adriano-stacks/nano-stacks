#!/usr/bin/env bash
set -euo pipefail

artifact=${1:?usage: check-follower-artifact.sh ARTIFACT}
evidence="$artifact/share/nano-stacks-follower"
policy="$evidence/follower-policy.json"
surface="$evidence/surface-inventory.json"
schema="$evidence/config.schema.json"
dependencies="$evidence/dependencies.txt"
binary_name=$(jq -er '.artifact.binary' "$policy")
binary="$artifact/bin/$binary_name"

for required in "$binary" "$policy" "$surface" "$schema" "$dependencies"; do
  test -e "$required"
done

inspection_tmp=$(mktemp -d "${TMPDIR:-/tmp}/follower-inspection.XXXXXX")
cleanup() {
  rm -rf -- "$inspection_tmp"
}
trap cleanup EXIT

jq -e --slurpfile policy "$policy" '
  .schema == "nano-stacks/follower-surface-inventory/v1" and
  .commands == $policy[0].commands and
  .loopback_routes == $policy[0].loopback_surfaces and
  .public_routes == $policy[0].public_surfaces
' "$surface" >/dev/null
jq -S '.config_top_level' "$surface" >"$inspection_tmp/inventory-config.json"
jq -S '.properties | keys' "$schema" >"$inspection_tmp/schema-config.json"
cmp "$inspection_tmp/inventory-config.json" "$inspection_tmp/schema-config.json"

sed -E 's/^[|`+ -]+//' "$dependencies" \
  | awk '$1 ~ /^nano-/ { print $1 }' \
  | sort -u >"$inspection_tmp/linked-internal"
jq -r '.allowed_internal_packages[]' "$policy" | sort -u >"$inspection_tmp/allowed-internal"
jq -r '.forbidden_internal_packages[]' "$policy" | sort -u >"$inspection_tmp/forbidden-internal"
comm -23 "$inspection_tmp/linked-internal" "$inspection_tmp/allowed-internal" \
  >"$inspection_tmp/unexpected-internal"
comm -12 "$inspection_tmp/linked-internal" "$inspection_tmp/forbidden-internal" \
  >"$inspection_tmp/linked-forbidden"
test ! -s "$inspection_tmp/unexpected-internal"
test ! -s "$inspection_tmp/linked-forbidden"

nm -C --defined-only "$binary" >"$inspection_tmp/symbols"
test -s "$inspection_tmp/symbols"
while IFS= read -r package; do
  prefix=${package//-/_}::
  if grep -F -- "$prefix" "$inspection_tmp/symbols" >/dev/null; then
    printf 'forbidden package symbol in %s: %s\n' "$binary" "$prefix" >&2
    exit 1
  fi
done <"$inspection_tmp/forbidden-internal"
while IFS= read -r fragment; do
  if grep -F -- "$fragment" "$inspection_tmp/symbols" >/dev/null; then
    printf 'forbidden engine symbol in %s: %s\n' "$binary" "$fragment" >&2
    exit 1
  fi
done < <(jq -r '.forbidden_engine_symbol_fragments[]' "$policy")
while IFS= read -r fragment; do
  grep -F -- "$fragment" "$inspection_tmp/symbols" >/dev/null
done < <(jq -r '.required_engine_symbol_fragments[]' "$policy")

linked_internal=$(jq -Rsc 'split("\n") | map(select(length > 0))' \
  <"$inspection_tmp/linked-internal")
commands=$(jq -c '.commands' "$surface")
config_top_level=$(jq -c '.config_top_level' "$surface")
loopback_routes=$(jq -c '.loopback_routes' "$surface")
public_routes=$(jq -c '.public_routes' "$surface")
jq -n \
  --arg binary "$binary_name" \
  --arg binary_sha256 "$(sha256sum "$binary" | awk '{ print $1 }')" \
  --arg dependency_tree_sha256 "$(sha256sum "$dependencies" | awk '{ print $1 }')" \
  --arg policy_sha256 "$(sha256sum "$policy" | awk '{ print $1 }')" \
  --arg surface_inventory_sha256 "$(sha256sum "$surface" | awk '{ print $1 }')" \
  --argjson commands "$commands" \
  --argjson config_top_level "$config_top_level" \
  --argjson linked_internal_packages "$linked_internal" \
  --argjson loopback_routes "$loopback_routes" \
  --argjson public_routes "$public_routes" \
  '{
    schema: "nano-stacks/follower-artifact-inspection/v1",
    binary: $binary,
    binary_sha256: $binary_sha256,
    dependency_tree_sha256: $dependency_tree_sha256,
    policy_sha256: $policy_sha256,
    surface_inventory_sha256: $surface_inventory_sha256,
    commands: $commands,
    config_top_level: $config_top_level,
    linked_internal_packages: $linked_internal_packages,
    loopback_routes: $loopback_routes,
    public_routes: $public_routes,
    symbol_table_inspected: true,
    forbidden_symbol_matches: 0
  }'
