$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Resolve-Path (Join-Path $scriptDir "..")
$fitsmithManifest = Join-Path $repoRoot "fitsmith\Cargo.toml"
$releaseDir = Join-Path $repoRoot "target\release"
$makensis = Get-Command "makensis" -ErrorAction SilentlyContinue

if (-not $makensis) {
    throw "NSIS is required to build the Windows installer. Install NSIS and make makensis available on PATH: https://nsis.sourceforge.io/"
}

Push-Location $repoRoot
try {
    cargo build --locked --release -p fitz -p fitsmith
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }

    foreach ($binary in @("fitz.exe", "fitsmith.exe")) {
        if (-not (Test-Path (Join-Path $releaseDir $binary))) {
            throw "Expected release binary was not produced: $binary"
        }
    }

    $bayerDll = Get-ChildItem $releaseDir -Filter "bayer.dll" -File |
        Select-Object -First 1
    if (-not $bayerDll) {
        throw "Expected Bayer runtime DLL was not produced under target\release."
    }

    $versionLine = Get-Content $fitsmithManifest |
        Select-String -Pattern '^\s*version\s*=\s*"([^"]+)"' |
        Select-Object -First 1
    if (-not $versionLine -or $versionLine.Line -notmatch '^\s*version\s*=\s*"([^"]+)"') {
        throw "Could not read the FitSmith version from fitsmith\Cargo.toml."
    }
    $version = $Matches[1]

    $bundleDir = Join-Path $releaseDir "bundle"
    New-Item -ItemType Directory -Force -Path $bundleDir | Out-Null

    & $makensis.Source "/DAPP_VERSION=$version" "/DBAYER_DLL=$($bayerDll.FullName)" (Join-Path $repoRoot "build\package-windows.nsi")
    if ($LASTEXITCODE -ne 0) {
        throw "makensis failed with exit code $LASTEXITCODE"
    }

    $installer = Join-Path $releaseDir "bundle\FitSmith-$version-windows-x64-setup.exe"
    if (-not (Test-Path $installer)) {
        throw "NSIS completed without producing the expected installer: $installer"
    }
    Write-Host "Packaged: $installer"
} finally {
    Pop-Location
}
