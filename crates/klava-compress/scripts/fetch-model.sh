#!/usr/bin/env bash
# Fetch the kompress model into the cache directory klava reads.
#
# Manual fallback for `klava-compress --fetch-model`, which is what klava runs
# as the extension's [setup] hook (`klava extensions setup klava-compress`).
# This script needs only curl; use it when the binary cannot be run.
#
# Usage: ./scripts/fetch-model.sh [destination]
#
# Default destination: $XDG_CACHE_HOME/klava/kompress (else ~/.cache/klava/kompress)
set -euo pipefail

repo="chopratejas/kompress-v2-base"
base="https://huggingface.co/${repo}/resolve/main"

if [ "$#" -ge 1 ]; then
  dest="$1"
elif [ -n "${XDG_CACHE_HOME:-}" ]; then
  dest="${XDG_CACHE_HOME}/klava/kompress"
else
  dest="${HOME}/.cache/klava/kompress"
fi

files=(
  "onnx/kompress-fp32.onnx:kompress-fp32.onnx"
  "tokenizer.json:tokenizer.json"
)

mkdir -p "$dest"

for entry in "${files[@]}"; do
  remote="${entry%%:*}"
  local_name="${entry##*:}"
  target="${dest}/${local_name}"

  if [ -s "$target" ]; then
    echo "have  ${target}"
    continue
  fi

  echo "fetch ${local_name}"
  # --location: the HF CDN redirects. --continue-at -: resume a partial file.
  curl --fail --location --continue-at - --output "${target}.part" "${base}/${remote}"
  mv "${target}.part" "$target"
done

echo
echo "Model ready in: ${dest}"
echo "Enable it in klava's config:"
echo
echo "  [extensions.settings.klava-compress]"
echo "  enabled = true"
