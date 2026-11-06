#!/usr/bin/env bash
set -euo pipefail

tag=$(git tag --merged HEAD --list 'v[0-9]*' --sort=-v:refname | sed -n '1p')
if [ -z "$tag" ]; then
  printf '%s\n' 'A baseline version tag is required before semantic releases.' >&2
  exit 1
fi

state=$(gh api --paginate "repos/$GITHUB_REPOSITORY/releases" --jq ".[] | select(.tag_name == \"$tag\") | if .draft then \"draft\" else \"published\" end")
commit=$(git rev-list -n 1 "$tag")
head=$(git rev-parse HEAD)
if [ "$commit" != "$head" ]; then
  case "$state" in
    published) ;;
    draft) printf 'Release %s is still a draft at %s. Rerun the workflow for that commit before releasing later commits.\n' "$tag" "$commit" >&2; exit 1 ;;
    '')
      baseline=$(git tag --merged HEAD --list 'v[0-9]*' --sort=v:refname | sed -n '1p')
      if [ "$tag" != "$baseline" ]; then
        printf 'Tag %s has no release. Rerun its workflow before releasing later commits.\n' "$tag" >&2
        exit 1
      fi ;;
    *) printf 'Unexpected release state: %s\n' "$state" >&2; exit 1 ;;
  esac
fi
if [ "$ATTEMPT" -gt 1 ] && [ "$commit" = "$head" ]; then
  printf 'tag=%s\ncommit=%s\n' "$tag" "$commit" >> "$GITHUB_OUTPUT"
else
  next=$(convco version --bump)
  if [ "$next" != "${tag#v}" ]; then
    printf 'tag=v%s\ncommit=%s\n' "$next" "$head" >> "$GITHUB_OUTPUT"
  fi
fi
