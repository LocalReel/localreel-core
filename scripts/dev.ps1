param(
    [int]$Port = 3101,
    [string]$DataDir,
    [string]$Profile = 'debug',
    [switch]$SkipBuild,
    [string]$FfmpegDir
)

# Development server for LocalReel.
#
# This is deliberately separate from an installed release:
#   * release install (MSI/zip) -> %LOCALAPPDATA%\LocalReel, port 3001
#   * dev server (this script)  -> <repo>\.dev-data,        port 3101
# so experiments, test scans and cache churn never touch a user's real library
# database. Nothing here reads or writes %LOCALAPPDATA%\LocalReel.

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (-not $DataDir) {
    $DataDir = Join-Path $root '.dev-data'
}

$cargo = if ($env:CARGO) {
    $env:CARGO
} elseif (Get-Command cargo -ErrorAction SilentlyContinue) {
    (Get-Command cargo).Source
} else {
    Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
}

New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
$env:LOCALREEL_DATA_DIR = $DataDir
$env:LOCALREEL_PORT = "$Port"
if ($FfmpegDir) {
    $env:LOCALREEL_FFMPEG_DIR = $FfmpegDir
}

Push-Location -LiteralPath $root
try {
    if (-not $SkipBuild) {
        Write-Host "Building localreel-server ($Profile)..."
        # `debug` is not a cargo profile name; the default `dev` profile already
        # writes to target\debug, so only release needs an explicit flag.
        $buildArgs = @('build', '-p', 'localreel-server')
        if ($Profile -eq 'release') {
            $buildArgs += @('--profile', 'release')
        }
        & $cargo @buildArgs
        if ($LASTEXITCODE -ne 0) {
            throw 'cargo build failed'
        }
    }

    $exe = if ($env:OS -eq 'Windows_NT') { 'localreel.exe' } else { 'localreel' }
    $binary = Join-Path $root "target\$Profile\$exe"
    if (-not (Test-Path -LiteralPath $binary)) {
        throw "binary not found: $binary (run without -SkipBuild)"
    }

    Write-Host "dev server: http://127.0.0.1:$Port"
    Write-Host "dev data:   $DataDir"
    Write-Host "release installs use port 3001 and %LOCALAPPDATA%\LocalReel; this process never touches them."
    & $binary serve
} finally {
    Pop-Location
}
