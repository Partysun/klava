#!/usr/bin/env bash
# Linux release build + publish, split into phases so one registry failing
# never blocks the other:
#
#   bash scripts/release-linux.sh build   # wheels (x86_64 + aarch64) + sdist + npm binaries
#   bash scripts/release-linux.sh pypi    # uv publish, skipping files already there
#   bash scripts/release-linux.sh npm     # npm platform packages + meta package
#   bash scripts/release-linux.sh extensions-build # extension npm binaries (x64 + arm64)
#   bash scripts/release-linux.sh extensions       # publish the extension npm packages
#
# Called by the linux steps of .woodpecker/release-linux.yml. All logic lives
# here rather than in the YAML: Woodpecker rewrites inline step commands with
# its envsubst (${...} gets interpolated and can even make the config fail to
# parse), while script files reach bash untouched.
#
# aarch64 is cross-compiled from the amd64 agent with `maturin --zig`
# (ziglang comes from pip). Zig links against its own glibc 2.17 stubs, so the
# aarch64 wheel is manylinux_2_17 like the x86_64 one and no arm64 agent (or
# qemu) is needed.
set -euo pipefail

cd "$(dirname "$0")/.."
phase="${1:?usage: release-linux.sh <build|pypi|npm|extensions-build|extensions>}"

export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"

case "$phase" in
  build)
    bash scripts/check-version.sh

    # zig for the aarch64 cross build. The pip package only ships the
    # compiler inside the module dir, so download the wheel and unpack it
    # (instead of installing into the interpreter: the maturin image is fine
    # with pip, but a PEP 668 host is not) then expose it as a plain `zig` on
    # PATH so maturin (--zig) and the cc-rs builds it wires up agree on one
    # compiler.
    zig_tmp="$(mktemp -d)"
    python3 -m pip download --quiet --disable-pip-version-check \
      --no-deps --dest "$zig_tmp" ziglang
    mkdir -p "$zig_tmp/pkg" "$zig_tmp/bin"
    unzip -q -o "$zig_tmp"/ziglang-*.whl -d "$zig_tmp/pkg"
    ln -sf "$zig_tmp/pkg/ziglang/zig" "$zig_tmp/bin/zig"
    export PATH="$zig_tmp/bin:$PATH"
    zig version
    rustup target add aarch64-unknown-linux-gnu

    # maturin only got a zig toolchain that understands recent zig in 1.15
    # (older ones fail to link: "unsupported linker arg
    # --fix-cortex-a53-843419"). The CI image ships a recent maturin; anything
    # else is a hard error rather than a confusing linker failure.
    maturin_bin="${MATURIN:-maturin}"
    maturin_version="$("$maturin_bin" --version | awk '{print $2}')"
    if [ "$(printf '%s\n1.15\n' "$maturin_version" | sort -V | head -n 1)" != "1.15" ]; then
      echo "maturin $maturin_version is too old for --zig: install maturin >= 1.15" >&2
      exit 1
    fi
    echo "maturin $maturin_version"

    rm -rf dist
    # `--zig` + manylinux2014: the wheel is tagged manylinux_2_17 and audited
    # against zig's glibc stubs instead of the build host's (CentOS 7) libc.
    "$maturin_bin" build --release --locked --zig --compatibility manylinux2014 \
      --target x86_64-unknown-linux-gnu --out dist
    "$maturin_bin" build --release --locked --zig --compatibility manylinux2014 \
      --target aarch64-unknown-linux-gnu --out dist
    "$maturin_bin" sdist --out dist

    # the npm platform binaries are the exact wheels' binaries
    bash scripts/wheel-binary.sh \
      "$(ls dist/*manylinux*_x86_64.whl)" npm/platforms/linux-x64-gnu/klava
    bash scripts/wheel-binary.sh \
      "$(ls dist/*manylinux*_aarch64.whl)" npm/platforms/linux-arm64-gnu/klava

    ./npm/platforms/linux-x64-gnu/klava --version
    file npm/platforms/linux-x64-gnu/klava npm/platforms/linux-arm64-gnu/klava
    ls -l dist
    ;;

  pypi)
    command -v uv >/dev/null || pip install --quiet --disable-pip-version-check uv
    bash scripts/pypi-publish.sh
    ;;

  npm)
    export NPM_TOKEN="${NPM_TOKEN:?NPM_TOKEN is required}"
    bash scripts/publish-npm-platform.sh npm/platforms/linux-x64-gnu
    bash scripts/publish-npm-platform.sh npm/platforms/linux-arm64-gnu
    # the meta package ships last; give the darwin/win32 pipelines a bounded
    # window to publish their platform packages first (see npm-publish.js).
    # npm auth is already in ~/.npmrc from publish-npm-platform.sh.
    node scripts/npm-version.js npm
    node scripts/npm-publish.js npm --wait-deps=600
    ;;

  extensions-build)
    # The extension crates need a different build image than the CLI: they
    # link a glibc floor of 2.34 (ONNX runtime's prebuilt arm64 objects), while
    # the maturin image is CentOS 7 / glibc 2.17. This phase only produces the
    # binaries; `extensions` publishes them from node:22.
    bash scripts/release-extensions.sh build linux-x64-gnu
    bash scripts/release-extensions.sh build linux-arm64-gnu
    ;;

  extensions)
    # Binaries come from the `extensions-build` phase above: this image has
    # node but no cargo.
    export NPM_TOKEN="${NPM_TOKEN:?NPM_TOKEN is required}"
    bash scripts/release-extensions.sh npm linux-x64-gnu linux-arm64-gnu
    ;;

  *)
    echo "unknown phase: $phase" >&2
    exit 2
    ;;
esac
