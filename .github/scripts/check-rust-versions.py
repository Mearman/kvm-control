#!/usr/bin/env python3
"""Checks that every workspace crate declares the Rust version its own dependency tree needs.

A crate's required version is the highest `rust-version` declared by any third-party package it builds against (normal and build dependencies only, since dev-dependencies do not constrain a consumer), and never below LANGUAGE_FLOOR. A crate declaring more than that makes its consumers adopt a toolchain it does not need, and one declaring less would not build on the version it advertises, so the declared value must equal the required one.

Usage: check-rust-versions.py [path to `cargo metadata --format-version 1` output]; without an argument it runs `cargo metadata --locked` itself.
"""
import json
import subprocess
import sys

# Trait upcasting from `Arc<dyn Connection>` to `Arc<dyn Device>` stabilised in 1.86 and is used by kvm-core, so no workspace crate builds on anything older.
LANGUAGE_FLOOR = (1, 86, 0)


def parse(version):
    parts = [int(part) for part in version.split(".")]
    return tuple(parts + [0] * (3 - len(parts)))


def render(version):
    return ".".join(str(part) for part in version)


def required_versions(metadata):
    """Returns {crate name: (required version, the package that sets it)} for each workspace member."""
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    members = set(metadata["workspace_members"])
    result = {}
    for member in sorted(members):
        floor, source = LANGUAGE_FLOOR, "the language floor"
        seen, pending = set(), [member]
        while pending:
            current = pending.pop()
            if current in seen:
                continue
            seen.add(current)
            for dependency in nodes[current]["deps"]:
                if any(kind["kind"] in (None, "build") for kind in dependency["dep_kinds"]):
                    pending.append(dependency["pkg"])
            declared = packages[current].get("rust_version")
            # A member's own value is what is being derived, so only third-party packages set the floor.
            if current not in members and declared and parse(declared) > floor:
                floor, source = parse(declared), f'{packages[current]["name"]} {packages[current]["version"]}'
        result[packages[member]["name"]] = (floor, source)
    return result


def mismatches(metadata):
    """Returns one message for each workspace crate whose declared rust-version differs from the required one."""
    declared = {package["name"]: package.get("rust_version") for package in metadata["packages"] if package["id"] in set(metadata["workspace_members"])}
    messages = []
    for name, (required, source) in sorted(required_versions(metadata).items()):
        if declared[name] is None or parse(declared[name]) != required:
            messages.append(f"{name} declares rust-version {declared[name]} but its dependencies require {render(required)} (set by {source})")
    return messages


def main(argv):
    if len(argv) > 1:
        with open(argv[1], encoding="utf-8") as handle:
            metadata = json.load(handle)
    else:
        metadata = json.loads(subprocess.run(["cargo", "metadata", "--locked", "--format-version", "1"], check=True, capture_output=True, text=True).stdout)
    messages = mismatches(metadata)
    for message in messages:
        print(message, file=sys.stderr)
    return 1 if messages else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
