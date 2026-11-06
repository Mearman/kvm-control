#!/usr/bin/env bash
set -euo pipefail

TAG=$1
if [[ ! "$TAG" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  printf 'Invalid release tag: %s\n' "$TAG" >&2
  exit 1
fi

metadata=$(cargo metadata --locked --format-version 1 --no-deps)
VERSION=${TAG#v} perl -0777 -i -pe '
  $count = s/(\[workspace\.package\][^\[]*?^version\s*=\s*)"[^"]+"/${1}"$ENV{VERSION}"/m;
  die "Expected one workspace package version\n" unless $count == 1;
' Cargo.toml
printf '%s' "$metadata" | python3 -c '
import json, pathlib, re, sys

data = json.load(sys.stdin)
workspace = set(data["workspace_members"])
packages = {package["name"] for package in data["packages"] if package["id"] in workspace}
version = sys.argv[1]
for package in data["packages"]:
    if package["id"] not in workspace:
        continue
    manifest = pathlib.Path(package["manifest_path"])
    text = manifest.read_text()
    for dependency in package["dependencies"]:
        if dependency["name"] not in packages or dependency["path"] is None or dependency["req"] == "*":
            continue
        name = re.escape(dependency["rename"] or dependency["name"])
        pattern = r"(?m)^(\s*" + name + r"\s*=\s*\{[^\n}]*?\bversion\s*=\s*)\"[^\"]+\""
        text, count = re.subn(pattern, lambda match: match.group(1) + "\"" + version + "\"", text)
        if count != 1:
            raise SystemExit("Cannot update workspace dependency " + dependency["name"] + " in " + str(manifest))
    manifest.write_text(text)
' "${TAG#v}"
cargo update --workspace
cargo metadata --locked --format-version 1 --no-deps >/dev/null
