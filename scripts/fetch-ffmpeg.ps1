param(
    [Parameter(Mandatory = $true)]
    [string]$Url,
    [Parameter(Mandatory = $true)]
    [string]$Sha256,
    [string]$Dest = (Join-Path $PSScriptRoot '..\.tools\ffmpeg'),
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

if ((Test-Path $Dest) -and -not $Force) {
    throw "Destination '$Dest' already exists. Use -Force to replace it."
}

$temp = Join-Path ([System.IO.Path]::GetTempPath()) ("localreel-ffmpeg-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $temp | Out-Null
$archive = Join-Path $temp 'ffmpeg.zip'

try {
    Write-Host "Downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $archive -UseBasicParsing

    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
    $expected = $Sha256.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "SHA256 mismatch. expected=$expected actual=$actual"
    }

    $extract = Join-Path $temp 'extract'
    Expand-Archive -LiteralPath $archive -DestinationPath $extract -Force

    $ffmpeg = Get-ChildItem -LiteralPath $extract -Recurse -Filter 'ffmpeg.exe' -File |
        Select-Object -First 1
    $ffprobe = Get-ChildItem -LiteralPath $extract -Recurse -Filter 'ffprobe.exe' -File |
        Select-Object -First 1
    if (-not $ffmpeg -or -not $ffprobe) {
        throw 'Archive did not contain ffmpeg.exe and ffprobe.exe.'
    }

    $bin = Join-Path $Dest 'bin'
    New-Item -ItemType Directory -Force -Path $bin | Out-Null
    Copy-Item -LiteralPath $ffmpeg.FullName -Destination (Join-Path $bin 'ffmpeg.exe') -Force
    Copy-Item -LiteralPath $ffprobe.FullName -Destination (Join-Path $bin 'ffprobe.exe') -Force

    $ffmpegOutput = & (Join-Path $bin 'ffmpeg.exe') -version 2>&1
    $ffmpegExit = $LASTEXITCODE
    $ffmpegOutput | Select-Object -First 1 | Write-Host
    if ($ffmpegExit -ne 0) {
        throw 'Bundled ffmpeg.exe failed its -version check.'
    }
    $ffprobeOutput = & (Join-Path $bin 'ffprobe.exe') -version 2>&1
    $ffprobeExit = $LASTEXITCODE
    $ffprobeOutput | Select-Object -First 1 | Write-Host
    if ($ffprobeExit -ne 0) {
        throw 'Bundled ffprobe.exe failed its -version check.'
    }

    Write-Host "Installed ffmpeg/ffprobe into $bin"
    Write-Host "Set LOCALREEL_FFMPEG_DIR=$Dest or copy the bin directory next to localreel.exe."
} finally {
    Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
}
