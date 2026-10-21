#!/usr/bin/env bash
# Build one npm platform package from a binary and publish it idempotently.
#
# Usage: bash scripts/publish-npm-platform.sh <platform-dir> [binary-path]
#   e.g. bash scripts/publish-npm-platform.sh npm/platforms/linux-x64-gnu target/x86_64-unknown-linux-gnu/release/klava
#
# The binary path may be omitted when the platform dir already holds it (the
# linux flow extracts the binaries straight out of the built wheels).
# Requires NPM_TOKEN in the environment (and node/npm on PATH).
set -euo pipefail

cd "$(dirname "$0")/.."

dir="${1:?usage: publish-npm-platform.sh <platform-dir> [binary-path]}"
binary="${2:-}"

test -d "$dir" || { echo "no such platform package dir: $dir" >&2; exit 1; }

# `klava` on unix, `klava.exe` on windows — the name the platform package
# declares in its `files` list (and the one postinstall resolves).
name="klava"
case "$dir" in
  *win32*) name="klava.exe" ;;
esac

if [ -n "$binary" ]; then
  test -f "$binary" || { echo "no such binary: $binary" >&2; exit 1; }
  if [ "$(cd "$(dirname "$binary")" && pwd)/$(basename "$binary")" != "$(pwd)/$dir/$name" ]; then
    cp "$binary" "$dir/$name"
  fi
fi
test -f "$dir/$name" || { echo "no binary at $dir/$name" >&2; exit 1; }
chmod +x "$dir/$name" 2>/dev/null || true

if ! grep -q "//registry.npmjs.org/:_authToken" ~/.npmrc 2>/dev/null; then
  echo "//registry.npmjs.org/:_authToken=${NPM_TOKEN:?NPM_TOKEN is required}" >> ~/.npmrc
fi

node scripts/npm-version.js "$dir"
node scripts/npm-publish.js "$dir"
