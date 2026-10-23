#!/usr/bin/env bash
# Fail when the release version drifted between the manifests that are
# published independently: Cargo.toml ([package], crates.io), pyproject.toml
# (PyPI, maturin takes the wheel version from there) and the npm package
# templates (npm, pinned from Cargo.toml at publish time — a stale template
# only matters for local `npm pack`, but a mismatch there usually means the
# other two were bumped without it).
#
# Usage: scripts/check-version.sh   (run from anywhere; exits non-zero on drift)
set -euo pipefail

cd "$(dirname "$0")/.."

cargo_version="$(bash scripts/version.sh)"
pypi_version="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' pyproject.toml | head -n 1)"
npm_version="$(sed -n 's/^[[:space:]]*"version":[[:space:]]*"\([^"]*\)".*/\1/p' npm/package.json | head -n 1)"

[ -n "$cargo_version" ] || { echo "cannot read [package] version from Cargo.toml" >&2; exit 2; }
[ -n "$pypi_version" ] || { echo "cannot read version from pyproject.toml" >&2; exit 2; }
[ -n "$npm_version" ] || { echo "cannot read version from npm/package.json" >&2; exit 2; }

echo "cargo (crates.io): $cargo_version"
echo "pyproject (PyPI):  $pypi_version"
echo "npm/package.json:  $npm_version"

if [ "$cargo_version" != "$pypi_version" ] || [ "$cargo_version" != "$npm_version" ]; then
  echo "version mismatch: bump Cargo.toml [package], pyproject.toml [project] and npm/package.json together" >&2
  exit 1
fi

# the npm packages are pinned from the tag while the wheel/sdist versions come
# from Cargo.toml/pyproject.toml, so a tag that disagrees with the manifests
# would publish two different versions by accident
if [ -n "${CI_COMMIT_TAG:-}" ]; then
  tag_version="${CI_COMMIT_TAG#v}"
  if [ "$tag_version" != "$cargo_version" ]; then
    echo "tag $CI_COMMIT_TAG does not match Cargo.toml version $cargo_version" >&2
    exit 1
  fi
  echo "tag: $CI_COMMIT_TAG"
fi

# The extension crates are versioned independently of the CLI and of each other,
# and publish to npm from their own manifest at publish time, so they are not
# part of the CLI's manifest check — only surface them for context.
if command -v node >/dev/null 2>&1; then
  node scripts/ext-npm.js versions | while read -r crate version; do
    echo "extension $crate: $version (independent)"
  done
fi

echo "versions agree"
