#!/usr/bin/env bash
# Print the version the CLI ships as: the [package] version in Cargo.toml.
#
# The naive `sed -n 's/^version = .../p' Cargo.toml | head -1` is wrong here:
# klava's Cargo.toml carries a [workspace.package] version (0.1.x, used by the
# klava-* extension crates) BEFORE the [package] version of the CLI itself.
set -euo pipefail

cd "$(dirname "$0")/.."

awk '
  /^\[package\]/ { in_package = 1; next }
  /^\[/         { in_package = 0 }
  in_package && /^version[[:space:]]*=/ {
    gsub(/[",]/, "", $3)
    print $3
    exit
  }
' Cargo.toml
