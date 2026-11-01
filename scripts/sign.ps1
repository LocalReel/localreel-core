param(
    [Parameter(Mandatory = $true)]
    [string[]]$Path,
    [string]$PfxPath,
    [string]$PfxPassword,
    [switch]$SelfSignedTest,
    [switch]$NoTimestamp,
    [string]$TimestampUrl = 'http://timestamp.digicert.com'
)

$ErrorActionPreference = 'Stop'

$signtool = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\bin' -Recurse -Filter 'signtool.exe' -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match '\\x64\\signtool\.exe$' } |
    Sort-Object FullName -Descending |
    Select-Object -First 1 -ExpandProperty FullName
if (-not $signtool) {
    throw 'signtool.exe not found (install the Windows SDK)'
}

$files = foreach ($item in $Path) {
    $resolved = Resolve-Path -LiteralPath $item
    $resolved.Path
}
foreach ($file in $files) {
    if (-not (Test-Path -LiteralPath $file)) {
        throw "file not found: $file"
    }
}

$temporaryPfx = $null
$temporaryCert = $null
$pfxPasswordPlain = $PfxPassword

if ($SelfSignedTest) {
    if ($PfxPath) {
        throw 'use either -SelfSignedTest or -PfxPath, not both'
    }
    $temporaryPfx = Join-Path ([System.IO.Path]::GetTempPath()) ("localreel-sign-" + [guid]::NewGuid() + '.pfx')
    $pfxPasswordPlain = "test-" + [guid]::NewGuid().ToString('N')
    $password = ConvertTo-SecureString -String $pfxPasswordPlain -Force -AsPlainText
    $temporaryCert = New-SelfSignedCertificate `
        -Type CodeSigningCert `
        -Subject 'CN=LocalReel Test Signing' `
        -CertStoreLocation 'Cert:\CurrentUser\My' `
        -KeyExportPolicy Exportable `
        -HashAlgorithm SHA256
    Export-PfxCertificate -Cert $temporaryCert -FilePath $temporaryPfx -Password $password | Out-Null
    $PfxPath = $temporaryPfx
    Write-Host "Self-signed test certificate: $($temporaryCert.Thumbprint)"
}

if (-not $PfxPath) {
    throw 'pass -PfxPath/-PfxPassword for a real certificate, or -SelfSignedTest for a local test'
}
if (-not $pfxPasswordPlain) {
    throw 'PfxPassword is required with PfxPath'
}

try {
    foreach ($file in $files) {
        $arguments = @('sign', '/fd', 'SHA256', '/f', $PfxPath, '/p', $pfxPasswordPlain)
        if (-not $NoTimestamp -and -not $SelfSignedTest) {
            $arguments += @('/tr', $TimestampUrl, '/td', 'SHA256')
        }
        $arguments += $file
        Write-Host "Signing: $file"
        & $signtool @arguments
        if ($LASTEXITCODE -ne 0) {
            throw "signtool failed for $file (exit $LASTEXITCODE)"
        }
        $signature = Get-AuthenticodeSignature -LiteralPath $file
        Write-Host "  status: $($signature.Status)"
        Write-Host "  signer: $($signature.SignerCertificate.Subject)"
        if (-not $signature.SignerCertificate) {
            throw "no signer certificate embedded in $file"
        }
    }
} finally {
    if ($temporaryPfx) {
        Remove-Item -LiteralPath $temporaryPfx -Force -ErrorAction SilentlyContinue
    }
    if ($temporaryCert) {
        Remove-Item -LiteralPath ("Cert:\CurrentUser\My\" + $temporaryCert.Thumbprint) -Force -ErrorAction SilentlyContinue
    }
}
