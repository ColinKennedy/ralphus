# Builds the vendored psmux git submodule (vendor/psmux, RAL-347) from source
# and copies the resulting tmux.exe into daemon/assets/tmux/windows/, where
# daemon/src/tmux.rs's `embedded` module `include_bytes!`s it when the
# ralphus-daemon `embedded-tmux` Cargo feature is enabled.
#
# This is a deliberate, explicit step -- not something `cargo build` triggers
# on its own -- so a plain `cargo build`/`cargo test` never needs the
# submodule checked out or psmux's own dependency tree fetched. Run this
# first, then `cargo build --release --features ralphus-daemon/embedded-tmux`
# (scripts/build-release.cmd does both by default; opt out with
# RALPHUS_SKIP_VENDORED_TMUX=1).
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$vendorDir = Join-Path $repoRoot 'vendor\psmux'
$vendorManifest = Join-Path $vendorDir 'Cargo.toml'
$assetDir = Join-Path $repoRoot 'daemon\assets\tmux\windows'

if (-not (Test-Path -LiteralPath $vendorManifest)) {
    Write-Host "== vendor/psmux submodule not initialized; running git submodule update =="
    & git -C $repoRoot submodule update --init -- vendor/psmux
    if ($LASTEXITCODE -ne 0) {
        throw "git submodule update --init -- vendor/psmux failed with exit code $LASTEXITCODE"
    }
}

if (-not (Test-Path -LiteralPath $vendorManifest)) {
    throw "vendor/psmux still has no Cargo.toml after submodule update -- check .gitmodules and network access"
}

Write-Host "== building vendored psmux (release, bin: tmux) =="
& cargo build --release --manifest-path $vendorManifest --bin tmux
if ($LASTEXITCODE -ne 0) {
    throw "cargo build --manifest-path $vendorManifest failed with exit code $LASTEXITCODE"
}

$builtExe = Join-Path $vendorDir 'target\release\tmux.exe'
if (-not (Test-Path -LiteralPath $builtExe)) {
    throw "expected build output missing: $builtExe"
}

New-Item -ItemType Directory -Force -Path $assetDir | Out-Null
$destExe = Join-Path $assetDir 'tmux.exe'
Copy-Item -LiteralPath $builtExe -Destination $destExe -Force

$version = & $destExe -V
Write-Host "== wrote $destExe ($version) =="
