#!/usr/bin/env bash
set -euo pipefail

failed=false

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
  fi
done < <(rg --line-number --no-heading -o 'uses: [^[:space:]#]+' .github/workflows)

test "$failed" = false
