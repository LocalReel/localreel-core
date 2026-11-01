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

$npm = if (Get-Command npm -ErrorAction SilentlyContinue) {
    (Get-Command npm).Source
} else {
    'C:\Program Files\nodejs\npm.cmd'
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
