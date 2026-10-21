#!/usr/bin/env bash
# Check that a version is live on every registry: crates.io (klava), PyPI
# (klava), and npm (the meta package and the four platform packages). Exits
# non-zero when anything is missing or a registry cannot be reached.
#
# Usage: scripts/check-published.sh [version]
# (defaults to the CLI version in Cargo.toml [package])
set -euo pipefail

cd "$(dirname "$0")/.."

version="${1:-}"
if [ -z "$version" ]; then
  version="$(bash scripts/version.sh)"
fi
[ -n "$version" ] || { echo "cannot read version from Cargo.toml" >&2; exit 2; }

checked=0
missing=0

check() { # <label> <url>
  checked=$((checked + 1))
  code=$(curl -sS -o /dev/null -w '%{http_code}' -L --max-time 30 \
    -A 'klava-check-published (https://github.com/Partysun/klava)' "$2") || code=000
  if [ "$code" = 200 ]; then
    printf '  ok        %s\n' "$1"
  elif [ "$code" = 404 ]; then
    printf '  MISSING   %s\n' "$1"
    missing=$((missing + 1))
  else
    printf '  ERROR(%s) %s\n' "$code" "$1"
    missing=$((missing + 1))
  fi
}

echo "checking version $version"

check "klava $version (crates.io)" "https://crates.io/api/v1/crates/klava/$version"
check "klava $version (PyPI)" "https://pypi.org/pypi/klava/$version/json"
check "@zatsepin/klava $version (npm)" "https://registry.npmjs.org/@zatsepin%2fklava/$version"
check "@zatsepin/klava-darwin $version (npm)" "https://registry.npmjs.org/@zatsepin%2fklava-darwin/$version"
check "@zatsepin/klava-linux-x64-gnu $version (npm)" "https://registry.npmjs.org/@zatsepin%2fklava-linux-x64-gnu/$version"
check "@zatsepin/klava-linux-arm64-gnu $version (npm)" "https://registry.npmjs.org/@zatsepin%2fklava-linux-arm64-gnu/$version"
check "@zatsepin/klava-win32-x64-msvc $version (npm)" "https://registry.npmjs.org/@zatsepin%2fklava-win32-x64-msvc/$version"

echo
if [ "$missing" -eq 0 ]; then
  echo "published: all $checked checks passed for $version"
else
  echo "published: $missing of $checked checks failed for $version" >&2
  exit 1
fi
