param(
    [ValidateSet('x86_64-pc-windows-msvc')]
    [string]$Target = 'x86_64-pc-windows-msvc'
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$outputDir = Join-Path $repoRoot "dist\remote-runners\$Target"
$previousRustflags = $env:RUSTFLAGS
try {
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --release --package ralphus-runner --target $Target --manifest-path (Join-Path $repoRoot 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed for $Target" }
} finally {
    $env:RUSTFLAGS = $previousRustflags
}

New-Item -ItemType Directory -Force -Path $outputDir | Out-Null
$source = Join-Path $repoRoot "target\$Target\release\ralphus-runner.exe"
$artifact = Join-Path $outputDir 'ralphus-runner.exe'
Copy-Item -LiteralPath $source -Destination $artifact -Force
$checksum = (Get-FileHash -Algorithm SHA256 -LiteralPath $artifact).Hash.ToLowerInvariant()
Set-Content -LiteralPath "$artifact.sha256" -Value "$checksum  ralphus-runner.exe" -Encoding ascii
Write-Host "Built $artifact"
Write-Host "SHA-256 $checksum"
