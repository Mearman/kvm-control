#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/release-test.XXXXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/repo"
export PATH="$tmp/bin:$PATH" GITHUB_REPOSITORY=example/project ATTEMPT=2 GITHUB_OUTPUT="$tmp/output" TAG=v1.1.0 MOCK_STATE=draft MOCK_ASSETS="$tmp/assets" MOCK_LOG="$tmp/log"
git -C "$tmp/repo" init -q
git -C "$tmp/repo" -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: baseline'
git -C "$tmp/repo" -c tag.gpgsign=false tag v1.0.0
git -C "$tmp/repo" -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: release'
git -C "$tmp/repo" -c tag.gpgsign=false tag v1.1.0
printf '%s\n' '#!/usr/bin/env bash' 'set -e' 'if [[ "$1" == api ]]; then' '  if [[ "$*" == *releases/tags/* ]]; then cat "$MOCK_ASSETS"; else printf "%s\n" "$MOCK_STATE"; fi' 'else printf "%s\n" "$*" >> "$MOCK_LOG"; fi' > "$tmp/bin/gh"
printf '%s\n' '#!/usr/bin/env bash' 'if [[ "$1" == changelog ]]; then printf "notes from %s\n" "$(git describe --tags --abbrev=0)" > release-notes.md; else printf "1.2.0\n"; fi' > "$tmp/bin/convco"
chmod +x "$tmp/bin/gh" "$tmp/bin/convco"
cd "$tmp/repo"
bash "$root/.github/scripts/version.sh"
grep -q 'tag=v1.1.0' "$GITHUB_OUTPUT"
grep -q "commit=$(git rev-parse HEAD)" "$GITHUB_OUTPUT"
mkdir dist
: > "$MOCK_ASSETS"
for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin; do
  name="kvm-control-${TAG}-${target}.tar.gz"
  printf '%s\n' "$target" > "dist/$name"
  (cd dist && shasum -a 256 "$name" > "$name.sha256")
  for asset in "$name" "$name.sha256"; do
    printf '%s\tuploaded\tsha256:%s\n' "$asset" "$(shasum -a 256 "dist/$asset" | cut -d ' ' -f 1)" >> "$MOCK_ASSETS"
  done
done
bash "$root/.github/scripts/release.sh"
grep -q 'notes from v1.0.0' release-notes.md
grep -q 'release edit v1.1.0 --title v1.1.0 --notes-file release-notes.md' "$MOCK_LOG"
grep -q 'release edit v1.1.0 --draft=false' "$MOCK_LOG"
test "$(git rev-list -n 1 "$TAG")" = "$(git rev-parse HEAD)"
export MOCK_STATE=published
: > "$MOCK_LOG"
bash "$root/.github/scripts/release.sh"
test ! -s "$MOCK_LOG"
printf 'bad\n' > "$MOCK_ASSETS"
if bash "$root/.github/scripts/release.sh"; then printf '%s\n' 'Missing published assets were accepted.' >&2; exit 1; fi
git -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: later'
export MOCK_STATE=draft
if bash "$root/.github/scripts/version.sh"; then printf '%s\n' 'An earlier draft was ignored.' >&2; exit 1; fi
export MOCK_STATE=''
if bash "$root/.github/scripts/version.sh"; then printf '%s\n' 'A release missing after tag creation was ignored.' >&2; exit 1; fi
export MOCK_STATE=published ATTEMPT=1
: > "$GITHUB_OUTPUT"
bash "$root/.github/scripts/version.sh"
grep -q 'tag=v1.2.0' "$GITHUB_OUTPUT"
printf '%s\n' 'Release recovery tests passed.'
