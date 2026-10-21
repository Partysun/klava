// Publish an npm package dir idempotently: skip when <name>@<version>
// already exists on the registry (tag/deployment re-runs must not fail
// with npm's 403 "cannot publish over previously published versions"),
// publish otherwise. Runs after scripts/npm-version.js pinned the version
// in <dir>/package.json; invoked from the workspace root by every
// platform pipeline (bash on linux/macOS, PowerShell on Windows).
//
// Usage: node scripts/npm-publish.js <dir> [--wait-deps=<seconds>]
//
// `--wait-deps` is for the meta package only: its optionalDependencies are
// published by the other agents' pipelines, which finish at different times,
// so publishing it immediately could point `npx`/`npm i` at platform packages
// that do not exist yet. Wait (bounded) for them, then publish anyway — the
// release must not hang if one platform build is slow or broken.
const fs = require("fs");
const path = require("path");
const { execSync } = require("child_process");

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function exists(name, version) {
  try {
    const res = await fetch(`https://registry.npmjs.org/${name}/${version}`, { redirect: "manual" });
    if (res.status === 200) return true;
    if (res.status === 404) return false;
    throw new Error(`unexpected registry response ${res.status}`);
  } catch (e) {
    console.error(`cannot reach registry for ${name}@${version}: ${e.message}`);
    throw e;
  }
}

async function waitForDeps(pkg, seconds) {
  if (!pkg.optionalDependencies || seconds <= 0) return;
  const deadline = Date.now() + seconds * 1000;
  const pending = new Set(Object.keys(pkg.optionalDependencies));
  for (;;) {
    for (const name of Array.from(pending)) {
      if (await exists(name, pkg.version)) pending.delete(name);
    }
    if (pending.size === 0) {
      console.log("all platform packages are on the registry");
      return;
    }
    if (Date.now() >= deadline) {
      console.error(
        `warning: still missing after ${seconds}s: ${Array.from(pending).join(", ")} — ` +
          "publishing the meta package anyway; installs on those platforms fail until they land",
      );
      return;
    }
    console.log(`waiting for: ${Array.from(pending).join(", ")}`);
    await sleep(15000);
  }
}

async function main() {
  const args = process.argv.slice(2);
  const dir = args.find((a) => !a.startsWith("--"));
  const waitFlag = args.find((a) => a.startsWith("--wait-deps="));
  const waitSecs = waitFlag ? Number(waitFlag.split("=")[1]) : 0;
  if (!dir) {
    console.error("usage: node scripts/npm-publish.js <dir> [--wait-deps=<seconds>]");
    process.exit(1);
  }
  const pkg = JSON.parse(fs.readFileSync(path.join(dir, "package.json"), "utf8"));
  const tag = `${pkg.name}@${pkg.version}`;

  if (await exists(pkg.name, pkg.version)) {
    console.log(`skip: ${tag} already published`);
    return;
  }

  await waitForDeps(pkg, waitSecs);

  console.log(`publishing ${tag}`);
  // npm has no readme path field (unlike Cargo.toml / pyproject.toml): the
  // registry derives the package README from files inside the tarball.
  // README.md lives next to package.json (npm always packs it) — the npm page
  // has its own README, separate from README.md on PyPI/crates.io.
  execSync("npm publish --access public", { cwd: dir, stdio: "inherit" });
}

main().catch((e) => {
  console.error(e.message);
  process.exit(1);
});
