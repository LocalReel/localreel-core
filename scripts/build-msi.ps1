param(
    [string]$Stage,
    [string]$Output,
    [switch]$SkipValidation
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

if (-not $Stage) {
    $candidate = Get-ChildItem -LiteralPath (Join-Path $root 'dist') -Directory -Filter 'localreel-*-x64' |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1
    if (-not $candidate) {
        throw "no package stage found; run scripts\package.ps1 first or pass -Stage"
    }
    $Stage = $candidate.FullName
}
$exe = Join-Path $Stage 'localreel.exe'
if (-not (Test-Path -LiteralPath $exe)) {
    throw "missing $exe; build the portable package first"
}

$version = if ((Split-Path -Leaf $Stage) -match '^localreel-(.+)-x64$') {
    $Matches[1]
} else {
    '0.1.0'
}

$wix = if (Get-Command wix -ErrorAction SilentlyContinue) {
    (Get-Command wix).Source
} elseif (Test-Path (Join-Path $env:USERPROFILE '.dotnet\tools\wix.exe')) {
    Join-Path $env:USERPROFILE '.dotnet\tools\wix.exe'
} else {
    throw 'wix.exe not found; install with: dotnet tool install --global wix --version 5.0.2'
}

if (-not $Output) {
    $Output = Join-Path $root "dist\localreel-$version-x64.msi"
}
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Output) | Out-Null
if (Test-Path -LiteralPath $Output) {
    Remove-Item -LiteralPath $Output -Force
}

& $wix build (Join-Path $root 'installer\localreel.wxs') `
    -arch x64 `
    -d "SourceDir=$Stage" `
    -d "Version=$version" `
    -o $Output
if ($LASTEXITCODE -ne 0) {
    throw "wix build failed with exit $LASTEXITCODE"
}
$info = Get-Item -LiteralPath $Output
Write-Host "MSI: $($info.FullName) ($([math]::Round($info.Length / 1MB, 2)) MiB)"

if (-not $SkipValidation) {
    $extract = Join-Path ([System.IO.Path]::GetTempPath()) ("localreel-msi-" + [guid]::NewGuid())
    New-Item -ItemType Directory -Path $extract | Out-Null
    try {
        $arguments = @('/a', "`"$Output`"", '/qn', "TARGETDIR=`"$extract`"")
        $process = Start-Process -FilePath 'msiexec.exe' -ArgumentList $arguments -Wait -PassThru
        if ($process.ExitCode -ne 0) {
            throw "msiexec administrative extract failed with $($process.ExitCode)"
        }
        $extracted = Get-ChildItem -LiteralPath $extract -Recurse -Filter 'localreel.exe' -File |
            Select-Object -First 1
        if (-not $extracted) {
            throw 'administrative extract did not produce localreel.exe'
        }
        Write-Host "Extract check: $($extracted.FullName)"
    } finally {
        Remove-Item -LiteralPath $extract -Recurse -Force -ErrorAction SilentlyContinue
    }
}
