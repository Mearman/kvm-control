#!/usr/bin/env bash
# Writes the archive build matrix for the release plan in PLAN (a JSON array of {tag, commit}) to GITHUB_OUTPUT: one row per planned release and target in release.json.
set -euo pipefail

printf 'matrix=%s\n' "$(jq -c --argjson plan "$PLAN" '[$plan[] as $release | .targets[] | . + $release]' "$(dirname "$0")/release.json")" >> "$GITHUB_OUTPUT"
