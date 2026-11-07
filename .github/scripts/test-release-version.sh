#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/app/src" "$work/lib/src" "$work/tui/src" "$work/mcp/src"
printf '%s\n' '[package]' 'name = "synthetic-root"' 'version.workspace = true' 'edition = "2021"' '[dependencies]' "synthetic-lib = { version = '0.1.0', path = 'lib' }" '[workspace]' 'members = ["app", "lib", "tui", "mcp"]' 'resolver = "2"' '[workspace.package]' "version = '0.1.0'" '[workspace.dependencies]' "synthetic-lib = { version = '0.1.0', path = 'lib' }" > "$work/Cargo.toml"
mkdir -p "$work/src"
printf '%s\n' 'pub fn version() -> &'"'"'static str { synthetic_lib::version() }' > "$work/src/lib.rs"
printf '%s\n' '[package]' 'name = "synthetic-app"' 'version.workspace = true' 'edition = "2021"' '[dependencies]' "synthetic-lib = { version = '0.1.0', path = '../lib' }" > "$work/app/Cargo.toml"
for binary in tui mcp; do
  printf '%s\n' '[package]' "name = \"synthetic-$binary\"" 'version.workspace = true' 'edition = "2021"' '[dependencies]' 'synthetic-lib.workspace = true' > "$work/$binary/Cargo.toml"
  printf '%s\n' 'fn main() { println!("{}", synthetic_lib::version()); }' > "$work/$binary/src/main.rs"
done
printf '%s\n' '[package]' 'name = "synthetic-lib"' "version = '0.1.0'" 'edition = "2021"' > "$work/lib/Cargo.toml"
printf '%s\n' 'fn main() { println!("{} {}", env!("CARGO_PKG_VERSION"), synthetic_lib::version()); }' > "$work/app/src/main.rs"
printf '%s\n' 'pub fn version() -> &'"'"'static str { env!("CARGO_PKG_VERSION") }' > "$work/lib/src/lib.rs"
cd "$work"
cargo generate-lockfile --offline
bash "$script_dir/set-release-version.sh" v2.3.4
cargo metadata --locked --offline --format-version 1 --no-deps | python3 -c 'import json, sys; data = json.load(sys.stdin); assert {p["version"] for p in data["packages"]} == {"2.3.4"}'
cargo build --locked --offline --release --workspace
[ "$(./target/release/synthetic-app)" = '2.3.4 2.3.4' ]
[ "$(./target/release/synthetic-tui)" = '2.3.4' ]
[ "$(./target/release/synthetic-mcp)" = '2.3.4' ]
if bash "$script_dir/set-release-version.sh" invalid; then
  printf '%s\n' 'Invalid tag was accepted' >&2
  exit 1
fi
