#!/usr/bin/env bash
# Check that a version is live on every registry: crates.io (klava), PyPI
# (klava), npm (the meta package and the four platform packages), plus the
# extension npm packages. Exits non-zero when anything is missing or a registry
# cannot be reached.
#
# Usage: scripts/check-published.sh [version] [extensions-version]
#   the CLI version defaults to Cargo.toml [package]
#   the extension version defaults to Cargo.toml [workspace.package]
#   pass an empty extensions version (or "none") to check only the CLI
set -euo pipefail

cd "$(dirname "$0")/.."

version="${1:-}"
if [ -z "$version" ]; then
  version="$(bash scripts/version.sh)"
fi
[ -n "$version" ] || { echo "cannot read version from Cargo.toml" >&2; exit 2; }

ext_version="${2-}"
if [ "$ext_version" = "none" ]; then
  ext_version=""
elif [ -z "$ext_version" ]; then
  # all three crates share [workspace.package]; take the first
  ext_version="$(node scripts/ext-npm.js versions | awk 'NR == 1 { print $2 }')"
fi

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

# The extensions carry their own version (Cargo.toml [workspace.package]) and
# ship as one package per crate and platform.
if [ -n "$ext_version" ]; then
  echo "checking extension version $ext_version"
  for crate in klava-compress klava-pii-guardrail klava-model-escalation; do
    for platform in linux-x64-gnu linux-arm64-gnu darwin win32-x64-msvc; do
      encoded="@zatsepin%2f${crate}-${platform}"
      check "@zatsepin/${crate}-${platform} $ext_version (npm)" \
        "https://registry.npmjs.org/${encoded}/${ext_version}"
    done
  done
fi

echo
if [ "$missing" -eq 0 ]; then
  echo "published: all $checked checks passed for $version${ext_version:+ (extensions $ext_version)}"
else
  echo "published: $missing of $checked checks failed for $version${ext_version:+ (extensions $ext_version)}" >&2
  exit 1
fi
