#!/usr/bin/env bash
# Publish dist/* artifacts to PyPI, skipping files that already exist —
# PyPI rejects re-uploads with "File already exists", so re-runs of the
# same version must be a no-op (mirrors scripts/npm-publish.js for npm).
#
# Both the "is it there?" query and the upload are retried: the macOS and
# Windows release agents have flaky DNS, and one lost lookup during a release
# must not fail the whole pipeline (and must never be mistaken for "missing",
# which would turn a re-run into a "File already exists" failure).
#
# Usage: bash scripts/pypi-publish.sh   (run from anywhere)
set -euo pipefail

cd "$(dirname "$0")/.."

PROJECT="klava" # PyPI project name (static publish target)
# [package] version, not [workspace.package] (see scripts/version.sh)
VERSION="$(bash scripts/version.sh)"
[ -n "$VERSION" ] || { echo "cannot read version from Cargo.toml" >&2; exit 1; }

shopt -s nullglob
artifacts=(dist/*)
shopt -u nullglob
[ "${#artifacts[@]}" -gt 0 ] || { echo "no artifacts in dist/" >&2; exit 1; }

API="https://pypi.org/pypi/$PROJECT/$VERSION/json"
attempts=8
body="$(mktemp)"
trap 'rm -f "$body"' EXIT

# Print the filenames already on PyPI for this version, one per line.
# Exits non-zero when the registry cannot be reached (404 = nothing yet, which
# is *not* an error: the first publisher of a version always sees it).
#
# Only flags that every curl build understands are used: --retry-connrefused
# is 7.52+/TLS-only and the release images ship older curls (the maturin image
# rejects it outright). The outer loop below is what actually rides out a
# dead resolver on the macOS/Windows agents.
fetch_published() {
  local http
  http="$(curl -sS --retry 3 --retry-delay 5 \
    --max-time 45 -o "$body" -w '%{http_code}' "$API")" || return 1
  case "$http" in
    200) python3 -c '
import json, sys
with open(sys.argv[1]) as fh:
    for u in json.load(fh).get("urls", []):
        print(u["filename"])
' "$body" ;;
    404) : ;;
    *) echo "PyPI query returned HTTP $http" >&2; return 1 ;;
  esac
}

published=""
for ((i = 1; i <= attempts; i++)); do
  if published="$(fetch_published)"; then
    break
  fi
  if [ "$i" -eq "$attempts" ]; then
    echo "cannot query PyPI for $PROJECT $VERSION after $attempts attempts" >&2
    exit 1
  fi
  echo "PyPI query failed (attempt $i/$attempts), retrying in 15s" >&2
  sleep 15
done

missing=()
for artifact in "${artifacts[@]}"; do
  f="$(basename "$artifact")"
  # exact line match: longer similar wheel names must not mask a missing file
  # (e.g. ..._aarch64.whl vs ..._manylinux_2_17_aarch64.manylinux2014_aarch64)
  if grep -qxF "$f" <<<"$published"; then
    echo "skip: $f already on PyPI"
  else
    missing+=("$artifact")
  fi
done

if [ "${#missing[@]}" -eq 0 ]; then
  echo "nothing new to publish"
  exit 0
fi

echo "publishing ${#missing[@]} file(s): ${missing[*]}"
# A failed upload is re-planned from scratch: if the connection died *after*
# PyPI accepted the file, the re-run sees it as published and skips it.
for ((i = 1; i <= attempts; i++)); do
  if uv publish "${missing[@]}"; then
    exit 0
  fi
  if [ "$i" -eq "$attempts" ]; then
    echo "uv publish failed after $attempts attempts" >&2
    exit 1
  fi
  echo "uv publish failed (attempt $i/$attempts), retrying in 15s" >&2
  sleep 15
  published="$(fetch_published)" || continue
  missing=()
  for artifact in "${artifacts[@]}"; do
    f="$(basename "$artifact")"
    if grep -qxF "$f" <<<"$published"; then
      echo "skip: $f already on PyPI"
    else
      missing+=("$artifact")
    fi
  done
  [ "${#missing[@]}" -gt 0 ] || exit 0
  echo "retrying with ${#missing[@]} file(s): ${missing[*]}"
done
