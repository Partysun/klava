// Port of jigor's @zatsepin/jigor postinstall: locate the native `klava`
// binary for the current platform (a platform package installed as an
// optional dependency, or a local `target/release|debug/klava` dev build)
// and hard-link it over the JS shim next to this file.
const fs = require("fs");
const path = require("path");

const binaryName = process.platform === "win32" ? "klava.exe" : "klava";

// musl and everything else are not packaged yet: klava links the system
// libc (no static musl build), so an Alpine container must install it with
// cargo or pip-from-source instead of npm.
function isMusl() {
  if (process.platform !== "linux") return false;
  try {
    // glibc systems report a runtime version here, musl systems do not.
    const report = process.report && process.report.getReport();
    return !(report && report.header && report.header.glibcVersionRuntime);
  } catch (_) {
    return false;
  }
}

function detectPackageName() {
  const { platform, arch } = process;
  switch (platform) {
    case "darwin":
      // one universal2 fat binary covers Intel (x64) and Apple Silicon (arm64)
      if (arch === "x64" || arch === "arm64") return "@zatsepin/klava-darwin";
      break;
    case "linux":
      if (isMusl()) return null;
      if (arch === "arm64") return "@zatsepin/klava-linux-arm64-gnu";
      if (arch === "x64") return "@zatsepin/klava-linux-x64-gnu";
      break;
    case "win32":
      if (arch === "x64") return "@zatsepin/klava-win32-x64-msvc";
      break;
  }
  return null;
}

function resolveBinaryDir() {
  const pkgName = detectPackageName();
  if (pkgName) {
    try {
      const dir = path.dirname(
        require.resolve(`${pkgName}/package.json`, { paths: [__dirname] }),
      );
      if (fs.existsSync(path.join(dir, binaryName))) return dir;
    } catch (_) {
      // fall through to local dev paths
    }
  }
  for (const profile of ["release", "debug"]) {
    const dir = path.join(__dirname, "..", "target", profile);
    if (fs.existsSync(path.join(dir, binaryName))) return dir;
  }
  return null;
}

function resolveBinaryPath() {
  const dir = resolveBinaryDir();
  return dir ? path.join(dir, binaryName) : null;
}

function installBinary(src, dest) {
  try {
    fs.linkSync(src, dest);
  } catch (_) {
    fs.copyFileSync(src, dest);
  }
}

function main() {
  const sourceDir = resolveBinaryDir();
  if (!sourceDir) {
    console.error(
      "[klava] No native binary found for " +
        `${process.platform ?? "?"}/${process.arch ?? "?"}. ` +
        "Supported: linux x64/arm64 (glibc), macos (universal2: Intel + Apple Silicon), windows x64. " +
        "Musl and other platforms are not packaged yet — use `cargo install klava` there.",
    );
    process.exit(1);
  }

  const src = path.join(sourceDir, binaryName);
  const destBin = path.join(__dirname, binaryName);
  try {
    installBinary(src, destBin);
  } catch (_) {
    console.error("[klava] Failed to move the native binary into place.");
    process.exit(1);
  }
}

module.exports = {
  binaryName,
  detectPackageName,
  resolveBinaryDir,
  resolveBinaryPath,
};

if (require.main === module) {
  main();
}
