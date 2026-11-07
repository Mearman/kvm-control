#!/usr/bin/env bash
# Usage: package-archive.sh TARGET TAG BUILD_MESSAGES. Packages the binaries a `cargo build --release --target TARGET --message-format=json` run reported in BUILD_MESSAGES as dist/kvm-control-TAG-TARGET.tar.gz with a SHA-256 checksum file beside it. Fails unless the build produced exactly the binaries in release.json.
set -euo pipefail

target=$1
tag=$2
messages=$3
name="kvm-control-${tag}-${target}.tar.gz"
binaries=$(mktemp)
trap 'rm -f "$binaries"' EXIT
jq -r 'select(.reason == "compiler-artifact" and (.target.kind | index("bin")) != null and .executable != null) | .target.name' "$messages" | sort -u > "$binaries"
diff -u <(jq -r '.binaries[]' "$(dirname "$0")/release.json" | sort) "$binaries"
while IFS= read -r binary; do
  test -x "target/$target/release/$binary"
done < "$binaries"
mkdir -p dist
tar -czf "dist/$name" -C "target/$target/release" -T "$binaries"
(cd dist && shasum -a 256 "$name" > "$name.sha256" && shasum -a 256 -c "$name.sha256")
