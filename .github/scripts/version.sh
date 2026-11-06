#!/usr/bin/env bash
set -euo pipefail

releases=$(gh api --paginate --slurp "repos/$GITHUB_REPOSITORY/releases?per_page=100")
tags=$(git tag --merged HEAD --list 'v[0-9]*' --sort=v:refname)
if [ -z "$tags" ]; then
  next=$(convco version --bump --initial-bump-version 0.1.0)
  plan=$(jq -cn --arg tag "v$next" --arg commit "$(git rev-parse HEAD)" '[{tag:$tag,commit:$commit}]')
else
plan='[]'
baseline=true
while IFS= read -r tag; do
  state=$(printf '%s' "$releases" | jq -r --arg tag "$tag" '[.[][] | select(.tag_name == $tag) | if .draft then "draft" else "published" end] | if length > 1 then error("duplicate release") else .[0] // "missing" end')
  commit=$(git rev-list -n 1 "$tag")
  if [ "$state" = draft ] || { [ "$state" = missing ] && [ "$baseline" = false ]; } || { [ "$ATTEMPT" -gt 1 ] && [ "$commit" = "$(git rev-parse HEAD)" ]; }; then
    plan=$(printf '%s' "$plan" | jq -c --arg tag "$tag" --arg commit "$commit" '. + [{tag:$tag,commit:$commit}]')
  fi
  baseline=false
done <<< "$tags"
latest=$(printf '%s\n' "$tags" | sed -n '$p')
next=$(convco version --bump)
if [ "$next" != "${latest#v}" ]; then
  plan=$(printf '%s' "$plan" | jq -c --arg tag "v$next" --arg commit "$(git rev-parse HEAD)" '. + [{tag:$tag,commit:$commit}]')
fi
fi
matrix=$(printf '%s' "$plan" | jq -c '[.[] as $release | [{os:"ubuntu-24.04",target:"x86_64-unknown-linux-gnu"},{os:"ubuntu-24.04-arm",target:"aarch64-unknown-linux-gnu"},{os:"macos-15-intel",target:"x86_64-apple-darwin"},{os:"macos-15",target:"aarch64-apple-darwin"}][] | . + $release]')
printf 'plan=%s\nmatrix=%s\n' "$plan" "$matrix" >> "$GITHUB_OUTPUT"
