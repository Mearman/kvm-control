#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
real_convco=$(command -v convco)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/release-test.XXXXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/repo"
export PATH="$tmp/bin:$PATH" GITHUB_REPOSITORY=example/project ATTEMPT=1 GITHUB_OUTPUT="$tmp/output" MOCK_ASSETS="$tmp/assets" MOCK_LOG="$tmp/log" MOCK_RELEASES="$tmp/releases"
export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0=/dev/null
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
printf '%s\n' '#!/usr/bin/env bash' 'set -e' 'if [[ "$1" == api ]]; then' '  if [[ "$*" == *--slurp* ]]; then cat "$MOCK_RELEASES"; elif [[ "$*" == *.assets* ]]; then cat "$MOCK_ASSETS"; else if [[ ${MOCK_PUBLISHED:-} ]]; then printf "published\n"; else printf "draft\n"; fi; fi' 'elif [[ "$1 $2" == "release download" ]]; then cp "$MOCK_REMOTE"/* "$5"/; else printf "%s %s\n" "$(git rev-parse HEAD)" "$*" >> "$MOCK_LOG"; fi' > "$tmp/bin/gh"
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
printf '%s\n' '#!/usr/bin/env bash' 'printf "%s\n" "$MOCK_NEXT"' > "$tmp/bin/convco"
chmod +x "$tmp/bin/convco"
export MOCK_NEXT=0.1.0
mkdir "$tmp/untagged"
git -C "$tmp/untagged" init -q
git -C "$tmp/untagged" -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: initial release'
printf '%s\n' '[[]]' > "$MOCK_RELEASES"
: > "$GITHUB_OUTPUT"
(cd "$tmp/untagged" && bash "$root/.github/scripts/version.sh")
test "$(grep '^plan=' "$GITHUB_OUTPUT" | cut -d = -f 2- | jq -r '.[0].tag')" = v0.1.0
git init -q --bare "$tmp/intent-origin"
git -C "$tmp/untagged" remote add origin "$tmp/intent-origin"
initial=$(git -C "$tmp/untagged" rev-parse HEAD)
intent=$(grep '^plan=' "$GITHUB_OUTPUT" | cut -d = -f 2-)
if (cd "$tmp/untagged" && PLAN="$intent" bash "$root/.github/scripts/check-release-binaries.sh"); then
  printf '%s\n' 'A release without binary manifests was accepted.' >&2
  exit 1
fi
(cd "$tmp/untagged" && PLAN='[]' bash "$root/.github/scripts/check-release-binaries.sh")
mkdir -p "$tmp/untagged/crates/kvm-cli/src" "$tmp/untagged/crates/kvm-tui/src" "$tmp/untagged/crates/kvm-mcp/src"
printf '%s\n' '[workspace]' 'members = ["crates/*"]' 'resolver = "2"' > "$tmp/untagged/Cargo.toml"
for binary in kvm-cli kvm-tui kvm-mcp; do
  printf '%s\n' '[package]' "name = \"$binary\"" 'version = "0.1.0"' 'edition = "2021"' > "$tmp/untagged/crates/$binary/Cargo.toml"
  printf '%s\n' 'fn main() {}' > "$tmp/untagged/crates/$binary/src/main.rs"
done
printf '%s\n' 'autobins = false' >> "$tmp/untagged/crates/kvm-cli/Cargo.toml"
printf '%s\n' 'pub fn marker() {}' > "$tmp/untagged/crates/kvm-cli/src/lib.rs"
(cd "$tmp/untagged" && cargo generate-lockfile --offline && git add Cargo.toml Cargo.lock crates && git -c user.name=test -c user.email=test@example.com commit -qm 'test: add disabled binary')
disabled=$(git -C "$tmp/untagged" rev-parse HEAD)
disabled_plan=$(jq -cn --arg commit "$disabled" '[{tag:"v0.2.0",commit:$commit}]')
if (cd "$tmp/untagged" && PLAN="$disabled_plan" bash "$root/.github/scripts/check-release-binaries.sh"); then
  printf '%s\n' 'An auto-disabled binary was accepted.' >&2
  exit 1
fi
printf '%s\n' '[features]' 'extra = []' '[[bin]]' 'name = "kvm-cli"' 'path = "src/main.rs"' 'required-features = ["extra"]' >> "$tmp/untagged/crates/kvm-cli/Cargo.toml"
(cd "$tmp/untagged" && git add crates/kvm-cli/Cargo.toml && git -c user.name=test -c user.email=test@example.com commit -qm 'test: require binary feature')
feature_plan=$(jq -cn --arg commit "$(git -C "$tmp/untagged" rev-parse HEAD)" '[{tag:"v0.2.0",commit:$commit}]')
if (cd "$tmp/untagged" && PLAN="$feature_plan" bash "$root/.github/scripts/check-release-binaries.sh"); then
  printf '%s\n' 'A binary requiring an inactive feature was accepted.' >&2
  exit 1
fi
printf '%s\n' 'default = ["extra"]' >> "$tmp/untagged/crates/kvm-cli/Cargo.toml"
# Place the default feature in the features table rather than the binary table.
python3 -c 'import pathlib, sys; p = pathlib.Path(sys.argv[1]); s = p.read_text().replace("[features]\n", "[features]\ndefault = [\"extra\"]\n").replace("required-features = [\"extra\"]\ndefault = [\"extra\"]\n", "required-features = [\"extra\"]\n"); p.write_text(s)' "$tmp/untagged/crates/kvm-cli/Cargo.toml"
(cd "$tmp/untagged" && git add crates/kvm-cli/Cargo.toml && git -c user.name=test -c user.email=test@example.com commit -qm 'test: enable binary feature')
feature_plan=$(jq -cn --arg commit "$(git -C "$tmp/untagged" rev-parse HEAD)" '[{tag:"v0.2.0",commit:$commit}]')
(cd "$tmp/untagged" && PLAN="$feature_plan" bash "$root/.github/scripts/check-release-binaries.sh")
printf '%s\n' '[[bin]]' 'name = "optional-tool"' 'path = "src/main.rs"' 'required-features = ["optional-tool"]' >> "$tmp/untagged/crates/kvm-cli/Cargo.toml"
python3 -c 'import pathlib, sys; p = pathlib.Path(sys.argv[1]); p.write_text(p.read_text().replace("[features]\n", "[features]\noptional-tool = []\n"))' "$tmp/untagged/crates/kvm-cli/Cargo.toml"
(cd "$tmp/untagged" && cargo build --offline --workspace --bins --message-format=json > "$tmp/build-messages")
jq -r 'select(.reason == "compiler-artifact" and (.target.kind | index("bin")) != null and .executable != null) | .target.name' "$tmp/build-messages" | sort -u > "$tmp/built-binaries"
grep -qx kvm-cli "$tmp/built-binaries"
if grep -qx optional-tool "$tmp/built-binaries"; then printf '%s\n' 'Inactive binary appeared in build messages.' >&2; exit 1; fi
python3 -c 'import pathlib, sys; p = pathlib.Path(sys.argv[1]); p.write_text(p.read_text().replace("default = [\"extra\"]\n", ""))' "$tmp/untagged/crates/kvm-cli/Cargo.toml"
printf '%s\n' '[target.'"'"'cfg(target_os = "macos")'"'"'.dependencies]' 'kvm-cli = { path = "../kvm-cli", features = ["extra"] }' >> "$tmp/untagged/crates/kvm-tui/Cargo.toml"
(cd "$tmp/untagged" && cargo generate-lockfile --offline && git add Cargo.lock crates && git -c user.name=test -c user.email=test@example.com commit -qm 'test: target-specific binary feature')
platform_plan=$(jq -cn --arg commit "$(git -C "$tmp/untagged" rev-parse HEAD)" '[{tag:"v0.2.0",commit:$commit}]')
if (cd "$tmp/untagged" && PLAN="$platform_plan" bash "$root/.github/scripts/check-release-binaries.sh"); then
  printf '%s\n' 'A feature enabled only on a different platform was accepted.' >&2
  exit 1
fi
python3 -c 'import pathlib, sys; p = pathlib.Path(sys.argv[1]); p.write_text(p.read_text().replace("[target.\x27cfg(target_os = \"macos\")\x27.dependencies]", "[dependencies]"))' "$tmp/untagged/crates/kvm-tui/Cargo.toml"
(cd "$tmp/untagged" && git add crates/kvm-tui/Cargo.toml && git -c user.name=test -c user.email=test@example.com commit -qm 'test: enable binary feature through workspace dependency')
dependency_plan=$(jq -cn --arg commit "$(git -C "$tmp/untagged" rev-parse HEAD)" '[{tag:"v0.2.0",commit:$commit}]')
(cd "$tmp/untagged" && PLAN="$dependency_plan" bash "$root/.github/scripts/check-release-binaries.sh")
for invalid in '{' '[{}]' '[{"tag":"v0.1.0"}]'; do
  if (cd "$tmp/untagged" && PLAN="$invalid" bash "$root/.github/scripts/record-release-intent.sh"); then
    printf '%s\n' 'Invalid release plan was accepted.' >&2
    exit 1
  fi
done
test -z "$(git -C "$tmp/intent-origin" tag --list)"
(cd "$tmp/untagged" && PLAN="$intent" bash "$root/.github/scripts/record-release-intent.sh")
test "$(git -C "$tmp/intent-origin" rev-list -n 1 v0.1.0)" = "$initial"
(cd "$tmp/untagged" && PLAN="$intent" bash "$root/.github/scripts/record-release-intent.sh")
git -C "$tmp/untagged" -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: after interrupted release'
later=$(git -C "$tmp/untagged" rev-parse HEAD)
export MOCK_NEXT=0.1.1
: > "$GITHUB_OUTPUT"
(cd "$tmp/untagged" && bash "$root/.github/scripts/version.sh")
test "$(grep '^plan=' "$GITHUB_OUTPUT" | cut -d = -f 2- | jq -r 'map(.tag) | join(",")')" = 'v0.1.0,v0.1.1'
test "$(grep '^plan=' "$GITHUB_OUTPUT" | cut -d = -f 2- | jq -r '.[0].commit')" = "$initial"
test "$(grep '^plan=' "$GITHUB_OUTPUT" | cut -d = -f 2- | jq -r '.[1].commit')" = "$later"
: > "$GITHUB_OUTPUT"
(cd "$tmp/untagged" && ABANDON_INVALID_TAG=v0.1.0 bash "$root/.github/scripts/version.sh")
test "$(git -C "$tmp/intent-origin" rev-list -n 1 v0.1.0)" = "$initial"
test "$(git -C "$tmp/intent-origin" rev-list -n 1 abandoned-release-v0.1.0)" = "$initial"
test "$(grep '^plan=' "$GITHUB_OUTPUT" | cut -d = -f 2- | jq -r 'map(.tag) | join(",")')" = v0.1.1
export MOCK_NEXT=0.1.0
: > "$GITHUB_OUTPUT"
(cd "$tmp/untagged" && bash "$root/.github/scripts/version.sh")
test "$(grep '^plan=' "$GITHUB_OUTPUT" | cut -d = -f 2- | jq -r 'map(.tag) | join(",")')" = v0.1.1
printf '%s\n' '#!/usr/bin/env bash' 'if [[ "$1" == changelog ]]; then git log -1 --format=%s > "${@: -1}"; else printf "%s\n" "$MOCK_NEXT"; fi' > "$tmp/bin/convco"
chmod +x "$tmp/bin/convco"
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
mkdir "$tmp/real-convco"
git -C "$tmp/real-convco" init -q
git -C "$tmp/real-convco" -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'fix: baseline'
git -C "$tmp/real-convco" -c tag.gpgsign=false tag v1.0.0
git -C "$tmp/real-convco" -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'feat: retained change'
git -C "$tmp/real-convco" -c tag.gpgsign=false tag v1.1.0
git -C "$tmp/real-convco" -c tag.gpgsign=false tag abandoned-release-v1.1.0
git -C "$tmp/real-convco" -c user.name=test -c user.email=test@example.com commit --allow-empty -qm 'fix: later change'
rm "$tmp/bin/convco"
ln -s "$real_convco" "$tmp/bin/convco"
(cd "$tmp/real-convco" && convco changelog --unreleased 1.2.0 --max-versions 1 --output "$tmp/raw-notes" && bash "$root/.github/scripts/convco-release.sh" changelog --unreleased 1.2.0 --max-versions 1 --output "$tmp/filtered-notes")
if grep -q 'retained change' "$tmp/raw-notes"; then printf '%s\n' 'Unfiltered changelog did not reproduce the abandoned baseline.' >&2; exit 1; fi
grep -q 'retained change' "$tmp/filtered-notes"
raw_version=$(cd "$tmp/real-convco" && convco version --bump)
filtered_version=$(cd "$tmp/real-convco" && bash "$root/.github/scripts/convco-release.sh" version --bump)
test "$raw_version" = 1.1.1
test "$filtered_version" = 1.1.0
test "$(git -C "$tmp/real-convco" rev-list -n 1 v1.1.0)" = "$(git -C "$tmp/real-convco" rev-list -n 1 abandoned-release-v1.1.0)"
printf '%s\n' 'Release recovery tests passed.'
