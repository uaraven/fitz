# Build a native FitSmith .msi installer on Windows.
# Requires the WiX Toolset (https://wixtoolset.org/) on PATH; cargo-bundle shells
# out to it to produce the .msi.
$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Resolve-Path (Join-Path $scriptDir "..\..")

if (-not (Get-Command "cargo-bundle" -ErrorAction SilentlyContinue)) {
    Write-Host "cargo-bundle not found; installing it (cargo install cargo-bundle)..."
    cargo install cargo-bundle
}

if (-not ((Get-Command "candle" -ErrorAction SilentlyContinue) -or (Get-Command "wix" -ErrorAction SilentlyContinue))) {
    Write-Warning "WiX Toolset not found on PATH. Install it from https://wixtoolset.org/ before running this script."
    exit 1
}

# Run from fitsmith/ itself: cargo-bundle resolves the icon/resources globs in
# [package.metadata.bundle] relative to the current directory, not the manifest's.
Push-Location (Join-Path $repoRoot "fitsmith")
try {
    cargo bundle -p fitsmith --release -f msi
} finally {
    Pop-Location
}

$msi = Get-ChildItem (Join-Path $repoRoot "target\release\bundle\msi\*.msi") | Select-Object -First 1
Write-Host "Packaged: $($msi.FullName)"
