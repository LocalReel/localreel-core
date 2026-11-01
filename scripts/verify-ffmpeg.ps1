param(
    [string]$Ffmpeg = 'ffmpeg',
    [string]$Ffprobe = 'ffprobe'
)

$ErrorActionPreference = 'Stop'

foreach ($pair in @(@('ffmpeg', $Ffmpeg), @('ffprobe', $Ffprobe))) {
    $label = $pair[0]
    $binary = $pair[1]
    Write-Host "== $label =="
    $output = & $binary -version 2>&1
    $exitCode = $LASTEXITCODE
    $output | Select-Object -First 1 | Write-Host
    if ($exitCode -ne 0) {
        throw "$label failed: $binary -version"
    }
}

Write-Host 'ffmpeg/ffprobe are usable.'
