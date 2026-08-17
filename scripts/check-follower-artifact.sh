#!/usr/bin/env bash
set -euo pipefail

artifact=${1:?usage: check-follower-artifact.sh ARTIFACT}
evidence="$artifact/share/nano-stacks-follower"
policy="$evidence/follower-policy.json"
surface="$evidence/surface-inventory.json"
schema="$evidence/config.schema.json"
dependencies="$evidence/dependencies.txt"
service_directory="$evidence/systemd"
binary_name=$(jq -er '.artifact.binary' "$policy")
binary="$artifact/bin/$binary_name"
service_name=$(jq -er '.process_authority.chainstate.service' "$policy")
service="$service_directory/$service_name"

for required in "$binary" "$policy" "$surface" "$schema" "$dependencies" "$service"; do
  test -e "$required"
done

inspection_tmp=$(mktemp -d "${TMPDIR:-/tmp}/follower-inspection.XXXXXX")
cleanup() {
  rm -rf -- "$inspection_tmp"
}
trap cleanup EXIT

unit_value() {
  local section=$1
  local key=$2
  local unit=$3
  awk -v wanted_section="$section" -v wanted_key="$key" '
    $0 == "[" wanted_section "]" {
      in_section = 1
      next
    }
    /^\[/ {
      in_section = 0
    }
    in_section && index($0, wanted_key "=") == 1 {
      value = substr($0, length(wanted_key) + 2)
    }
    END {
      if (value == "") {
        exit 1
      }
      print value
    }
  ' "$unit"
}

find -L "$artifact/bin" -mindepth 1 -maxdepth 1 -type f -perm /111 \
  -printf '%f\n' | sort -u >"$inspection_tmp/executables"
jq -r '.artifact.executables[]' "$policy" | sort -u \
  >"$inspection_tmp/expected-executables"
cmp "$inspection_tmp/expected-executables" "$inspection_tmp/executables"

find -L "$service_directory" -mindepth 1 -maxdepth 1 -type f -name '*.service' \
  -printf '%f\n' | sort -u >"$inspection_tmp/service-units"
jq -r '.artifact.service_units[]' "$policy" | sort -u \
  >"$inspection_tmp/expected-service-units"
cmp "$inspection_tmp/expected-service-units" "$inspection_tmp/service-units"

service_user=$(unit_value Service User "$service")
service_group=$(unit_value Service Group "$service")
state_directory=$(unit_value Service StateDirectory "$service")
state_mode=$(unit_value Service StateDirectoryMode "$service")
service_umask=$(unit_value Service UMask "$service")
read_write_paths=$(unit_value Service ReadWritePaths "$service")
test "$service_user" = "$(jq -er '.process_authority.chainstate.user' "$policy")"
test "$service_group" = "$(jq -er '.process_authority.chainstate.group' "$policy")"
test "/var/lib/$state_directory" = \
  "$(jq -er '.process_authority.chainstate.path' "$policy")"
test "$state_mode" = "$(jq -er '.process_authority.chainstate.mode' "$policy")"
test "$service_umask" = 0077
test "$read_write_paths" = "/var/lib/$state_directory"
test "$(unit_value Service NoNewPrivileges "$service")" = true
test "$(unit_value Service ProtectSystem "$service")" = strict
test "$(unit_value Service ProtectHome "$service")" = true
jq -e '
  .process_authority.external_component_requirements == {
    separate_process: true,
    distinct_service_identity: true,
    chainstate_inaccessible_path: .process_authority.chainstate.path,
    protocols_must_be_explicitly_allowlisted: true
  } and
  (.process_authority.optional_roles | length > 0) and
  ([.process_authority.optional_roles[] |
    .shipped == false and
    .chainstate_access == "none" and
    .protocols == []] | all)
' "$policy" >/dev/null

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
executables=$(jq -Rsc 'split("\n") | map(select(length > 0))' \
  <"$inspection_tmp/executables")
service_units=$(jq -Rsc 'split("\n") | map(select(length > 0))' \
  <"$inspection_tmp/service-units")
commands=$(jq -c '.commands' "$surface")
config_top_level=$(jq -c '.config_top_level' "$surface")
loopback_routes=$(jq -c '.loopback_routes' "$surface")
public_routes=$(jq -c '.public_routes' "$surface")
optional_roles=$(jq -c '.process_authority.optional_roles | keys' "$policy")
jq -n \
  --arg binary "$binary_name" \
  --arg binary_sha256 "$(sha256sum "$binary" | awk '{ print $1 }')" \
  --arg dependency_tree_sha256 "$(sha256sum "$dependencies" | awk '{ print $1 }')" \
  --arg policy_sha256 "$(sha256sum "$policy" | awk '{ print $1 }')" \
  --arg surface_inventory_sha256 "$(sha256sum "$surface" | awk '{ print $1 }')" \
  --arg service "$service_name" \
  --arg service_user "$service_user" \
  --arg service_group "$service_group" \
  --arg state_directory "/var/lib/$state_directory" \
  --arg state_mode "$state_mode" \
  --arg service_umask "$service_umask" \
  --argjson commands "$commands" \
  --argjson config_top_level "$config_top_level" \
  --argjson executables "$executables" \
  --argjson linked_internal_packages "$linked_internal" \
  --argjson loopback_routes "$loopback_routes" \
  --argjson optional_roles "$optional_roles" \
  --argjson public_routes "$public_routes" \
  --argjson service_units "$service_units" \
  '{
    schema: "nano-stacks/follower-artifact-inspection/v1",
    binary: $binary,
    binary_sha256: $binary_sha256,
    dependency_tree_sha256: $dependency_tree_sha256,
    policy_sha256: $policy_sha256,
    surface_inventory_sha256: $surface_inventory_sha256,
    commands: $commands,
    config_top_level: $config_top_level,
    executables: $executables,
    linked_internal_packages: $linked_internal_packages,
    loopback_routes: $loopback_routes,
    public_routes: $public_routes,
    service_units: $service_units,
    chainstate_authority: {
      service: $service,
      user: $service_user,
      group: $service_group,
      state_directory: $state_directory,
      state_mode: $state_mode,
      umask: $service_umask
    },
    omitted_optional_roles: $optional_roles,
    symbol_table_inspected: true,
    forbidden_symbol_matches: 0
  }'
