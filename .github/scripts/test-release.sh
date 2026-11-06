#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/release-test.XXXXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/repo"
export PATH="$tmp/bin:$PATH" GITHUB_REPOSITORY=example/project ATTEMPT=1 GITHUB_OUTPUT="$tmp/output" MOCK_ASSETS="$tmp/assets" MOCK_LOG="$tmp/log" MOCK_RELEASES="$tmp/releases"
git -C "$tmp/repo" init -q
git -C "$tmp/repo" -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: baseline'
git -C "$tmp/repo" -c tag.gpgsign=false tag v1.0.0
git -C "$tmp/repo" -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: first release'
git -C "$tmp/repo" -c tag.gpgsign=false tag v1.1.0
first=$(git -C "$tmp/repo" rev-parse HEAD)
git -C "$tmp/repo" -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: later release'
second=$(git -C "$tmp/repo" rev-parse HEAD)
printf '%s\n' '#!/usr/bin/env bash' 'set -e' 'if [[ "$1" == api ]]; then' '  if [[ "$*" == *--slurp* ]]; then cat "$MOCK_RELEASES"; elif [[ "$*" == *releases/tags/* ]]; then cat "$MOCK_ASSETS"; else printf "draft\n"; fi' 'else printf "%s %s\n" "$(git rev-parse HEAD)" "$*" >> "$MOCK_LOG"; fi' > "$tmp/bin/gh"
printf '%s\n' '#!/usr/bin/env bash' 'if [[ "$1" == changelog ]]; then printf "notes from %s\n" "$(git describe --tags --abbrev=0)" > release-notes.md; else printf "%s\n" "$MOCK_NEXT"; fi' > "$tmp/bin/convco"
chmod +x "$tmp/bin/gh" "$tmp/bin/convco"
cd "$tmp/repo"
export MOCK_NEXT=1.2.0
printf '%s\n' '[[{"tag_name":"v1.1.0","draft":true}]]' > "$MOCK_RELEASES"
bash "$root/.github/scripts/version.sh"
plan=$(grep '^plan=' "$GITHUB_OUTPUT" | cut -d = -f 2-)
test "$(printf '%s' "$plan" | jq -r 'map(.tag) | join(",")')" = 'v1.1.0,v1.2.0'
test "$(printf '%s' "$plan" | jq -r '.[0].commit')" = "$first"
test "$(printf '%s' "$plan" | jq -r '.[1].commit')" = "$second"
test "$(grep '^matrix=' "$GITHUB_OUTPUT" | cut -d = -f 2- | jq length)" = 8
mkdir dist
: > "$MOCK_ASSETS"
for tag in v1.1.0 v1.2.0; do
  for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin; do
    name="kvm-control-${tag}-${target}.tar.gz"
    printf '%s\n' "$target" > "dist/$name"
    (cd dist && shasum -a 256 "$name" > "$name.sha256")
    for asset in "$name" "$name.sha256"; do
      printf '%s\tuploaded\tsha256:%s\n' "$asset" "$(shasum -a 256 "dist/$asset" | cut -d ' ' -f 1)" >> "$MOCK_ASSETS"
    done
  done
done
git init -q --bare "$tmp/origin"
git remote add origin "$tmp/origin"
PLAN="$plan" bash "$root/.github/scripts/publish-plan.sh"
grep -q "$first release edit v1.1.0 --draft=false" "$MOCK_LOG"
grep -q "$second release edit v1.2.0 --draft=false" "$MOCK_LOG"
test "$(grep -n -- '--draft=false' "$MOCK_LOG" | cut -d: -f2- | cut -d' ' -f1 | tr '\n' ' ')" = "$first $second "
grep -q "$first release edit v1.1.0 --title v1.1.0 --notes-file release-notes.md" "$MOCK_LOG"
printf 'bad\n' > "$MOCK_ASSETS"
: > "$MOCK_LOG"
if PLAN="$plan" bash "$root/.github/scripts/publish-plan.sh"; then printf '%s\n' 'Missing remote assets were accepted.' >&2; exit 1; fi
if grep -q "$second release" "$MOCK_LOG"; then printf '%s\n' 'A later release ran after recovery failed.' >&2; exit 1; fi
git checkout --detach "$second"
git -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: third release'
git -c tag.gpgsign=false tag v1.3.0
printf '%s\n' '[[{"tag_name":"v1.1.0","draft":true},{"tag_name":"v1.3.0","draft":true}]]' > "$MOCK_RELEASES"
export MOCK_NEXT=1.3.0
: > "$GITHUB_OUTPUT"
bash "$root/.github/scripts/version.sh"
test "$(grep '^plan=' "$GITHUB_OUTPUT" | cut -d = -f 2- | jq -r 'map(.tag) | join(",")')" = 'v1.1.0,v1.2.0,v1.3.0'
printf '%s\n' 'Release recovery tests passed.'
