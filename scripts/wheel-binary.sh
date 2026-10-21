#!/usr/bin/env bash
# Copy the `klava` console script out of a maturin bin wheel, so the same
# binary that ships on PyPI also ships on npm (one compile per target).
#
# Usage: bash scripts/wheel-binary.sh <wheel> <dest>
set -euo pipefail

wheel="${1:?usage: wheel-binary.sh <wheel> <dest>}"
dest="${2:?usage: wheel-binary.sh <wheel> <dest>}"

test -f "$wheel" || { echo "no such wheel: $wheel" >&2; exit 1; }

# maturin puts a `bindings = "bin"` console script at
# <name>-<version>.data/scripts/<bin>; match loosely so a layout change on
# either OS/arch still lands.
entry="$(unzip -Z1 "$wheel" | grep -E 'data/scripts/klava(\.exe)?$' | head -n 1)" || true
[ -n "$entry" ] || { echo "no console script inside $wheel" >&2; exit 1; }

mkdir -p "$(dirname "$dest")"
unzip -p "$wheel" "$entry" > "$dest"
chmod +x "$dest" 2>/dev/null || true
echo "$wheel -> $dest ($entry)"
