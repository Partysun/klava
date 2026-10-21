// Stage (and describe) the per-platform npm packages of the klava extension
// crates.
//
// Why one package per platform and no meta package: `klava extensions install
// npm:<pkg>` runs `npm pack`, extracts the tarball and installs the single file
// named by `bin`. `npm pack` never installs dependencies and never runs
// postinstall, so a meta package that pulls a platform package through
// optionalDependencies would install a JS shim with no binary next to it. Each
// published package therefore carries its own prebuilt binary and is directly
// installable:
//
//   klava extensions install npm:@zatsepin/klava-compress-linux-x64-gnu
//
// Usage:
//   node scripts/ext-npm.js prepare <platform> <bin-dir>
//   node scripts/ext-npm.js dirs <platform>          # staged dirs, one per line
//   node scripts/ext-npm.js versions                 # "<ext> <version>" lines
//
// `prepare` copies <bin-dir>/<binary> into npm/extensions/stage/<platform>/
// <package>/ and writes that package's package.json + README. It only needs
// node: the binaries are built beforehand by scripts/release-extensions.sh
// (or scripts/release-windows.ps1), which is why the build and publish steps
// can run in different images.
const fs = require("fs");
const path = require("path");

const root = path.join(__dirname, "..");
const stageRoot = path.join(root, "npm", "extensions", "stage");

// The three extension crates, and how each one presents itself on npm.
const extensions = [
  {
    crate: "klava-compress",
    tagline: "context compression extension for klava",
    keywords: ["compress", "context", "tokens"],
  },
  {
    crate: "klava-pii-guardrail",
    tagline: "PII guardrail extension for klava",
    keywords: ["pii", "redaction", "guardrail"],
  },
  {
    crate: "klava-model-escalation",
    tagline: "model escalation extension for klava",
    keywords: ["model", "escalation", "routing"],
  },
];

// npm platform key -> the metadata the registry and `klava` look at.
const platforms = {
  "linux-x64-gnu": {
    os: ["linux"],
    cpu: ["x64"],
    libc: ["glibc"],
    label: "linux x64 glibc",
    exe: false,
  },
  "linux-arm64-gnu": {
    os: ["linux"],
    cpu: ["arm64"],
    libc: ["glibc"],
    label: "linux arm64 glibc",
    exe: false,
  },
  darwin: {
    os: ["darwin"],
    // one universal2 binary serves both slices
    cpu: ["x64", "arm64"],
    libc: undefined,
    label: "macOS universal2, Apple Silicon and Intel",
    exe: false,
  },
  "win32-x64-msvc": {
    os: ["win32"],
    cpu: ["x64"],
    libc: undefined,
    label: "Windows x64",
    exe: true,
  },
};

function section(text, name) {
  const parts = text.split(/^\[/m);
  const part = parts.find((p) => p.startsWith(`${name}]`));
  return part || "";
}

function workspaceVersion() {
  const text = fs.readFileSync(path.join(root, "Cargo.toml"), "utf8");
  const m = section(text, "workspace.package").match(
    /^\s*version\s*=\s*"([^"]+)"/m,
  );
  if (!m) throw new Error("cannot read [workspace.package] version from Cargo.toml");
  return m[1];
}

// The extension's own version (0.1.x, inherited from [workspace.package]).
// Deliberately not CI_COMMIT_TAG: a v0.3.0 tag releases the CLI, not the
// extensions, and re-running it must republish nothing.
function crateVersion(crate) {
  const text = fs.readFileSync(
    path.join(root, "crates", crate, "Cargo.toml"),
    "utf8",
  );
  const pkg = section(text, "package");
  const explicit = pkg.match(/^\s*version\s*=\s*"([^"]+)"/m);
  if (explicit) return explicit[1];
  if (/^\s*version\.workspace\s*=\s*true/m.test(pkg)) return workspaceVersion();
  throw new Error(`cannot determine the version of ${crate}`);
}

function crateDescription(crate) {
  const text = fs.readFileSync(
    path.join(root, "crates", crate, "Cargo.toml"),
    "utf8",
  );
  const m = section(text, "package").match(/^\s*description\s*=\s*"([^"]+)"/m);
  return m ? m[1] : "";
}

function packageName(ext, platform) {
  return `@zatsepin/${ext}-${platform}`;
}

function binaryName(ext, platformKey) {
  return platforms[platformKey].exe ? `${ext}.exe` : ext;
}

function prepare(platformKey, binDir) {
  const platform = platforms[platformKey];
  if (!platform) {
    throw new Error(
      `unknown platform '${platformKey}' (known: ${Object.keys(platforms).join(", ")})`,
    );
  }

  const platformStage = path.join(stageRoot, platformKey);
  fs.rmSync(platformStage, { recursive: true, force: true });
  fs.mkdirSync(platformStage, { recursive: true });

  for (const ext of extensions) {
    const binary = binaryName(ext.crate, platformKey);
    const source = path.join(binDir, binary);
    if (!fs.existsSync(source)) {
      throw new Error(`no ${binary} in ${binDir}: build it before preparing npm packages`);
    }

    const dir = path.join(platformStage, packageName(ext.crate, platformKey).split("/")[1]);
    fs.mkdirSync(dir, { recursive: true });
    fs.copyFileSync(source, path.join(dir, binary));
    if (!platform.exe) fs.chmodSync(path.join(dir, binary), 0o755);

    // The npm page shows README.md from the tarball; use the crate's own.
    const readme = path.join(root, "crates", ext.crate, "README.md");
    if (fs.existsSync(readme)) {
      fs.copyFileSync(readme, path.join(dir, "README.md"));
    }

    const description = crateDescription(ext.crate);
    const pkg = {
      name: packageName(ext.crate, platformKey),
      version: crateVersion(ext.crate),
      // The crate description already names the extension and ends with a
      // period; append the platform parenthetically.
      description: `${(description || ext.tagline).replace(/\.$/, "")} (${platform.label})`,
      homepage: "https://github.com/Partysun/klava",
      repository: {
        type: "git",
        url: "git+https://github.com/Partysun/klava.git",
      },
      license: "Apache-2.0",
      keywords: ["klava", "klava-extension", ...ext.keywords],
      os: platform.os,
      cpu: platform.cpu,
      ...(platform.libc ? { libc: platform.libc } : {}),
      // `klava extensions install npm:<pkg>` installs exactly this one file.
      files: [binary],
      // A map, not a string: the string form derives the installed name from
      // the package name (@zatsepin/klava-compress-linux-x64-gnu), which would
      // land in the extensions dir under the platform-suffixed name.
      bin: { [binary]: binary },
      publishConfig: {
        registry: "https://registry.npmjs.org/",
        access: "public",
      },
      bugs: { url: "https://github.com/Partysun/klava/issues" },
    };
    fs.writeFileSync(
      path.join(dir, "package.json"),
      JSON.stringify(pkg, null, 2) + "\n",
    );
    console.log(`${pkg.name}@${pkg.version} <- ${source}`);
  }
  return platformStage;
}

function dirs(platformKey) {
  const platformStage = path.join(stageRoot, platformKey);
  if (!fs.existsSync(platformStage)) {
    throw new Error(`nothing staged for ${platformKey}; run prepare first`);
  }
  for (const entry of fs.readdirSync(platformStage).sort()) {
    const dir = path.join(platformStage, entry);
    if (fs.existsSync(path.join(dir, "package.json"))) console.log(dir);
  }
}

const [command, ...rest] = process.argv.slice(2);
try {
  switch (command) {
    case "prepare":
      prepare(rest[0], rest[1]);
      break;
    case "dirs":
      dirs(rest[0]);
      break;
    case "versions":
      for (const ext of extensions) console.log(`${ext.crate} ${crateVersion(ext.crate)}`);
      break;
    default:
      console.error(
        "usage: node scripts/ext-npm.js <prepare <platform> <bin-dir> | dirs <platform> | versions>",
      );
      process.exit(2);
  }
} catch (e) {
  console.error(e.message);
  process.exit(1);
}
