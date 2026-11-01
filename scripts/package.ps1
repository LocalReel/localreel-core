param(
    [ValidateSet('debug', 'release')]
    [string]$Profile = 'release',
    [string]$OutputDir = 'dist',
    [string]$FfmpegDir,
    [switch]$SkipBuild,
    [switch]$BuildWeb
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$webDir = Join-Path (Split-Path -Parent $root) 'localreel-web'

$cargo = if ($env:CARGO) {
    $env:CARGO
} elseif (Get-Command cargo -ErrorAction SilentlyContinue) {
    (Get-Command cargo).Source
} else {
    Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
}

$npm = if (Get-Command npm -ErrorAction SilentlyContinue) {
    (Get-Command npm).Source
} else {
    'C:\Program Files\nodejs\npm.cmd'
}

if ($BuildWeb) {
    Write-Host 'Building web assets...'
    $nodeDir = Split-Path -Parent $npm
    if ($nodeDir) {
        $env:Path = "$nodeDir;$env:Path"
    }
    Push-Location $webDir
    try {
        & $npm run build
        if ($LASTEXITCODE -ne 0) {
            throw 'web build failed'
        }
    } finally {
        Pop-Location
    }
}

$webIndex = Join-Path $webDir 'dist\index.html'
if (-not (Test-Path $webIndex)) {
    throw "web dist is missing at $webIndex; run 'npm run build' in localreel-web or pass -BuildWeb"
}

$metadata = & $cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
$package = $metadata.packages | Where-Object { $_.name -eq 'localreel-server' } | Select-Object -First 1
if (-not $package) {
    throw 'could not determine localreel-server version from cargo metadata'
}
$version = $package.version
$arch = if ($env:PROCESSOR_ARCHITECTURE -match 'ARM64') { 'arm64' } else { 'x64' }
$stageName = "localreel-$version-$arch"
$stage = Join-Path (Join-Path $root $OutputDir) $stageName

if (-not $SkipBuild) {
    Write-Host "Building localreel-server ($Profile)..."
    & $cargo build -p localreel-server --profile $Profile
    if ($LASTEXITCODE -ne 0) {
        throw 'cargo build failed'
    }
}

$exeName = if ($env:OS -eq 'Windows_NT') { 'localreel.exe' } else { 'localreel' }
$binary = Join-Path (Join-Path $root "target\$Profile") $exeName
if (-not (Test-Path $binary)) {
    throw "binary not found: $binary (run without -SkipBuild)"
}

if (Test-Path $stage) {
    Remove-Item -LiteralPath $stage -Recurse -Force
}
New-Item -ItemType Directory -Path $stage | Out-Null
Copy-Item -LiteralPath $binary -Destination (Join-Path $stage $exeName)
Copy-Item -LiteralPath (Join-Path $root 'localreel.toml.example') -Destination (Join-Path $stage 'localreel.toml.example')
Copy-Item -LiteralPath (Join-Path $root 'README.md') -Destination (Join-Path $stage 'README.md')
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'docs') | Out-Null
Copy-Item -LiteralPath (Join-Path $root 'docs\packaging.md') -Destination (Join-Path $stage 'docs\packaging.md')
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'tools') | Out-Null
Copy-Item -LiteralPath (Join-Path $root 'scripts\fetch-ffmpeg.ps1') -Destination (Join-Path $stage 'tools\fetch-ffmpeg.ps1')
Copy-Item -LiteralPath (Join-Path $root 'scripts\verify-ffmpeg.ps1') -Destination (Join-Path $stage 'tools\verify-ffmpeg.ps1')
Copy-Item -LiteralPath (Join-Path $root 'scripts\install.ps1') -Destination (Join-Path $stage 'install.ps1')
Copy-Item -LiteralPath (Join-Path $root 'scripts\uninstall.ps1') -Destination (Join-Path $stage 'uninstall.ps1')

if ($FfmpegDir) {
    $source = if (Test-Path (Join-Path $FfmpegDir 'ffmpeg.exe')) {
        $FfmpegDir
    } elseif (Test-Path (Join-Path $FfmpegDir 'bin\ffmpeg.exe')) {
        Join-Path $FfmpegDir 'bin'
    } else {
        throw "ffmpeg.exe/ffprobe.exe not found in $FfmpegDir"
    }
    $target = Join-Path $stage 'ffmpeg\bin'
    New-Item -ItemType Directory -Force -Path $target | Out-Null
    Copy-Item -LiteralPath (Join-Path $source 'ffmpeg.exe') -Destination $target
    Copy-Item -LiteralPath (Join-Path $source 'ffprobe.exe') -Destination $target
}

$quickStart = @"
LocalReel $version ($arch)

Quick start (Windows):
  1. Optional one-click install: .\install.ps1
     (copies to %LOCALAPPDATA%\LocalReel and creates a Start Menu shortcut)
     Uninstall later with .\uninstall.ps1 (keeps data unless -RemoveData).
  2. Or run in place: copy localreel.toml.example to localreel.toml,
     edit [data]/[media], then .\localreel.exe doctor && .\localreel.exe serve.
  3. Optional: put ffmpeg.exe/ffprobe.exe in ffmpeg\bin, or run tools\fetch-ffmpeg.ps1.
  4. Open http://127.0.0.1:3001

The web UI is embedded in localreel.exe; no separate dist directory is needed.
"@
Set-Content -LiteralPath (Join-Path $stage 'README.txt') -Value $quickStart -Encoding ASCII

$versionOutput = & (Join-Path $stage $exeName) --version
if ($LASTEXITCODE -ne 0) {
    throw 'packaged binary failed --version'
}
Write-Host "Packaged binary: $versionOutput"

$zip = Join-Path (Join-Path $root $OutputDir) "$stageName.zip"
if (Test-Path $zip) {
    Remove-Item -LiteralPath $zip -Force
}
Compress-Archive -LiteralPath $stage -DestinationPath $zip -CompressionLevel Optimal
$zipInfo = Get-Item $zip
$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash.ToLowerInvariant()
$checksum = "$hash  $($zipInfo.Name)"
Set-Content -LiteralPath "$zip.sha256" -Value $checksum -Encoding ASCII
Write-Host "Package: $($zipInfo.FullName) ($([math]::Round($zipInfo.Length / 1MB, 1)) MiB)"
Write-Host "SHA256: $hash"
