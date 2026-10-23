#!/usr/bin/env bash
# Build and publish the klava extension crates as per-platform npm packages.
#
#   bash scripts/release-extensions.sh build <platform>
#   bash scripts/release-extensions.sh npm   <platform>...
#
# Platforms are the npm keys from scripts/ext-npm.js:
#   linux-x64-gnu linux-arm64-gnu darwin win32-x64-msvc
#
# `build` compiles the three extension crates and leaves the binaries in
# target/ext-npm/<platform>/; `npm` stages a package per crate and publishes it
# idempotently. Build and publish are separate phases (and may run in different
# images) exactly like the CLI's own build/publish steps.
#
# Why there is no meta package with optionalDependencies, unlike @zatsepin/klava:
# `klava extensions install npm:<pkg>` copies the single file named by `bin`
# after `npm pack`, and `npm pack` neither installs dependencies nor runs
# postinstall. A meta package would install a shim with no binary beside it, so
# every package here carries its own prebuilt binary and the platform is part of
# the name.
#
# Versions come from the crates' own manifests (0.1.x) and not from
# CI_COMMIT_TAG: a v0.3.0 tag releases the CLI, and the extensions are versioned
# independently. A crate may pin its own `[package] version`; one that inherits
# `version.workspace = true` uses [workspace.package], so bumping a single
# extension does not move the others.
set -euo pipefail

cd "$(dirname "$0")/.."

phase="${1:?usage: release-extensions.sh <build|npm> <platform>...}"
shift
platforms=("$@")
[ "${#platforms[@]}" -gt 0 ] || { echo "no platforms given" >&2; exit 2; }

crates=(klava-compress klava-pii-guardrail klava-model-escalation)
out_root="target/ext-npm"

export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$HOME/.local/bin:$PATH"

# macOS release steps run with the agent's HOME, which may differ from the
# login shell's: point rustup at the real user's homes (same as
# release-macos.sh) or the toolchain is invisible. HOME itself matters too:
# `ort-sys` caches the ONNX Runtime archives it downloads under
# $HOME/Library/Caches, and the npm phase writes ~/.npmrc, so a step HOME that
# is not the user's home makes both land in the wrong place.
if [ "$(uname -s)" = Darwin ]; then
  user_home="$(eval echo "~$(id -un)")"
  export HOME="$user_home"
  export RUSTUP_HOME="$user_home/.rustup"
  export CARGO_HOME="$user_home/.cargo"
  export MACOSX_DEPLOYMENT_TARGET=11.0
  export PATH="$CARGO_HOME/bin:$HOME/.local/bin:$PATH"
fi

# Debian/Ubuntu build images (rust:bookworm) ship no C toolchain deps for the
# extension crates: `ort-sys` fetches prebuilt ONNX binaries but the aarch64
# ones link a GNU libstdc++ (whose ABI zig's libc++ cannot satisfy, so the cross
# build needs a real GNU cross toolchain), and ureq's build dependency
# native-tls needs OpenSSL headers.
ensure_linux_deps() {
  local missing=()
  command -v pkg-config >/dev/null 2>&1 || missing+=(pkg-config)
  [ -f /usr/include/openssl/ssl.h ] || missing+=(libssl-dev)
  if [ "${1:-}" = arm64 ] && ! command -v aarch64-linux-gnu-g++ >/dev/null 2>&1; then
    missing+=(g++-aarch64-linux-gnu)
  fi
  [ "${#missing[@]}" -gt 0 ] || return 0
  apt-get update -qq
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${missing[@]}"
}

build() {
  local platform="$1"
  local triple ext

  # The Windows agent has no bash, so its step builds and publishes through
  # scripts/release-windows.ps1 instead.
  if [ "$platform" = win32-x64-msvc ]; then
    echo "win32-x64-msvc is built and published by release-windows.ps1" >&2
    exit 2
  fi

  # A target dir of its own: the extension step runs in a different image (and
  # rustc) than the CLI's build step, and two toolchains sharing target/ would
  # invalidate each other's cache and serialize on cargo's lock.
  export CARGO_TARGET_DIR="$PWD/target/ext-build/$platform"

  case "$platform" in
    linux-x64-gnu)
      triple=x86_64-unknown-linux-gnu
      ensure_linux_deps
      rustup target add "$triple" >/dev/null
      ;;
    linux-arm64-gnu)
      triple=aarch64-unknown-linux-gnu
      ensure_linux_deps arm64
      rustup target add "$triple" >/dev/null
      export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
      export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
      export CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++
      export AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
      ;;
    darwin)
      # one universal2 binary per crate, the shape the CLI's npm package uses
      rustup target add x86_64-apple-darwin aarch64-apple-darwin >/dev/null
      ;;
    *)
      echo "unknown platform: $platform" >&2
      exit 2
      ;;
  esac

  local dest="$out_root/$platform"
  rm -rf "$dest"
  mkdir -p "$dest"

  # `ort-sys` asks pkg-config for libonnxruntime *before* falling back to its
  # static download, and accepts any version >= 1.22. A build machine with a
  # shared ONNX runtime installed (Homebrew on macOS, distro packages on linux)
  # would then link it dynamically, which a single-file npm package cannot
  # carry. Shadow it with a deliberately stale .pc so ort-sys takes the
  # documented "out of date, will be ignored" path and statically links the
  # prebuilt binaries it downloads. Other libraries are unaffected: entries in
  # PKG_CONFIG_PATH are searched before the system directories.
  local pc_dir="$CARGO_TARGET_DIR/pkgconfig"
  mkdir -p "$pc_dir"
  printf 'Name: onnxruntime\nDescription: shadowed, see scripts/release-extensions.sh\nVersion: 1.0.0\nLibs:\nCflags:\n' \
    >"$pc_dir/libonnxruntime.pc"
  export PKG_CONFIG_PATH="$pc_dir${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
  unset ORT_LIB_LOCATION

  if [ "$platform" = darwin ]; then
    for ext in "${crates[@]}"; do
      cargo build --release --locked -p "$ext" --target x86_64-apple-darwin
      cargo build --release --locked -p "$ext" --target aarch64-apple-darwin
      lipo -create \
        "$CARGO_TARGET_DIR/x86_64-apple-darwin/release/$ext" \
        "$CARGO_TARGET_DIR/aarch64-apple-darwin/release/$ext" \
        -output "$dest/$ext"
      archs="$(lipo -archs "$dest/$ext")"
      echo "$ext architectures: $archs"
    done
  else
    # One cargo invocation for all three crates shares the dependency build.
    local packages=()
    for ext in "${crates[@]}"; do packages+=(-p "$ext"); done
    cargo build --release --locked --target "$triple" "${packages[@]}"
    for ext in "${crates[@]}"; do
      local binary="$ext"
      cp "$CARGO_TARGET_DIR/$triple/release/$binary" "$dest/$binary"
      # the extension only runs when the file is executable
      chmod +x "$dest/$binary" 2>/dev/null || true
    done
  fi

  # `klava extensions install npm:` copies exactly one file out of the package,
  # so a binary that dlopen()s or links a shared ONNX runtime would arrive
  # broken. `ort`'s download-binaries links it statically (confirmed by ldd on
  # linux); assert that instead of trusting it.
  for ext in "${crates[@]}"; do
    if [ "$platform" = darwin ]; then
      if otool -L "$dest/$ext" | grep -qi onnxruntime; then
        echo "$ext links a shared ONNX runtime: a single-file npm package cannot work" >&2
        exit 1
      fi
    elif command -v ldd >/dev/null 2>&1; then
      if ldd "$dest/$ext" 2>/dev/null | grep -qi onnxruntime; then
        echo "$ext links a shared ONNX runtime: a single-file npm package cannot work" >&2
        exit 1
      fi
    fi
  done

  # Smoke-check the binaries by asking for their manifest: that is what klava
  # does right after installing one, and it catches a truncated artifact. Only
  # when the host can execute the target: the arm64 binaries are cross-built on
  # the amd64 agent and the release image has no qemu.
  local host_arch runnable=0
  host_arch="$(uname -m)"
  case "$platform" in
    linux-x64-gnu) [ "$host_arch" = x86_64 ] && runnable=1 ;;
    linux-arm64-gnu) [ "$host_arch" = aarch64 ] && runnable=1 ;;
    darwin) [ "$(uname -s)" = Darwin ] && runnable=1 ;;
  esac
  if [ "$runnable" = 1 ]; then
    for ext in "${crates[@]}"; do
      "$dest/$ext" --print-manifest >/dev/null
    done
    echo "smoke: every $platform binary printed a manifest"
  else
    file "$dest"/*
  fi
  ls -l "$dest"
}

publish() {
  export NPM_TOKEN="${NPM_TOKEN:?NPM_TOKEN is required}"
  if ! grep -q "//registry.npmjs.org/:_authToken" ~/.npmrc 2>/dev/null; then
    echo "//registry.npmjs.org/:_authToken=${NPM_TOKEN}" >> ~/.npmrc
  fi

  local platform dir
  for platform in "${platforms[@]}"; do
    # Stage from the binaries the build phase left behind (or, on Windows,
    # from target/release since the same step built them).
    local bin_dir="$out_root/$platform"
    node scripts/ext-npm.js prepare "$platform" "$bin_dir"
    while IFS= read -r dir; do
      node scripts/npm-publish.js "$dir"
    done < <(node scripts/ext-npm.js dirs "$platform")
  done
}

case "$phase" in
  build) build "${platforms[0]}" ;;
  npm) publish ;;
  *)
    echo "unknown phase: $phase" >&2
    exit 2
    ;;
esac
