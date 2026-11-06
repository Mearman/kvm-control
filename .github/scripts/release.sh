#!/usr/bin/env bash
set -euo pipefail

for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin; do
  archive="kvm-control-${TAG}-${target}.tar.gz"
  test -s "dist/$archive"
  test -s "dist/$archive.sha256"
  (cd dist && shasum -a 256 -c "$archive.sha256")
done

if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  if [ "$(git rev-list -n 1 "$TAG")" != "$(git rev-parse HEAD)" ]; then
    printf 'Tag %s points to a different commit.\n' "$TAG" >&2
    exit 1
  fi
  tag_object=$(git rev-parse "refs/tags/$TAG")
  git update-ref -d "refs/tags/$TAG" "$tag_object"
  trap 'git update-ref "refs/tags/$TAG" "$tag_object"' EXIT
fi
convco changelog --unreleased "${TAG#v}" --max-versions 1 --output release-notes.md
if [ -n "${tag_object-}" ]; then
  git update-ref "refs/tags/$TAG" "$tag_object"
  trap - EXIT
else
  git -c user.name='github-actions[bot]' -c user.email='41898282+github-actions[bot]@users.noreply.github.com' -c tag.gpgsign=false tag -a "$TAG" -m "$TAG"
  git push origin "refs/tags/$TAG"
fi

state=$(gh api --paginate "repos/$GITHUB_REPOSITORY/releases" --jq ".[] | select(.tag_name == \"$TAG\") | if .draft then \"draft\" else \"published\" end")
case "$state" in
  published) ;;
  '') gh release create "$TAG" --draft --verify-tag --title "$TAG" --notes-file release-notes.md ;;
  draft) gh release edit "$TAG" --title "$TAG" --notes-file release-notes.md ;;
  *) printf 'Unexpected release state: %s\n' "$state" >&2; exit 1 ;;
esac

if [ "$state" != published ]; then
  gh release upload "$TAG" dist/* --clobber
fi
assets=$(gh api --paginate "repos/$GITHUB_REPOSITORY/releases/tags/$TAG" --jq '.assets[] | [.name, .state, .digest] | @tsv')
for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin; do
  archive="kvm-control-${TAG}-${target}.tar.gz"
  for name in "$archive" "$archive.sha256"; do
    digest="sha256:$(shasum -a 256 "dist/$name" | cut -d ' ' -f 1)"
    if [ "$(printf '%s\n' "$assets" | awk -F '\t' -v name="$name" -v digest="$digest" '$1 == name && $2 == "uploaded" && $3 == digest { count++ } END { print count+0 }')" != 1 ]; then
      printf 'Remote asset %s is missing, incomplete or has a different digest.\n' "$name" >&2
      exit 1
    fi
  done
done
if [ "$state" != published ]; then
  gh release edit "$TAG" --draft=false
fi
