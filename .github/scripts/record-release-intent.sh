#!/usr/bin/env bash
set -euo pipefail

printf '%s' "$PLAN" | jq -e 'type == "array" and all(.[]; (.tag | type == "string" and length > 0) and (.commit | type == "string" and length > 0))' >/dev/null
printf '%s' "$PLAN" | jq -r '.[] | [.tag,.commit] | @tsv' | while IFS=$'\t' read -r tag commit; do
  if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
    if [ "$(git rev-list -n 1 "$tag")" != "$commit" ]; then
      printf 'Tag %s points to a different commit.\n' "$tag" >&2
      exit 1
    fi
  else
    git -c user.name='github-actions[bot]' -c user.email='41898282+github-actions[bot]@users.noreply.github.com' -c tag.gpgsign=false tag -a "$tag" "$commit" -m "$tag"
    git push origin "refs/tags/$tag"
  fi
done
