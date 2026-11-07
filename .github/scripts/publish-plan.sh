#!/usr/bin/env bash
set -euo pipefail
script_dir=$(cd "$(dirname "$0")" && pwd)
release_dir=$(mktemp -d)
# Each release checks out its own commit, so the release tooling runs from a copy taken from the commit that started the run.
cp "$script_dir/release.sh" "$script_dir/convco-release.sh" "$script_dir/release.json" "$release_dir/"
trap 'rm -rf "$release_dir"' EXIT
while IFS=$'\t' read -r tag commit; do
  git checkout --detach "$commit"
  TAG="$tag" bash "$release_dir/release.sh"
done < <(printf '%s' "$PLAN" | jq -r '.[] | [.tag,.commit] | @tsv')
