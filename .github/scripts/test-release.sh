#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/release-test.XXXXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/repo"
export PATH="$tmp/bin:$PATH" GITHUB_REPOSITORY=example/project ATTEMPT=1 GITHUB_OUTPUT="$tmp/output" MOCK_ASSETS="$tmp/assets" MOCK_LOG="$tmp/log" MOCK_RELEASES="$tmp/releases"
git -C "$tmp/repo" init -q
git -C "$tmp/repo" -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: baseline'
export MOCK_NEXT=0.1.0
printf '%s\n' '#!/usr/bin/env bash' 'printf "%s\n" "$MOCK_NEXT"' > "$tmp/bin/convco"
printf '%s\n' '#!/usr/bin/env bash' 'printf "[]\n"' > "$tmp/bin/gh"
chmod +x "$tmp/bin/convco" "$tmp/bin/gh"
(cd "$tmp/repo" && bash "$root/.github/scripts/version.sh")
grep -q '"tag":"v0.1.0"' "$GITHUB_OUTPUT"
: > "$GITHUB_OUTPUT"
git -C "$tmp/repo" -c tag.gpgsign=false tag v1.0.0
git -C "$tmp/repo" -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: first release'
git -C "$tmp/repo" -c tag.gpgsign=false tag v1.1.0
first=$(git -C "$tmp/repo" rev-parse HEAD)
git -C "$tmp/repo" -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: later release'
second=$(git -C "$tmp/repo" rev-parse HEAD)
printf '%s\n' '#!/usr/bin/env bash' 'set -e' 'if [[ "$1" == api ]]; then' '  if [[ "$*" == *--slurp* ]]; then cat "$MOCK_RELEASES"; elif [[ "$*" == *releases/tags/* ]]; then cat "$MOCK_ASSETS"; else if [[ ${MOCK_PUBLISHED:-} ]]; then printf "published\n"; else printf "draft\n"; fi; fi' 'elif [[ "$1 $2" == "release download" ]]; then cp "$MOCK_REMOTE"/* "$5"/; else printf "%s %s\n" "$(git rev-parse HEAD)" "$*" >> "$MOCK_LOG"; fi' > "$tmp/bin/gh"
printf '%s\n' '#!/usr/bin/env bash' 'if [[ "$1" == changelog ]]; then printf "notes from %s\n" "$(git describe --tags --abbrev=0)" > release-notes.md; else printf "%s\n" "$MOCK_NEXT"; fi' > "$tmp/bin/convco"
chmod +x "$tmp/bin/gh" "$tmp/bin/convco"
cd "$tmp/repo"
export MOCK_NEXT=1.2.0
printf '%s\n' '[[{"tag_name":"v1.0.0","draft":false},{"tag_name":"v1.1.0","draft":true}]]' > "$MOCK_RELEASES"
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
export MOCK_REMOTE="$tmp/remote"
mkdir "$MOCK_REMOTE"
cp dist/* "$MOCK_REMOTE"/
git init -q --bare "$tmp/origin"
git remote add origin "$tmp/origin"
PLAN="$plan" bash "$root/.github/scripts/publish-plan.sh"
grep -q "$first release edit v1.1.0 --draft=false" "$MOCK_LOG"
grep -q "$second release edit v1.2.0 --draft=false" "$MOCK_LOG"
test "$(grep -n -- '--draft=false' "$MOCK_LOG" | cut -d: -f2- | cut -d' ' -f1 | tr '\n' ' ')" = "$first $second "
grep -q "$first release edit v1.1.0 --title v1.1.0 --notes-file release-notes.md" "$MOCK_LOG"
export MOCK_PUBLISHED=1
printf 'different local archive\n' > dist/kvm-control-v1.1.0-x86_64-unknown-linux-gnu.tar.gz
(cd dist && shasum -a 256 kvm-control-v1.1.0-x86_64-unknown-linux-gnu.tar.gz > kvm-control-v1.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256)
git checkout --detach "$first"
TAG=v1.1.0 bash "$root/.github/scripts/release.sh"
unset MOCK_PUBLISHED
printf 'bad\n' > "$MOCK_ASSETS"
: > "$MOCK_LOG"
if PLAN="$plan" bash "$root/.github/scripts/publish-plan.sh"; then printf '%s\n' 'Missing remote assets were accepted.' >&2; exit 1; fi
if grep -q "$second release" "$MOCK_LOG"; then printf '%s\n' 'A later release ran after recovery failed.' >&2; exit 1; fi
git checkout --detach "$second"
git -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: third release'
git -c tag.gpgsign=false tag v1.3.0
printf '%s\n' '[[{"tag_name":"v1.0.0","draft":false},{"tag_name":"v1.1.0","draft":true},{"tag_name":"v1.3.0","draft":true}]]' > "$MOCK_RELEASES"
git checkout --detach "$first"
cp "$MOCK_REMOTE/kvm-control-v1.1.0-aarch64-unknown-linux-gnu.tar.gz.sha256" "$MOCK_REMOTE/kvm-control-v1.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256"
if TAG=v1.1.0 bash "$root/.github/scripts/release.sh"; then printf '%s\n' 'Cross-target remote checksum was accepted.' >&2; exit 1; fi
(cd "$MOCK_REMOTE" && shasum -a 256 kvm-control-v1.1.0-x86_64-unknown-linux-gnu.tar.gz > kvm-control-v1.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256)
cp "$MOCK_REMOTE/kvm-control-v1.1.0-x86_64-unknown-linux-gnu.tar.gz" "$MOCK_REMOTE/kvm-control-v1.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256" dist/
rm "$tmp/bin/convco"
real_convco=$(command -v convco)
mkdir "$tmp/untagged"
git -C "$tmp/untagged" init -q
git -C "$tmp/untagged" -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: initial release'
printf '%s\n' '[[]]' > "$MOCK_RELEASES"
: > "$GITHUB_OUTPUT"
(cd "$tmp/untagged" && bash "$root/.github/scripts/version.sh")
test "$(grep '^plan=' "$GITHUB_OUTPUT" | cut -d = -f 2- | jq -r '.[0].tag')" = v0.1.0
git -C "$tmp/untagged" -c tag.gpgsign=false tag v0.1.0
initial=$(git -C "$tmp/untagged" rev-parse HEAD)
git -C "$tmp/untagged" -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: after interrupted release'
later=$(git -C "$tmp/untagged" rev-parse HEAD)
: > "$GITHUB_OUTPUT"
(cd "$tmp/untagged" && bash "$root/.github/scripts/version.sh")
test "$(grep '^plan=' "$GITHUB_OUTPUT" | cut -d = -f 2- | jq -r 'map(.tag) | join(",")')" = 'v0.1.0,v0.1.1'
test "$(grep '^plan=' "$GITHUB_OUTPUT" | cut -d = -f 2- | jq -r '.[0].commit')" = "$initial"
test "$(grep '^plan=' "$GITHUB_OUTPUT" | cut -d = -f 2- | jq -r '.[1].commit')" = "$later"
cp "$MOCK_ASSETS" "$tmp/invalid-assets"
for asset in "$MOCK_REMOTE"/*; do
  printf '%s\tuploaded\tsha256:%s\n' "$(basename "$asset")" "$(shasum -a 256 "$asset" | cut -d ' ' -f 1)"
done > "$MOCK_ASSETS"
TAG=v1.1.0 bash "$root/.github/scripts/release.sh"
grep -q 'first release' release-notes.md
if grep -q 'later release' release-notes.md; then printf '%s\n' 'Release notes include a later commit.' >&2; exit 1; fi
git checkout --detach "$second"
printf '%s\n' '#!/usr/bin/env bash' 'printf "%s\n" "$MOCK_NEXT"' > "$tmp/bin/convco"
chmod +x "$tmp/bin/convco"
export MOCK_NEXT=1.3.0
printf '%s\n' '[[{"tag_name":"v1.0.0","draft":false},{"tag_name":"v1.1.0","draft":true},{"tag_name":"v1.3.0","draft":true}]]' > "$MOCK_RELEASES"
: > "$GITHUB_OUTPUT"
bash "$root/.github/scripts/version.sh"
test "$(grep '^plan=' "$GITHUB_OUTPUT" | cut -d = -f 2- | jq -r 'map(.tag) | join(",")')" = 'v1.1.0,v1.2.0,v1.3.0'
printf '%s\n' 'Release recovery tests passed.'
