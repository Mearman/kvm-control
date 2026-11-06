#!/usr/bin/env bash
set -euo pipefail

commits=$(printf '%s' "$PLAN" | jq -er '[.[].commit] | unique | .[]' || { [ "$PLAN" = '[]' ] && exit 0; exit 1; })
while IFS= read -r commit; do
  [ -n "$commit" ] || continue
  for binary in kvm-cli kvm-tui kvm-mcp; do
    for file in Cargo.toml src/main.rs; do
      if ! git cat-file -e "$commit:crates/$binary/$file"; then
        printf 'Release commit %s lacks %s/%s.\n' "$commit" "$binary" "$file" >&2
        exit 1
      fi
    done
  done
done <<< "$commits"
