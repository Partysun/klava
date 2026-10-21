#!/usr/bin/env bash
# macOS release build + publish (single darwin agent):
#
#   bash scripts/release-macos.sh build   # universal2 wheel + npm fat binary
#   bash scripts/release-macos.sh pypi    # uv publish, skipping files already there
#   bash scripts/release-macos.sh npm     # npm darwin platform package
#
# Called by the darwin steps of .woodpecker/release-macos.yml. All logic lives
# here rather than in the YAML: Woodpecker rewrites inline step commands with
# its envsubst (${...} gets interpolated and can even make the config fail to
# parse), while script files reach bash untouched.
#
# maturin builds x86_64-apple-darwin and aarch64-apple-darwin separately and
# lipo-merges them into one `macosx_*_universal2` wheel, so an Intel agent
# covers Apple Silicon too. The npm package gets the same fat binary.
set -euo pipefail

cd "$(dirname "$0")/.."
phase="${1:?usage: release-macos.sh <build|pypi|npm>}"

# The Woodpecker step does not inherit the login shell's environment: HOME can
# point somewhere else, so rustup finds no settings.toml and fails with "no
# default is configured" even though `rustup show` in a terminal lists stable.
# Point both homes at the real user's (~$(id -un)), like jigor does.
if [ "$(uname -s)" = Darwin ]; then
  user_home="$(eval echo "~$(id -un)")"
  export RUSTUP_HOME="$user_home/.rustup"
  export CARGO_HOME="$user_home/.cargo"
  # the app targets macOS 11+
  export MACOSX_DEPLOYMENT_TARGET=11.0
fi
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$HOME/.local/bin:$PATH"

# uv and maturin are not preinstalled on the agent: prefer what is there and
# fall back to pip/cargo, which works without root.
have_uv() { command -v uv >/dev/null 2>&1; }
ensure_uv() {
  have_uv && return 0
  python3 -m pip install --quiet --user uv >/dev/null 2>&1 ||
    python3 -m pip install --quiet --user --break-system-packages uv >/dev/null 2>&1 ||
    cargo install --quiet uv
}
ensure_maturin() {
  command -v maturin >/dev/null 2>&1 && return 0
  ensure_uv
  uv tool install --quiet maturin || true
  export PATH="$HOME/.local/bin:$PATH"
  command -v maturin >/dev/null 2>&1 || cargo install --quiet maturin
}

case "$phase" in
  build)
    bash scripts/check-version.sh

    # the toolchain must be resolvable from the agent's environment
    rustup default stable
    rustup show active-toolchain
    ensure_uv
    ensure_maturin
    # both slices of the universal2 build
    rustup target add x86_64-apple-darwin aarch64-apple-darwin

    rm -rf dist
    # `--universal2` is not a maturin flag (the old pipeline used it and
    # therefore never produced a macOS wheel since 0.2.0): universal2 is a
    # target, built as two per-arch slices and lipo-merged by maturin.
    maturin build --release --locked \
      --target universal2-apple-darwin --out dist

    # the npm binary is the exact fat binary the wheel carries
    bash scripts/wheel-binary.sh \
      "$(ls dist/*universal2.whl)" npm/platforms/darwin/klava
    # `lipo -archs` prints the slices in no fixed order
    archs="$(lipo -archs npm/platforms/darwin/klava)"
    echo "npm binary architectures: $archs"
    case "$archs" in
      *x86_64*arm64* | *arm64*x86_64*) ;;
      *)
        echo "npm binary is not universal2: $archs" >&2
        exit 1
        ;;
    esac
    ./npm/platforms/darwin/klava --version
    ls -l dist
    ;;

  pypi)
    ensure_uv
    bash scripts/pypi-publish.sh
    ;;

  npm)
    export NPM_TOKEN="${NPM_TOKEN:?NPM_TOKEN is required}"
    bash scripts/publish-npm-platform.sh npm/platforms/darwin
    ;;

  *)
    echo "unknown phase: $phase" >&2
    exit 2
    ;;
esac
