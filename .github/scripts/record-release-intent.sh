#!/usr/bin/env bash
# Pushes the abandonment marker for ABANDON (a tag version.sh confirmed as an invalid release intent, or empty) and a tag for every release in PLAN (a JSON array of {tag, commit}), so an interrupted release is resumed by the next run. Existing tags must already point at the planned commits.
set -euo pipefail

printf '%s' "$PLAN" | jq -e 'type == "array" and all(.[]; (.tag | type == "string" and length > 0) and (.commit | type == "string" and length > 0))' >/dev/null
if [ -n "${ABANDON:-}" ]; then
  commit=$(git rev-list -n 1 "refs/tags/$ABANDON")
  marker="abandoned-release-$ABANDON"
  if git rev-parse -q --verify "refs/tags/$marker" >/dev/null; then
    [ "$(git rev-list -n 1 "$marker")" = "$commit" ] || { printf 'Abandonment marker points to a different commit: %s\n' "$marker" >&2; exit 1; }
  else
    git -c user.name='github-actions[bot]' -c user.email='41898282+github-actions[bot]@users.noreply.github.com' -c tag.gpgsign=false tag -a "$marker" "$commit" -m "Abandon invalid release intent $ABANDON"
  fi
  git push origin "refs/tags/$marker"
fi
printf '%s' "$PLAN" | jq -r '.[] | [.tag,.commit] | @tsv' | while IFS=$'\t' read -r tag commit; do
  if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
    if [ "$(git rev-list -n 1 "$tag")" != "$commit" ]; then
      printf 'Tag %s points to a different commit.\n' "$tag" >&2
      exit 1
    fi
  else
    git -c user.name='github-actions[bot]' -c user.email='41898282+github-actions[bot]@users.noreply.github.com' -c tag.gpgsign=false tag -a "$tag" "$commit" -m "$tag"
  fi
  git push origin "refs/tags/$tag"
done
