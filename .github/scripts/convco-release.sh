#!/usr/bin/env bash
set -euo pipefail

view=$(mktemp -d)
trap 'rm -rf "$view"' EXIT
source_repo=$(git rev-parse --show-toplevel)
commit=$(git rev-parse HEAD)
git clone -q --shared --no-checkout "$source_repo" "$view/repo"
git -C "$view/repo" checkout -q --detach "$commit"
# convco builds the commit and compare links in the release notes from the origin URL, which the clone would otherwise give as the local source path.
if origin=$(git -C "$source_repo" remote get-url origin 2>/dev/null); then
  git -C "$view/repo" remote set-url origin "$origin"
fi
while IFS= read -r marker; do
  tag=${marker#abandoned-release-}
  if git -C "$view/repo" rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
    [ "$(git -C "$view/repo" rev-list -n 1 "$marker")" = "$(git -C "$view/repo" rev-list -n 1 "$tag")" ] || { printf 'Abandonment marker points to a different commit: %s\n' "$marker" >&2; exit 1; }
    git -C "$view/repo" tag -d "$tag" >/dev/null
  fi
done < <(git -C "$view/repo" tag --list 'abandoned-release-v*')
if [ "${CONVCO_EXCLUDE_TAG:-}" ]; then
  git -C "$view/repo" tag -d "$CONVCO_EXCLUDE_TAG" >/dev/null
fi
(cd "$view/repo" && convco "$@")
