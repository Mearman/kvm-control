#!/usr/bin/env bash
set -euo pipefail

commits=$(printf '%s' "$PLAN" | jq -er '[.[].commit] | unique | .[]' || { [ "$PLAN" = '[]' ] && exit 0; exit 1; })
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
while IFS= read -r commit; do
  [ -n "$commit" ] || continue
  mkdir "$tmp/tree"
  git archive "$commit" | tar -x -C "$tmp/tree"
  if [ ! -f "$tmp/tree/Cargo.toml" ]; then
    printf 'Release commit %s lacks a Cargo workspace.\n' "$commit" >&2
    exit 1
  fi
  if ! metadata=$(cd "$tmp/tree" && cargo metadata --locked --format-version 1); then
    printf 'Cannot inspect release commit %s.\n' "$commit" >&2
    exit 2
  fi
  for binary in kvm-cli kvm-tui kvm-mcp; do
    if ! printf '%s' "$metadata" | jq -e --arg binary "$binary" '. as $root | any(.packages[]; .id as $id | ($root.workspace_members | index($id)) != null and ([$root.resolve.nodes[] | select(.id == $id) | .features[]] as $enabled | any(.targets[]; .name == $binary and (.kind | index("bin")) != null and all((.["required-features"] // [])[]; . as $feature | ($enabled | index($feature)) != null))))' >/dev/null; then
      printf 'Release commit %s lacks binary target %s.\n' "$commit" "$binary" >&2
      exit 1
    fi
  done
  rm -rf "$tmp/tree"
done <<< "$commits"
