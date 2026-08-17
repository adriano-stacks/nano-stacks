#!/usr/bin/env bash
set -euo pipefail

failed=false
declare -A action_pins=()

while IFS= read -r match; do
  reference=${match##*uses: }
  reference=${reference%% *}

  case "$reference" in
    ./* | docker://*) continue ;;
  esac

  revision=${reference##*@}
  if [[ ! $revision =~ ^[0-9a-f]{40}$ ]]; then
    echo "GitHub Action is not pinned by full SHA: $match" >&2
    failed=true
    continue
  fi

  action=${reference%@*}
  owner=${action%%/*}
  repository=${action#*/}
  repository=${repository%%/*}
  upstream="$owner/$repository"

  suffix=${match#*"$reference"}
  if [[ ! $suffix =~ ^[[:space:]]*#[[:space:]]*([^[:space:]]+)[[:space:]]*$ ]]; then
    echo "GitHub Action pin has no exact release tag comment: $match" >&2
    failed=true
    continue
  fi
  release=${BASH_REMATCH[1]}

  pin="$release $revision"
  if [[ -n ${action_pins[$upstream]:-} && ${action_pins[$upstream]} != "$pin" ]]; then
    echo "GitHub Action uses inconsistent pins: $upstream" >&2
    failed=true
    continue
  fi
  action_pins[$upstream]=$pin
done < <(rg --line-number --no-heading 'uses: [^[:space:]#]+' .github/workflows)

test "$failed" = false

for upstream in "${!action_pins[@]}"; do
  read -r release revision <<<"${action_pins[$upstream]}"
  latest=$(
    curl --fail --silent --show-error --location --retry 3 \
      --header 'Accept: application/vnd.github+json' \
      --header 'X-GitHub-Api-Version: 2022-11-28' \
      --header 'User-Agent: nano-stacks-action-pin-check' \
      "https://api.github.com/repos/$upstream/releases/latest" |
      jq --exit-status --raw-output .tag_name
  )
  if [[ $release != "$latest" ]]; then
    echo "GitHub Action $upstream uses $release; latest release is $latest" >&2
    failed=true
    continue
  fi

  refs=$(git ls-remote "https://github.com/$upstream.git" \
    "refs/tags/$release" "refs/tags/$release^{}")
  commit=$(awk '$2 ~ /\^\{\}$/ { print $1 }' <<<"$refs")
  if [[ -z $commit ]]; then
    commit=$(awk '$2 !~ /\^\{\}$/ { print $1 }' <<<"$refs")
  fi
  if [[ ! $commit =~ ^[0-9a-f]{40}$ ]]; then
    echo "GitHub Action release has no resolvable commit: $upstream@$release" >&2
    failed=true
  elif [[ $revision != "$commit" ]]; then
    echo "GitHub Action $upstream@$release is $commit, not pinned $revision" >&2
    failed=true
  fi
done

test "$failed" = false
