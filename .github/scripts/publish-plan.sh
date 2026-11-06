#!/usr/bin/env bash
set -euo pipefail
script_dir=$(cd "$(dirname "$0")" && pwd)
release_script=$(mktemp)
cp "$script_dir/release.sh" "$release_script"
trap 'rm -f "$release_script"' EXIT
while IFS=$'\t' read -r tag commit; do
  git checkout --detach "$commit"
  TAG="$tag" bash "$release_script"
done < <(printf '%s' "$PLAN" | jq -r '.[] | [.tag,.commit] | @tsv')
