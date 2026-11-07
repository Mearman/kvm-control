#!/usr/bin/env bash
set -euo pipefail

TAG=$1
if [[ ! "$TAG" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  printf 'Invalid release tag: %s\n' "$TAG" >&2
  exit 1
fi

metadata=$(cargo metadata --locked --format-version 1 --no-deps)
VERSION=${TAG#v} perl -0777 -i -pe '
  $count = s/(\[workspace\.package\][^\[]*?^version\s*=\s*)(?:"[^"]+"|\x27[^\x27]+\x27)/${1}"$ENV{VERSION}"/m;
  die "Expected one workspace package version\n" unless $count == 1;
' Cargo.toml
printf '%s' "$metadata" | python3 -c '
import json, pathlib, re, sys, tomllib

data = json.load(sys.stdin)
workspace = set(data["workspace_members"])
packages = {package["name"] for package in data["packages"] if package["id"] in workspace}
version = sys.argv[1]
root = pathlib.Path(data["workspace_root"]) / "Cargo.toml"
inherited = set()
for package in data["packages"]:
    if package["id"] not in workspace:
        continue
    manifest = pathlib.Path(package["manifest_path"])
    text = manifest.read_text()
    package_version = tomllib.loads(text)["package"].get("version")
    if isinstance(package_version, str):
        section = re.search(r"(?ms)^\[package\]\s*$.*?(?=^\[|\Z)", text)
        if section is None:
            raise SystemExit("Missing package section in " + str(manifest))
        updated, count = re.subn(r"(?m)^(\s*version\s*=\s*)(?:\"[^\"]+\"|\x27[^\x27]+\x27)", lambda match: match.group(1) + "\"" + version + "\"", section.group())
        if count != 1:
            raise SystemExit("Cannot update package version in " + str(manifest))
        text = text[:section.start()] + updated + text[section.end():]
    processed = set()
    for dependency in package["dependencies"]:
        if dependency["name"] not in packages or dependency["path"] is None or dependency["req"] == "*":
            continue
        key = dependency["rename"] or dependency["name"]
        if key in processed:
            continue
        processed.add(key)
        parsed = tomllib.loads(text)
        declarations = [table.get(key) for section in ("dependencies", "dev-dependencies", "build-dependencies") for table in [parsed.get(section, {})]]
        for target in parsed.get("target", {}).values():
            declarations.extend(target.get(section, {}).get(key) for section in ("dependencies", "dev-dependencies", "build-dependencies"))
        inherited_declarations = [declaration for declaration in declarations if isinstance(declaration, dict) and declaration.get("workspace") is True]
        direct = [declaration for declaration in declarations if isinstance(declaration, dict) and "version" in declaration]
        if inherited_declarations:
            inherited.add(key)
        name = re.escape(key)
        pattern = r"(?m)^(\s*" + name + r"\s*=\s*\{[^\n}]*?\bversion\s*=\s*)(?:\"[^\"]+\"|\x27[^\x27]+\x27)"
        workspace_section = re.search(r"(?ms)^\[workspace\.dependencies\]\s*$.*?(?=^\[|\Z)", text) if manifest == root else None
        if workspace_section:
            before, first = re.subn(pattern, lambda match: match.group(1) + "\"" + version + "\"", text[:workspace_section.start()])
            after, second = re.subn(pattern, lambda match: match.group(1) + "\"" + version + "\"", text[workspace_section.end():])
            text, count = before + workspace_section.group() + after, first + second
        else:
            text, count = re.subn(pattern, lambda match: match.group(1) + "\"" + version + "\"", text)
        if count != len(direct) or not (direct or inherited_declarations):
            raise SystemExit("Cannot update workspace dependency " + dependency["name"] + " in " + str(manifest))
    manifest.write_text(text)
root_text = root.read_text()
for key in inherited:
    section = re.search(r"(?ms)^\[workspace\.dependencies\]\s*$.*?(?=^\[|\Z)", root_text)
    if section is None:
        raise SystemExit("Missing workspace dependencies")
    pattern = r"(?m)^(\s*" + re.escape(key) + r"\s*=\s*\{[^\n}]*?\bversion\s*=\s*)(?:\"[^\"]+\"|\x27[^\x27]+\x27)"
    updated, count = re.subn(pattern, lambda match: match.group(1) + "\"" + version + "\"", section.group())
    if count != 1:
        raise SystemExit("Cannot update inherited workspace dependency " + key)
    root_text = root_text[:section.start()] + updated + root_text[section.end():]
root.write_text(root_text)
' "${TAG#v}"
cargo update --workspace
cargo metadata --locked --format-version 1 --no-deps >/dev/null
