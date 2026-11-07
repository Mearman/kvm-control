#!/usr/bin/env bash
# Usage: smoke-test-archive.sh ARCHIVE VERSION. Unpacks ARCHIVE into an empty directory and checks it holds exactly the binaries in release.json, that kvm-cli reports itself as kvm-cli VERSION, that kvm-cli and kvm-tui print their help, and that kvm-mcp rejects a missing configuration. Run it on a runner of the archive's own target.
set -euo pipefail

archive=$1
version=$2
installed=$(mktemp -d)
trap 'rm -rf "$installed"' EXIT
tar -xzf "$archive" -C "$installed"
diff -u <(jq -r '.binaries[]' "$(dirname "$0")/release.json" | sort) <(ls -A "$installed" | sort)
reported=$("$installed/kvm-cli" --version)
if [ "$reported" != "kvm-cli $version" ]; then
  printf 'kvm-cli reports %s, expected kvm-cli %s\n' "$reported" "$version" >&2
  exit 1
fi
"$installed/kvm-cli" --help >/dev/null
"$installed/kvm-tui" --help >/dev/null
if env -i "$installed/kvm-mcp" >"$installed/mcp.stdout" 2>"$installed/mcp.stderr"; then
  printf 'Expected kvm-mcp to reject missing configuration\n' >&2
  exit 1
fi
if ! grep -Fxq 'Error: NotPresent' "$installed/mcp.stderr"; then
  printf 'Unexpected kvm-mcp error for missing configuration:\n' >&2
  cat "$installed/mcp.stderr" >&2
  exit 1
fi
