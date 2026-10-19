//! Installing extensions from a source.
//!
//! A source is one of:
//!
//! | Form | Meaning |
//! |---|---|
//! | `./path`, `/abs/path` | a local executable (or a symlink to one) |
//! | `cargo:<crate>[@version]` | build and install with `cargo install` |
//! | `cargo-git:<url>[#rev]` | as above, from a git repository |
//! | `npm:<package>[@version]` | unpack with `npm pack` and take the binary |
//!
//! Every install is **staged then moved**: the artifact is prepared in a
//! temporary directory next to the destination, verified, and only then renamed
//! into place. A failed or interrupted install cannot leave a half-written
//! executable where klava would run it.
//!
//! Installing runs a build toolchain for `cargo:` and `npm:` sources, on code the
//! user just named. That is why the prefix is explicit — klava never guesses —
//! and why the exact command is printed before it runs.

use crate::extensions;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The SHA-256 of a file, hex encoded.
///
/// Implemented here rather than pulling in `sha2`, which is an optional
/// dependency tied to the qwen/klava features. This is only used to fingerprint
/// an artifact, never for authentication protocols, so a small correct
/// implementation avoids widening the default feature set.
pub fn sha256_of(path: &Path) -> Result<String> {
    use std::io::Read;

    let mut file =
        std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut state = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        state.update(&buffer[..read]);
    }
    Ok(state.finish_hex())
}

/// Minimal SHA-256 (FIPS 180-4), used only for artifact fingerprints.
struct Sha256 {
    state: [u32; 8],
    buffer: Vec<u8>,
    length: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: Vec::with_capacity(64),
            length: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        if !self.buffer.is_empty() {
            let need = 64 - self.buffer.len();
            let take = need.min(data.len());
            self.buffer.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buffer.len() == 64 {
                let block = self.buffer.clone();
                self.compress(&block);
                self.buffer.clear();
            }
        }
        while data.len() >= 64 {
            self.compress(&data[..64]);
            data = &data[64..];
        }
        self.buffer.extend_from_slice(data);
    }

    fn finish_hex(mut self) -> String {
        let bit_len = self.length.wrapping_mul(8);
        self.buffer.push(0x80);
        while self.buffer.len() % 64 != 56 {
            self.buffer.push(0);
        }
        self.buffer.extend_from_slice(&bit_len.to_be_bytes());
        let blocks = self.buffer.clone();
        for chunk in blocks.chunks(64) {
            self.compress(chunk);
        }

        let mut out = String::with_capacity(64);
        for word in self.state {
            out.push_str(&format!("{word:08x}"));
        }
        out
    }

    fn compress(&mut self, block: &[u8]) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];

        let mut w = [0u32; 64];
        for (i, chunk) in block.chunks(4).take(16).enumerate() {
            w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

/// Provenance recorded next to the artifact, as `<name>.source.toml`.
///
/// Kept beside the executable rather than in user config so that provenance and
/// artifact travel together: copy the extension somewhere else and its origin
/// goes with it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRecord {
    /// The source string exactly as the user typed it.
    pub source: String,
    /// Resolved version, when the source had one.
    #[serde(default)]
    pub version: Option<String>,
    pub sha256: String,
    /// RFC 3339 timestamp.
    pub installed_at: String,
}

impl SourceRecord {
    /// Path of the record for `name` inside `dir`.
    pub fn path_for(dir: &Path, name: &str) -> PathBuf {
        dir.join(format!("{name}.source.toml"))
    }

    /// Read the record for `name`, if any.
    pub fn load(dir: &Path, name: &str) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path_for(dir, name)).ok()?;
        toml::from_str(&text).ok()
    }

    /// Write the record atomically.
    pub fn save(&self, dir: &Path, name: &str) -> Result<()> {
        let path = Self::path_for(dir, name);
        let text = toml::to_string_pretty(self)?;
        let temp = path.with_extension("toml.tmp");
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    }
}

/// What an extension installs as.
#[derive(Debug, Clone)]
pub struct Install {
    /// Extension name (file stem at the destination).
    pub name: String,
    /// Where the executable landed.
    pub path: PathBuf,
    pub record: SourceRecord,
}

/// A parsed install source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A local file or directory holding the executable.
    Local(PathBuf),
    /// `cargo:<crate>[@version]`
    Cargo {
        krate: String,
        version: Option<String>,
    },
    /// `cargo-git:<url>[#rev]`
    CargoGit { url: String, rev: Option<String> },
    /// `cargo-path:<dir>` — build and install a local crate.
    CargoPath(PathBuf),
    /// `npm:<package>[@version]`
    Npm {
        package: String,
        version: Option<String>,
    },
}

impl Source {
    /// Parse a source string. The prefix selects the mechanism, so a typo is an
    /// error rather than a silent fallback to "copy this path".
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("empty source");
        }

        if let Some(rest) = raw.strip_prefix("cargo-path:") {
            let path = PathBuf::from(rest);
            if !path.exists() {
                bail!("cargo-path:{} does not exist", path.display());
            }
            return Ok(Source::CargoPath(path));
        }

        if let Some(rest) = raw.strip_prefix("cargo-git:") {
            let (url, rev) = match rest.split_once('#') {
                Some((url, rev)) => (url.to_string(), Some(rev.to_string())),
                None => (rest.to_string(), None),
            };
            if url.is_empty() {
                bail!("cargo-git: needs a repository URL");
            }
            return Ok(Source::CargoGit { url, rev });
        }

        if let Some(rest) = raw.strip_prefix("cargo:") {
            let (krate, version) = split_version(rest);
            if krate.is_empty() {
                bail!("cargo: needs a crate name");
            }
            return Ok(Source::Cargo { krate, version });
        }

        if let Some(rest) = raw.strip_prefix("npm:") {
            let (package, version) = split_version(rest);
            if package.is_empty() {
                bail!("npm: needs a package name");
            }
            return Ok(Source::Npm { package, version });
        }

        // Anything else must be a path that exists. This catches `foo` (a typo)
        // with a clear message instead of treating it as a local file.
        let path = PathBuf::from(raw);
        if path.exists() {
            return Ok(Source::Local(path));
        }
        bail!(
            "no such path: {raw}\n\
             expected a path, or one of cargo:<crate>, cargo-git:<url>, npm:<package>"
        )
    }

    /// A short label for logs.
    pub fn label(&self) -> String {
        match self {
            Source::Local(p) => p.display().to_string(),
            Source::Cargo { krate, version } => match version {
                Some(v) => format!("cargo:{krate}@{v}"),
                None => format!("cargo:{krate}"),
            },
            Source::CargoGit { url, rev } => match rev {
                Some(r) => format!("cargo-git:{url}#{r}"),
                None => format!("cargo-git:{url}"),
            },
            Source::CargoPath(path) => format!("cargo-path:{}", path.display()),
            Source::Npm { package, version } => match version {
                Some(v) => format!("npm:{package}@{v}"),
                None => format!("npm:{package}"),
            },
        }
    }
}

/// Split `name@version`, treating a leading `@` (npm scopes) as part of the name.
fn split_version(raw: &str) -> (String, Option<String>) {
    // `@scope/pkg@1.2.3` → name `@scope/pkg`, version `1.2.3`
    if let Some(index) = raw.rfind('@')
        && index > 0
    {
        return (raw[..index].to_string(), Some(raw[index + 1..].to_string()));
    }
    (raw.to_string(), None)
}

/// Install `source` into `dest_dir`, returning what was installed.
///
/// The caller is responsible for the trust gate; this only does the mechanical
/// work and verifies that something executable actually arrived.
pub fn install(source: &Source, dest_dir: &Path, name_override: Option<&str>) -> Result<Install> {
    std::fs::create_dir_all(dest_dir)?;

    let staging = dest_dir.join(".staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    let result = match source {
        Source::Local(path) => install_local(path, &staging, dest_dir, name_override),
        Source::Cargo { krate, version } => install_cargo(
            krate,
            version.as_deref(),
            None,
            &staging,
            dest_dir,
            name_override,
        ),
        Source::CargoGit { url, rev } => install_cargo(
            "",
            None,
            Some(CargoSource::Git {
                url,
                rev: rev.as_deref(),
            }),
            &staging,
            dest_dir,
            name_override,
        ),
        Source::CargoPath(path) => install_cargo(
            "",
            None,
            Some(CargoSource::Path(path)),
            &staging,
            dest_dir,
            name_override,
        ),
        Source::Npm { package, version } => install_npm(
            package,
            version.as_deref(),
            &staging,
            dest_dir,
            name_override,
        ),
    };

    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// Resolve the executable inside `path`.
///
/// A directory is accepted when it contains exactly one obvious candidate:
/// `<dir>/<dirname>` or a single executable file.
fn resolve_local_executable(path: &Path) -> Result<PathBuf> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve {}", path.display()))?;

    if path.is_file() {
        return Ok(path);
    }

    let dir_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let named = path.join(dir_name);
    if named.is_file() {
        return Ok(named);
    }

    let executables: Vec<PathBuf> = std::fs::read_dir(&path)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_executable(p))
        .collect();

    match executables.len() {
        1 => Ok(executables.into_iter().next().unwrap()),
        0 => bail!(
            "no executable found in {}; pass the file itself",
            path.display()
        ),
        _ => bail!(
            "several executables in {}; pass the one you want",
            path.display()
        ),
    }
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn install_local(
    source: &Path,
    staging: &Path,
    dest_dir: &Path,
    name_override: Option<&str>,
) -> Result<Install> {
    let executable = resolve_local_executable(source)?;
    let name = name_override
        .map(str::to_string)
        .or_else(|| {
            executable
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow::anyhow!("cannot derive a name; use --name"))?;

    let staged = staging.join(&name);
    // Preserve a symlink so a package-manager-managed binary stays linked to its
    // source; otherwise copy the bytes.
    let is_symlink = std::fs::symlink_metadata(&executable)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if is_symlink {
        copy_symlink(&executable, &staged)?;
    } else {
        std::fs::copy(&executable, &staged)?;
    }
    make_executable(&staged)?;

    // A neighbouring manifest, if the user pointed at a directory.
    if let Some(parent) = executable.parent()
        && let Ok(entries) = std::fs::read_dir(parent)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("toml")
                && path.file_stem().and_then(|s| s.to_str()) == Some(name.as_str())
            {
                std::fs::copy(&path, staging.join(format!("{name}.toml")))?;
            }
        }
    }

    commit(staging, &name, dest_dir, name.clone(), None)
}

fn copy_symlink(from: &Path, to: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let target = std::fs::read_link(from)?;
        std::os::unix::fs::symlink(target, to)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::copy(from, to)?;
        Ok(())
    }
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(perms.mode() | 0o755);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Where `cargo install` should take the crate from.
enum CargoSource<'a> {
    /// A git URL, optionally pinned to a revision.
    Git { url: &'a str, rev: Option<&'a str> },
    /// A local crate directory.
    Path(&'a Path),
}

fn install_cargo(
    krate: &str,
    version: Option<&str>,
    source: Option<CargoSource<'_>>,
    staging: &Path,
    dest_dir: &Path,
    name_override: Option<&str>,
) -> Result<Install> {
    let root = staging.join("cargo-root");
    let mut command = Command::new("cargo");
    command
        .arg("install")
        .arg("--root")
        .arg(&root)
        .arg("--force");

    match source {
        Some(CargoSource::Git { url, rev }) => {
            command.arg("--git").arg(url);
            if let Some(rev) = rev {
                command.arg("--rev").arg(rev);
            }
        }
        Some(CargoSource::Path(path)) => {
            command.arg("--path").arg(path);
        }
        None => {
            command.arg(krate);
        }
    }
    if let Some(version) = version {
        command.arg("--version").arg(version);
    }

    run_visible(&mut command)?;

    let bin_dir = root.join("bin");
    let executables: Vec<PathBuf> = std::fs::read_dir(&bin_dir)
        .with_context(|| format!("cargo produced no bin directory at {}", bin_dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();

    if executables.is_empty() {
        bail!("cargo install produced no binary");
    }
    if executables.len() > 1 && name_override.is_none() {
        let names: Vec<String> = executables
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect();
        bail!(
            "cargo install produced several binaries ({}); choose one with --name",
            names.join(", ")
        );
    }

    let chosen = match name_override {
        Some(name) => executables
            .iter()
            .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(name))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("cargo produced no binary named '{name}'"))?,
        None => executables.into_iter().next().unwrap(),
    };

    let name = name_override
        .map(str::to_string)
        .or_else(|| {
            chosen
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow::anyhow!("cannot derive a name; use --name"))?;

    let staged = staging.join(&name);
    std::fs::copy(&chosen, &staged)?;
    make_executable(&staged)?;

    commit(
        staging,
        &name,
        dest_dir,
        name.clone(),
        version.map(str::to_string),
    )
}

fn install_npm(
    package: &str,
    version: Option<&str>,
    staging: &Path,
    dest_dir: &Path,
    name_override: Option<&str>,
) -> Result<Install> {
    let spec = match version {
        Some(version) => format!("{package}@{version}"),
        None => package.to_string(),
    };

    let pack_dir = staging.join("npm-pack");
    std::fs::create_dir_all(&pack_dir)?;

    let mut command = Command::new("npm");
    command
        .arg("pack")
        .arg(&spec)
        .arg("--pack-destination")
        .arg(&pack_dir);
    run_visible(&mut command)?;

    let tarball = std::fs::read_dir(&pack_dir)?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("tgz"))
        .ok_or_else(|| anyhow::anyhow!("npm pack produced no tarball"))?;

    let extract_dir = staging.join("npm-extract");
    std::fs::create_dir_all(&extract_dir)?;

    // `tar` ships with every platform klava targets except a bare Windows, where
    // bsdtar is present in modern installs.
    let mut tar = Command::new("tar");
    tar.arg("-xzf").arg(&tarball).arg("-C").arg(&extract_dir);
    run_visible(&mut tar)?;

    let package_dir = extract_dir.join("package");
    let package_json = package_dir.join("package.json");
    let metadata: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&package_json)
            .with_context(|| format!("no package.json in {}", package_dir.display()))?,
    )?;

    // npm's `bin` field is either a string (name = package name) or a map.
    let (bin_name, bin_rel) = match metadata.get("bin") {
        Some(serde_json::Value::String(rel)) => {
            let base = package.rsplit('/').next().unwrap_or(package);
            (base.to_string(), rel.clone())
        }
        Some(serde_json::Value::Object(map)) => {
            let (name, rel) = map
                .iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("package.json has an empty bin map"))?;
            (
                name.clone(),
                rel.as_str()
                    .ok_or_else(|| anyhow::anyhow!("bin entry is not a string"))?
                    .to_string(),
            )
        }
        _ => bail!("npm package '{package}' declares no bin; it is not an executable extension"),
    };

    let executable = package_dir.join(bin_rel.trim_start_matches("./"));
    if !executable.is_file() {
        bail!(
            "npm package declares bin '{bin_name}' but {} is missing",
            executable.display()
        );
    }

    let name = name_override.map(str::to_string).unwrap_or(bin_name);
    let staged = staging.join(&name);
    std::fs::copy(&executable, &staged)?;
    make_executable(&staged)?;

    commit(
        staging,
        &name,
        dest_dir,
        name.clone(),
        version.map(str::to_string),
    )
}

/// Ask a freshly installed binary for a manifest and store it beside it.
///
/// The manifest is the extension's own business, but its `[setup]` hook is how
/// klava learns to provision the extension after install. A package-manager
/// install only copies the executable, so the only way to see a manifest the
/// author shipped is to let the binary print it.
///
/// Best effort by design: a missing flag, a timeout, invalid TOML or a non-zero
/// exit all mean "no manifest", never a failed install. An existing manifest —
/// including one the user edited — is never overwritten.
fn adopt_manifest(binary: &Path, name: &str, dest_dir: &Path) {
    let manifest = dest_dir.join(format!("{name}.toml"));
    if manifest.exists() {
        return;
    }
    write_manifest(binary, name, &manifest);
}

/// Re-probe an installed binary and rewrite its manifest in place.
///
/// Separate from `adopt_manifest` because it deliberately overwrites: it is the
/// explicit user request to refresh an extension installed before its manifest
/// declared anything. Returns false when no valid manifest was printed, so the
/// caller can leave the old file alone.
fn refresh_manifest(binary: &Path, name: &str, dest_dir: &Path) -> bool {
    let manifest = dest_dir.join(format!("{name}.toml"));
    if !binary.exists() {
        return false;
    }
    write_manifest(binary, name, &manifest)
}

/// Probe, validate, and atomically write `manifest`. Returns whether it landed.
fn write_manifest(binary: &Path, name: &str, manifest: &Path) -> bool {
    let Some(text) = probe_manifest(binary) else {
        return false;
    };

    // `toml::from_str`, not `str::parse`: `Value`'s `FromStr` impl in toml 1.x
    // rejects every document ("unexpected content"), which silently discarded
    // every manifest a binary printed.
    match toml::from_str::<toml::Value>(&text) {
        Ok(_) => {
            let temp = manifest.with_extension("toml.tmp");
            if std::fs::write(&temp, text).is_ok() && std::fs::rename(&temp, manifest).is_err() {
                let _ = std::fs::remove_file(&temp);
                return false;
            }
            true
        }
        Err(e) => {
            tracing::debug!("extension '{name}': --print-manifest produced invalid TOML: {e}");
            false
        }
    }
}

/// Run `<binary> --print-manifest` with a short deadline.
///
/// This happens once, at install time, so a binary that does not support the
/// flag costs one cheap process. Anything the binary prints on stderr is
/// dropped: a binary that does not know the flag usually complains there.
fn probe_manifest(binary: &Path) -> Option<String> {
    use std::io::Read;

    const CAP: u64 = 64 * 1024;
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    let mut child = crate::utils::spawn_retrying_busy(
        std::process::Command::new(binary)
            .arg("--print-manifest")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
    )
    .ok()?;

    // Read on a thread: waiting first would deadlock if the binary ever printed
    // more than a pipe buffer. The cap keeps a runaway binary harmless.
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        stdout.take(CAP).read_to_end(&mut out).ok().map(|_| out)
    });

    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(_) => {
                let _ = reader.join();
                return None;
            }
        }
    };
    let out = reader.join().ok()??;
    if !status.success() {
        return None;
    }

    let text = String::from_utf8(out).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    Some(text)
}

/// Run a children command, printing exactly what will run first.
fn run_visible(command: &mut Command) -> Result<()> {
    use colored::Colorize;
    let rendered = format!(
        "{} {}",
        command.get_program().to_string_lossy(),
        command
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!("  {}", rendered.dimmed());

    let status = command
        .status()
        .with_context(|| format!("cannot run '{}'", command.get_program().to_string_lossy()))?;
    if !status.success() {
        bail!("command failed with {status}");
    }
    Ok(())
}

/// Move a staged artifact into place and record its provenance.
///
/// The previous version is moved aside first, so a failure at any point leaves
/// either the old file or the new one, never a partial file.
fn commit(
    staging: &Path,
    name: &str,
    dest_dir: &Path,
    final_name: String,
    version: Option<String>,
) -> Result<Install> {
    let staged = staging.join(name);
    if !staged.exists() {
        bail!("staging produced nothing for '{name}'");
    }

    let destination = dest_dir.join(&final_name);
    let backup = staging.join(format!("{final_name}.previous"));
    if destination.exists() {
        // Move the old artifact into staging so it can be restored on failure.
        if std::fs::rename(&destination, &backup).is_err() {
            std::fs::remove_file(&destination)?;
        }
    }

    // Rename across the same filesystem is atomic; fall back to copy when the
    // staging directory is on a different mount.
    let placed = match std::fs::rename(&staged, &destination) {
        Ok(()) => Ok(()),
        Err(_) => std::fs::copy(&staged, &destination).map(|_| ()),
    };

    if let Err(e) = placed {
        if backup.exists() {
            let _ = std::fs::rename(&backup, &destination);
        }
        return Err(e).with_context(|| format!("cannot install to {}", destination.display()));
    }

    // A staged manifest travels with the artifact.
    let staged_manifest = staging.join(format!("{name}.toml"));
    if staged_manifest.exists() {
        let _ = std::fs::copy(
            &staged_manifest,
            dest_dir.join(format!("{final_name}.toml")),
        );
    } else {
        // `cargo install` and `npm pack` only carry the executable out of the
        // package, so a manifest declared by the extension itself is lost. Ask
        // the binary for one; a binary that does not answer leaves no manifest,
        // which is the same state as before this probe existed.
        adopt_manifest(&destination, &final_name, dest_dir);
    }

    let sha256 = sha256_of(&destination)?;
    let record = SourceRecord {
        source: String::new(), // filled in by the caller, which knows the raw text
        version,
        sha256,
        installed_at: chrono::Utc::now().to_rfc3339(),
    };

    Ok(Install {
        name: final_name,
        path: destination,
        record,
    })
}

/// What [`refresh_manifest_for`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refreshed {
    /// A manifest was written.
    pub wrote: bool,
    /// An existing manifest was replaced, so hand edits to it are gone.
    pub replaced: bool,
}

/// Re-probe an installed extension and rewrite its manifest beside it.
///
/// Nothing is written when the binary has nothing valid to say; the old file,
/// if any, is left untouched. This deliberately overwrites, which is why the
/// caller reports whether the file was replaced.
pub fn refresh_manifest_for(name: &str) -> Result<Refreshed> {
    let found = extensions::discover_all()
        .into_iter()
        .find(|e| e.name == name)
        .ok_or_else(|| anyhow::anyhow!("no extension named '{name}' installed"))?;
    let dir = found
        .path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", found.path.display()))?;
    let replaced = dir.join(format!("{name}.toml")).exists();

    Ok(Refreshed {
        wrote: refresh_manifest(&found.path, name, dir),
        replaced,
    })
}

/// Remove an installed extension from the directory that owns it.
pub fn remove(name: &str) -> Result<PathBuf> {
    let found = extensions::discover_all()
        .into_iter()
        .find(|e| e.name == name)
        .ok_or_else(|| anyhow::anyhow!("no extension named '{name}' is installed"))?;

    // Only managed locations may be removed; never delete from a system dir.
    let install_dir = extensions::install_dir();
    let dir = found
        .path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cannot determine install directory"))?;
    if dir != install_dir && found.origin != Some("KLAVA_EXTENSIONS_DIR") {
        bail!(
            "extension '{name}' lives in {} and was not installed by klava; remove it manually",
            found.path.display()
        );
    }

    std::fs::remove_file(&found.path)
        .with_context(|| format!("cannot remove {}", found.path.display()))?;

    for suffix in ["toml", "source.toml", "setup.toml"] {
        let path = dir.join(format!("{name}.{suffix}"));
        let _ = std::fs::remove_file(path);
    }

    Ok(found.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_local_requires_an_existing_path() {
        let missing = Source::parse("./definitely-not-here").unwrap_err();
        assert!(missing.to_string().contains("no such path"));
    }

    #[test]
    fn parse_local_accepts_an_existing_path() {
        let here = Source::parse(".").unwrap();
        assert!(matches!(here, Source::Local(_)));
    }

    #[test]
    fn parse_cargo_with_and_without_version() {
        assert_eq!(
            Source::parse("cargo:klava-compress").unwrap(),
            Source::Cargo {
                krate: "klava-compress".into(),
                version: None
            }
        );
        assert_eq!(
            Source::parse("cargo:klava-compress@0.1.0").unwrap(),
            Source::Cargo {
                krate: "klava-compress".into(),
                version: Some("0.1.0".into())
            }
        );
    }

    #[test]
    fn parse_cargo_git_with_revision() {
        assert_eq!(
            Source::parse("cargo-git:https://example.com/repo#abc123").unwrap(),
            Source::CargoGit {
                url: "https://example.com/repo".into(),
                rev: Some("abc123".into())
            }
        );
    }

    #[test]
    fn parse_npm_scoped_package_with_version() {
        assert_eq!(
            Source::parse("npm:@scope/pkg@1.2.3").unwrap(),
            Source::Npm {
                package: "@scope/pkg".into(),
                version: Some("1.2.3".into())
            }
        );
    }

    #[test]
    fn parse_npm_unscoped_without_version() {
        assert_eq!(
            Source::parse("npm:my-ext").unwrap(),
            Source::Npm {
                package: "my-ext".into(),
                version: None
            }
        );
    }

    #[test]
    fn empty_source_is_rejected() {
        assert!(Source::parse("   ").is_err());
    }

    #[test]
    fn cargo_needs_a_name() {
        assert!(Source::parse("cargo:").is_err());
        assert!(Source::parse("npm:").is_err());
        assert!(Source::parse("cargo-git:").is_err());
    }

    #[test]
    fn sha256_matches_a_known_vector() {
        let dir = std::env::temp_dir().join(format!("klava-sha-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("x");
        std::fs::write(&file, b"abc").unwrap();
        // echo -n abc | sha256sum
        assert_eq!(
            sha256_of(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn local_install_copies_and_marks_executable() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("klava-inst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src_dir = root.join("src");
        let dest_dir = root.join("dest");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dest_dir).unwrap();

        let src = src_dir.join("demo");
        std::fs::write(&src, "#!/bin/sh\necho hi\n").unwrap();
        let mut perms = std::fs::metadata(&src).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&src, perms).unwrap();

        let install = install(&Source::Local(src.clone()), &dest_dir, None).unwrap();
        assert_eq!(install.name, "demo");
        assert!(install.path.is_file());
        assert_eq!(install.path, dest_dir.join("demo"));

        let mode = std::fs::metadata(&install.path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "must stay executable");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn local_install_refuses_a_directory_with_no_executable() {
        let root = std::env::temp_dir().join(format!("klava-inst-ne-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("readme.txt"), "hi").unwrap();
        let dest = root.join("dest");
        let error = install(&Source::Local(root.clone()), &dest, None).unwrap_err();
        assert!(error.to_string().contains("no executable"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    fn fake_binary(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn a_printed_manifest_is_adopted_next_to_the_binary() {
        let root = std::env::temp_dir().join(format!("klava-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let binary = fake_binary(
            &root,
            "demo",
            r#"printf '[setup]\nargs = ["--fetch"]\nnetwork = true\n'"#,
        );

        adopt_manifest(&binary, "demo", &root);

        let text = std::fs::read_to_string(root.join("demo.toml")).expect("manifest written");
        let parsed: toml::Value = toml::from_str(&text).expect("valid toml");
        assert_eq!(parsed["setup"]["args"][0].as_str(), Some("--fetch"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_binary_that_does_not_know_the_flag_yields_no_manifest() {
        let root = std::env::temp_dir().join(format!("klava-probe-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // Exit 0 with no output: the common "unknown argument, ignored" shape.
        let binary = fake_binary(&root, "demo", "exit 0");

        adopt_manifest(&binary, "demo", &root);

        assert!(!root.join("demo.toml").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn invalid_printed_toml_is_discarded() {
        let root = std::env::temp_dir().join(format!("klava-probe-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let binary = fake_binary(&root, "demo", "printf 'not = = toml'");

        adopt_manifest(&binary, "demo", &root);

        assert!(!root.join("demo.toml").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_probe_never_writes_a_manifest() {
        let root = std::env::temp_dir().join(format!("klava-probe-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let binary = fake_binary(&root, "demo", "printf '[setup]' && exit 1");

        adopt_manifest(&binary, "demo", &root);

        assert!(!root.join("demo.toml").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_manifest_is_never_overwritten_by_the_probe() {
        let root = std::env::temp_dir().join(format!("klava-probe-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let binary = fake_binary(&root, "demo", r#"printf '[setup]\nargs = ["--new"]\n'"#);
        let manifest = root.join("demo.toml");
        std::fs::write(&manifest, "enabled = true # edited by the user\n").unwrap();

        adopt_manifest(&binary, "demo", &root);

        let text = std::fs::read_to_string(&manifest).unwrap();
        assert!(text.contains("edited by the user"), "got: {text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn refresh_manifest_overwrites_an_existing_one_only_when_asked() {
        let root = std::env::temp_dir().join(format!("klava-probe-refresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let binary = fake_binary(&root, "demo", r#"printf '[settings]\nthreshold = 0.7\n'"#);
        let manifest = root.join("demo.toml");
        std::fs::write(&manifest, "enabled = true\n").unwrap();

        // The install path still refuses to touch an existing manifest.
        adopt_manifest(&binary, "demo", &root);
        assert!(
            std::fs::read_to_string(&manifest)
                .unwrap()
                .contains("enabled")
        );

        // The explicit refresh replaces it with what the binary declares now.
        assert!(refresh_manifest(&binary, "demo", &root));
        let text = std::fs::read_to_string(&manifest).unwrap();
        assert!(text.contains("[settings]"), "got: {text}");
        assert!(text.contains("0.7"), "got: {text}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_refresh_that_gets_nothing_keeps_the_old_manifest() {
        let root =
            std::env::temp_dir().join(format!("klava-probe-refresh-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let binary = fake_binary(&root, "demo", "exit 0");
        let manifest = root.join("demo.toml");
        std::fs::write(&manifest, "enabled = true # keep me\n").unwrap();

        assert!(!refresh_manifest(&binary, "demo", &root));
        assert!(
            std::fs::read_to_string(&manifest)
                .unwrap()
                .contains("keep me")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn source_record_round_trips() {
        let dir = std::env::temp_dir().join(format!("klava-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let record = SourceRecord {
            source: "cargo:demo@1.0.0".into(),
            version: Some("1.0.0".into()),
            sha256: "abc".into(),
            installed_at: "2026-10-18T00:00:00Z".into(),
        };
        record.save(&dir, "demo").unwrap();
        let loaded = SourceRecord::load(&dir, "demo").expect("present");
        assert_eq!(loaded.source, record.source);
        assert_eq!(loaded.sha256, record.sha256);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
