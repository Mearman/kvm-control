#!/usr/bin/env bash
set -euo pipefail

releases=$(gh api --paginate --slurp "repos/$GITHUB_REPOSITORY/releases?per_page=100")
abandoned=${ABANDON_INVALID_TAG:-}
if [ -n "$abandoned" ]; then
  git rev-parse -q --verify "refs/tags/$abandoned" >/dev/null || { printf 'Unknown tag: %s\n' "$abandoned" >&2; exit 1; }
  if printf '%s' "$releases" | jq -e --arg tag "$abandoned" '[.[][] | select(.tag_name == $tag)] | length != 0' >/dev/null; then
    printf 'Cannot abandon a tag with a release: %s\n' "$abandoned" >&2
    exit 1
  fi
  commit=$(git rev-list -n 1 "$abandoned")
  candidate=$(jq -cn --arg tag "$abandoned" --arg commit "$commit" '[{tag:$tag,commit:$commit}]')
  result=0
  PLAN="$candidate" bash "$(dirname "$0")/check-release-binaries.sh" || result=$?
  if [ "$result" -ne 1 ]; then
    printf 'Cannot confirm an invalid release intent: %s\n' "$abandoned" >&2
    exit 1
  fi
  marker="abandoned-release-$abandoned"
  if git rev-parse -q --verify "refs/tags/$marker" >/dev/null; then
    [ "$(git rev-list -n 1 "$marker")" = "$commit" ] || { printf 'Abandonment marker points to a different commit: %s\n' "$marker" >&2; exit 1; }
  else
    git -c user.name='github-actions[bot]' -c user.email='41898282+github-actions[bot]@users.noreply.github.com' -c tag.gpgsign=false tag -a "$marker" "$commit" -m "Abandon invalid release intent $abandoned"
  fi
fi
tags=$(git tag --merged HEAD --list 'v[0-9]*' --sort=v:refname)
if [ -z "$tags" ]; then
  next=$(bash "$(dirname "$0")/convco-release.sh" version --bump --initial-bump-version 0.1.0)
  plan=$(jq -cn --arg tag "v$next" --arg commit "$(git rev-parse HEAD)" '[{tag:$tag,commit:$commit}]')
else
plan='[]'
while IFS= read -r tag; do
  state=$(printf '%s' "$releases" | jq -r --arg tag "$tag" '[.[][] | select(.tag_name == $tag) | if .draft then "draft" else "published" end] | if length > 1 then error("duplicate release") else .[0] // "missing" end')
  commit=$(git rev-list -n 1 "$tag")
  marker="abandoned-release-$tag"
  if git rev-parse -q --verify "refs/tags/$marker" >/dev/null; then
    [ "$(git rev-list -n 1 "$marker")" = "$commit" ] || { printf 'Abandonment marker points to a different commit: %s\n' "$marker" >&2; exit 1; }
    [ "$state" = missing ] || { printf 'Abandoned tag has a release: %s\n' "$tag" >&2; exit 1; }
    printf 'Skipping abandoned invalid intent %s.\n' "$tag" >&2
    continue
  fi
  if [ "$state" = draft ] || [ "$state" = missing ] || { [ "$ATTEMPT" -gt 1 ] && [ "$commit" = "$(git rev-parse HEAD)" ]; }; then
    plan=$(printf '%s' "$plan" | jq -c --arg tag "$tag" --arg commit "$commit" '. + [{tag:$tag,commit:$commit}]')
  fi
done <<< "$tags"
latest=$(printf '%s\n' "$tags" | sed -n '$p')
next=$(bash "$(dirname "$0")/convco-release.sh" version --bump)
if [ "$next" = "${latest#v}" ] && git rev-parse -q --verify "refs/tags/abandoned-release-$latest" >/dev/null; then
  if [[ ! "$next" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    printf 'Cannot advance abandoned version: %s\n' "$next" >&2
    exit 1
  fi
  next="${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.$((10#${BASH_REMATCH[3]} + 1))"
fi
if [ "$next" != "${latest#v}" ]; then
  plan=$(printf '%s' "$plan" | jq -c --arg tag "v$next" --arg commit "$(git rev-parse HEAD)" '. + [{tag:$tag,commit:$commit}]')
fi
fi
# The abandonment marker is created only in this clone, so that the plan above skips the tag; record-release-intent.sh pushes it once the checks have passed.
printf 'plan=%s\nabandon=%s\n' "$plan" "$abandoned" >> "$GITHUB_OUTPUT"
PLAN="$plan" bash "$(dirname "$0")/release-matrix.sh"
