param(
    [switch]$SkipRust,
    [switch]$SkipWeb
)

$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
$webDir = Join-Path (Split-Path -Parent $root) 'localreel-web'

$cargo = if ($env:CARGO) {
    $env:CARGO
} elseif (Get-Command cargo -ErrorAction SilentlyContinue) {
    (Get-Command cargo).Source
} else {
    Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
}

$npm = if ($env:NPM) {
    $env:NPM
} else {
    # Prefer a real executable shim (npm.cmd on Windows); npm.ps1 breaks when
    # invoked by full path because it builds its command via Invoke-Expression.
    $shim = Get-Command npm -All -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandType -ne 'ExternalScript' } |
        Select-Object -First 1
    if ($shim) {
        $shim.Source
    } elseif (Test-Path 'C:\Program Files\nodejs\npm.cmd') {
        'C:\Program Files\nodejs\npm.cmd'
    } else {
        'npm'
    }
}
$nodeDir = Split-Path -Parent $npm
if ($nodeDir) {
    $env:Path = "$nodeDir;$env:Path"
}

function Invoke-Check([string]$Name, [scriptblock]$Command) {
    Write-Host "== $Name =="
    & $Command
    $code = $LASTEXITCODE
    if ($code -ne 0) {
        throw "$Name failed (exit $code)"
    }
}

if (-not $SkipRust) {
    Push-Location $root
    try {
        Invoke-Check 'cargo fmt' { & $cargo fmt --all -- --check }
        Invoke-Check 'cargo clippy' {
            & $cargo clippy --workspace --all-targets -- -D warnings
        }
        Invoke-Check 'cargo test' { & $cargo test --workspace }
    } finally {
        Pop-Location
    }
}

if (-not $SkipWeb) {
    Push-Location $webDir
    try {
        Invoke-Check 'web unit tests' { & $npm test }
        Invoke-Check 'web build' { & $npm run build }
    } finally {
        Pop-Location
    }
}

Write-Host 'release checks passed.'
