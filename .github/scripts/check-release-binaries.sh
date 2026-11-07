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
  for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin; do
  if ! metadata=$(cd "$tmp/tree" && cargo metadata --locked --filter-platform "$target" --format-version 1); then
    printf 'Cannot inspect release commit %s.\n' "$commit" >&2
    exit 2
  fi
  if ! features=$(cd "$tmp/tree" && cargo tree --locked --workspace --target "$target" --edges normal,build --prefix none --format '{p}|{f}'); then
    printf 'Cannot resolve release features for %s.\n' "$target" >&2
    exit 2
  fi
  for binary in kvm-cli kvm-tui kvm-mcp; do
    if ! printf '%s' "$metadata" | jq -e --arg binary "$binary" --arg features "$features" '. as $root | any(.packages[]; .id as $id | ($root.workspace_members | index($id)) != null and (.name + " v" + .version + " (" + (.manifest_path | sub("/Cargo.toml$"; "")) + ")|") as $prefix | ([$features | split("\n")[] | select(startswith($prefix)) | ltrimstr($prefix) | split(",")[]] | unique) as $enabled | any(.targets[]; .name == $binary and (.kind | index("bin")) != null and all((.["required-features"] // [])[]; . as $feature | ($enabled | index($feature)) != null)))' >/dev/null; then
      printf 'Release commit %s lacks binary target %s.\n' "$commit" "$binary" >&2
      exit 1
    fi
  done
  done
  rm -rf "$tmp/tree"
done <<< "$commits"
