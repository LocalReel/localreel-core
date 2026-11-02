# Regenerate dist/SHA256SUMS.txt from the release artifacts in dist/.
#
# The manifest stays a build output (dist/ is git-ignored); the hashes that must
# survive in version control are recorded in docs/release/v1.0-readiness.md.

param(
    [string]$Dist = 'dist'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$dir = if ([System.IO.Path]::IsPathRooted($Dist)) { $Dist } else { Join-Path $root $Dist }

if (-not (Test-Path -LiteralPath $dir)) {
    throw "no artifact directory: $dir (run scripts\package.ps1 first)"
}

$artifacts = Get-ChildItem -LiteralPath $dir -File |
    Where-Object { $_.Extension -in '.zip', '.msi' } |
    Sort-Object Name
if (-not $artifacts) {
    throw "no .zip/.msi artifacts found in $dir"
}

$lines = foreach ($artifact in $artifacts) {
    $hash = (Get-FileHash -LiteralPath $artifact.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $($artifact.Name)"
}

$out = Join-Path $dir 'SHA256SUMS.txt'
Set-Content -LiteralPath $out -Value $lines -Encoding ASCII
Write-Host "Wrote $out"
$lines | ForEach-Object { Write-Host $_ }
