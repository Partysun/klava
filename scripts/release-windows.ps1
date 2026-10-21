# Windows release build + publish (native powershell on the local Windows
# agent; step environments DO reach powershell steps, but not the bash (WSL)
# steps, so everything here is powershell + node — no `bash scripts/...`).
#
#   powershell -File scripts\release-windows.ps1 build
#   powershell -File scripts\release-windows.ps1 pypi
#   powershell -File scripts\release-windows.ps1 npm
#
# Called by the windows steps of .woodpecker/release-windows.yml. All logic
# lives in this script file rather than in the YAML: Woodpecker rewrites
# inline step commands with its envsubst (${...} gets interpolated and can
# even make the config fail to parse — e.g. `$env:` in a one-liner), while
# script files reach the shell untouched.
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("build", "pypi", "npm", "extensions-build", "extensions")]
    [string]$Phase
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

# [package] version of the CLI (not [workspace.package], which is the
# extension crates' 0.1.x and comes first in Cargo.toml)
function Get-KlavaVersion {
    $inPackage = $false
    foreach ($line in (Get-Content Cargo.toml)) {
        if ($line -match '^\[package\]') { $inPackage = $true; continue }
        if ($line -match '^\[') { $inPackage = $false; continue }
        if ($inPackage -and $line -match '^\s*version\s*=\s*"([^"]+)"') { return $matches[1] }
    }
    throw "cannot read [package] version from Cargo.toml"
}

switch ($Phase) {
    "build" {
        # guard against an agent rustup with no default toolchain configured
        rustup default stable
        if (-not $?) { throw "rustup default stable failed" }

        # npm packages are pinned from the tag while the wheel version comes
        # from Cargo.toml: a tag that disagrees would publish two versions
        $cargoVersion = Get-KlavaVersion
        if ($env:CI_COMMIT_TAG) {
            $tagVersion = $env:CI_COMMIT_TAG -replace '^v', ''
            if ($tagVersion -ne $cargoVersion) {
                throw "tag $($env:CI_COMMIT_TAG) does not match Cargo.toml version $cargoVersion"
            }
        }

        if (-not (Get-Command uv -ErrorAction SilentlyContinue)) {
            cargo install --quiet uv
            if (-not $?) { throw "cargo install uv failed" }
            $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
        }
        uv tool install --quiet maturin
        if (-not $?) { throw "uv tool install maturin failed" }

        if (Test-Path dist) { Remove-Item dist -Recurse -Force }
        uv tool run maturin build --release --locked --out dist
        if (-not $?) { throw "maturin build failed" }

        # the npm binary is the same build the wheel carries
        $wheel = Get-ChildItem dist\*.whl | Select-Object -First 1
        if (-not $wheel) { throw "maturin produced no wheel in dist\" }
        $tmp = Join-Path $env:TEMP ("klava-wheel-" + [guid]::NewGuid())
        New-Item -ItemType Directory -Path $tmp -Force | Out-Null
        # Expand-Archive only takes .zip, and a wheel is a zip
        $zip = Join-Path $tmp "wheel.zip"
        Copy-Item $wheel.FullName $zip
        $extracted = Join-Path $tmp "x"
        Expand-Archive -Path $zip -DestinationPath $extracted
        $bin = Get-ChildItem -Path $extracted -Recurse -Filter "klava.exe" |
            Where-Object { $_.DirectoryName -like "*scripts" } |
            Select-Object -First 1
        if (-not $bin) { throw "no klava.exe console script inside $($wheel.Name)" }
        Copy-Item $bin.FullName npm\platforms\win32-x64-msvc\klava.exe -Force
        Remove-Item $tmp -Recurse -Force
        & ".\npm\platforms\win32-x64-msvc\klava.exe" --version
        if (-not $?) { throw "klava.exe smoke failed" }
        Get-ChildItem dist
    }

    "pypi" {
        # uv is usually already there from the build phase; only install it
        # when missing. Re-running `cargo install uv` here compiles uv from
        # source again (several minutes, and it looks hung in the log).
        if (-not (Get-Command uv -ErrorAction SilentlyContinue)) {
            cargo install --quiet uv
            if (-not $?) { throw "cargo install uv failed" }
            $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
        }

        # PyPI rejects re-uploads ("File already exists"): skip files already
        # on the registry so re-runs of the same version are a no-op (mirrors
        # scripts/pypi-publish.sh).
        $version = Get-KlavaVersion
        if (-not (Test-Path dist)) { throw "no dist\ directory" }
        $artifacts = @(Get-ChildItem -Path dist -File)
        if ($artifacts.Count -eq 0) { throw "no artifacts in dist\" }
        $missing = @()
        foreach ($a in $artifacts) {
            $found = $false
            try {
                $json = Invoke-RestMethod -Uri "https://pypi.org/pypi/klava/$version/json" -TimeoutSec 20
                foreach ($u in $json.urls) {
                    if ($u.filename -eq $a.Name) { $found = $true; break }
                }
            } catch {
                # a 404 means "version not on PyPI yet" (publish everything);
                # anything else (network, auth) must not silently turn into
                # "publish everything" and then fail with "already exists"
                $code = $null
                if ($_.Exception.Response) { $code = [int]$_.Exception.Response.StatusCode }
                if ($code -ne 404) {
                    throw "cannot query PyPI for klava $version (HTTP $code): $($_.Exception.Message)"
                }
            }
            if ($found) { echo "skip: $($a.Name) already on PyPI" } else { $missing += $a.Path }
        }
        if ($missing.Count -eq 0) {
            echo "nothing new to publish"
            exit 0
        }
        echo "publishing $($missing.Count) file(s)"
        uv publish @missing
        exit $LASTEXITCODE
    }

    "npm" {
        if (-not $env:NPM_TOKEN) { throw "NPM_TOKEN is required" }
        npm config set "//registry.npmjs.org/:_authToken" $env:NPM_TOKEN
        if (-not $?) { throw "npm config set failed" }

        node scripts\npm-version.js npm\platforms\win32-x64-msvc
        if (-not $?) { throw "npm-version.js failed" }
        node scripts\npm-publish.js npm\platforms\win32-x64-msvc
        exit $LASTEXITCODE
    }

    "extensions-build" {
        # `ort-sys` probes pkg-config for libonnxruntime *before* falling back
        # to its static download and accepts any version >= 1.22, so an agent
        # with ONNX Runtime installed would link it dynamically — which a
        # single-file npm package cannot carry. Shadow it with a stale .pc so
        # ort-sys ignores it and statically links its own prebuilt binaries.
        $pcDir = "target\ext-build\win32-x64-msvc\pkgconfig"
        New-Item -ItemType Directory -Path $pcDir -Force | Out-Null
        Set-Content -Path (Join-Path $pcDir "libonnxruntime.pc") `
            -Value "Name: onnxruntime`nDescription: shadowed, see scripts/release-windows.ps1`nVersion: 1.0.0`nLibs:`nCflags:`n"
        $env:PKG_CONFIG_PATH = $pcDir + $(if ($env:PKG_CONFIG_PATH) { ";$env:PKG_CONFIG_PATH" } else { "" })
        Remove-Item Env:ORT_LIB_LOCATION -ErrorAction SilentlyContinue

        # The three extension crates are independent binaries; `cargo build`
        # with several -p flags shares the dependency build.
        $packages = @("klava-compress", "klava-pii-guardrail", "klava-model-escalation")
        $cargoArgs = @("build", "--release", "--locked")
        foreach ($p in $packages) { $cargoArgs += @("-p", $p) }
        & cargo @cargoArgs
        if (-not $?) { throw "cargo build of the extensions failed" }

        # scripts/ext-npm.js stages the npm packages from a directory of
        # unconditionally named binaries; the msvc artifacts carry .exe.
        $dest = "target\ext-npm\win32-x64-msvc"
        if (Test-Path $dest) { Remove-Item $dest -Recurse -Force }
        New-Item -ItemType Directory -Path $dest -Force | Out-Null
        foreach ($p in $packages) {
            $src = "target\release\$p.exe"
            if (-not (Test-Path $src)) { throw "missing $src" }
            # the same dependency guard the unix phases run with ldd/otool
            $text = [System.Text.Encoding]::ASCII.GetString([System.IO.File]::ReadAllBytes($src))
            if ($text -match 'onnxruntime\.dll') {
                throw "$p.exe imports a shared ONNX runtime: a single-file npm package cannot work"
            }
            Copy-Item $src (Join-Path $dest "$p.exe") -Force
            # same smoke test klava runs after installing one
            & (Join-Path $dest "$p.exe") --print-manifest | Out-Null
            if (-not $?) { throw "$p.exe --print-manifest failed" }
        }
        Get-ChildItem $dest
    }

    "extensions" {
        # The binaries come from the extensions-build step; this image has node
        # but the publish step cannot recompile them (and must not).
        if (-not $env:NPM_TOKEN) { throw "NPM_TOKEN is required" }
        npm config set "//registry.npmjs.org/:_authToken" $env:NPM_TOKEN
        if (-not $?) { throw "npm config set failed" }

        $dest = "target\ext-npm\win32-x64-msvc"
        if (-not (Test-Path $dest)) {
            throw "no $dest : run the extensions-build phase first"
        }
        node scripts\ext-npm.js prepare win32-x64-msvc $dest
        if (-not $?) { throw "ext-npm.js prepare failed" }
        $dirs = node scripts\ext-npm.js dirs win32-x64-msvc
        if (-not $?) { throw "ext-npm.js dirs failed" }
        foreach ($dir in $dirs) {
            node scripts\npm-publish.js $dir
            if ($LASTEXITCODE -ne 0) { throw "npm publish of $dir failed" }
        }
        exit 0
    }
}
