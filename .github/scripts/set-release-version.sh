#!/usr/bin/env bash
set -euo pipefail

TAG=$1
if [[ ! "$TAG" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  printf 'Invalid release tag: %s\n' "$TAG" >&2
  exit 1
fi

VERSION=${TAG#v} perl -0777 -i -pe '
  $count = s/(\[workspace\.package\][^\[]*?^version\s*=\s*)"[^"]+"/${1}"$ENV{VERSION}"/m;
  die "Expected one workspace package version\n" unless $count == 1;
' Cargo.toml
cargo update --workspace
cargo metadata --locked --format-version 1 --no-deps >/dev/null
