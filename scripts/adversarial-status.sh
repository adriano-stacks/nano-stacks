#!/usr/bin/env bash
set -euo pipefail

: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY names the repository to inspect}"
inventory=${1:-adversarial-jobs.json}

printf '{"schema":2,"runs":['
separator=
while IFS=$'\t' read -r id workflow event; do
  latest=$(gh run list --repo "$GITHUB_REPOSITORY" --workflow "$workflow" \
    --branch main --event "$event" --status completed --limit 1 \
    --json databaseId,headSha,event,status,conclusion,updatedAt,url | jq '.[0] // null')
  last_success=$(gh run list --repo "$GITHUB_REPOSITORY" --workflow "$workflow" \
    --branch main --event "$event" --status success --limit 1 \
    --json databaseId,headSha,event,status,conclusion,updatedAt,url | jq '.[0] // null')
  printf '%s' "$separator"
  jq -cn \
    --arg job "$id" \
    --arg workflow "$workflow" \
    --arg event "$event" \
    --argjson latest "$latest" \
    --argjson last_success "$last_success" \
    '{job: $job, workflow: $workflow, event: $event,
      latest: ($latest | if . == null then null else {
        id: .databaseId, head_sha: .headSha, event: .event,
        status: .status, conclusion: .conclusion, completed_at: .updatedAt, url: .url
      } end), last_success: ($last_success | if . == null then null else {
        id: .databaseId, head_sha: .headSha, event: .event,
        status: .status, conclusion: .conclusion, completed_at: .updatedAt, url: .url
      } end)}'
  separator=,
done < <(jq -r '.jobs[] | [.id, .workflow, .event] | @tsv' "$inventory")
printf ']}\n'
