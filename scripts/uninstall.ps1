param(
    [string]$InstallDir,
    [switch]$RemoveData,
    [switch]$StopServer,
    [switch]$Force,
    [string]$StartMenuDir = [Environment]::GetFolderPath('Programs'),
    [string]$DesktopDir = [Environment]::GetFolderPath('Desktop'),
    [string]$StartupDir = [Environment]::GetFolderPath('Startup')
)

$ErrorActionPreference = 'Stop'

if (-not $InstallDir) {
    if (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'localreel.exe')) {
        $InstallDir = $PSScriptRoot
    } else {
        $InstallDir = Join-Path $env:LOCALAPPDATA 'LocalReel'
    }
}

if (-not (Test-Path -LiteralPath $InstallDir)) {
    Write-Host "Install directory not found: $InstallDir"
    exit 0
}

$manifestPath = Join-Path $InstallDir 'install.json'
$manifest = $null
if (Test-Path -LiteralPath $manifestPath) {
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
}

$exe = Join-Path $InstallDir 'localreel.exe'
$running = @(Get-Process localreel -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and ($_.Path -ieq $exe) })
if ($running.Count -gt 0) {
    if (-not $StopServer) {
        throw "LocalReel is running from $exe; stop it or pass -StopServer"
    }
    $running | ForEach-Object { Stop-Process -Id $_.Id -Force }
    Start-Sleep -Milliseconds 500
}

if (-not $Force) {
    $detail = if ($RemoveData) { 'This will delete data and progress.' } else { 'Data and localreel.toml will be kept.' }
    $answer = Read-Host "Uninstall LocalReel from $InstallDir? $detail [y/N]"
    if ($answer -notmatch '^[yY]') {
        Write-Host 'Canceled.'
        exit 0
    }
}

$shortcuts = @()
if ($manifest -and $manifest.shortcuts) {
    $shortcuts = @($manifest.shortcuts)
}
$defaults = @(
    (Join-Path $StartMenuDir 'LocalReel.lnk'),
    (Join-Path $DesktopDir 'LocalReel.lnk'),
    (Join-Path $StartupDir 'LocalReel.lnk')
)
foreach ($path in ($shortcuts + $defaults)) {
    if ($path -and (Test-Path -LiteralPath $path)) {
        Remove-Item -LiteralPath $path -Force
        Write-Host "Removed shortcut: $path"
    }
}

if ($RemoveData) {
    Remove-Item -LiteralPath $InstallDir -Recurse -Force
    Write-Host "Removed $InstallDir (including data)."
} else {
    Get-ChildItem -LiteralPath $InstallDir | Where-Object {
        $_.Name -notin @('data', 'localreel.toml')
    } | ForEach-Object {
        Remove-Item -LiteralPath $_.FullName -Recurse -Force
    }
    Write-Host "Removed program files from $InstallDir."
    if (Test-Path -LiteralPath (Join-Path $InstallDir 'data')) {
        Write-Host "Kept data: $(Join-Path $InstallDir 'data')"
    }
    if (Test-Path -LiteralPath (Join-Path $InstallDir 'localreel.toml')) {
        Write-Host "Kept config: $(Join-Path $InstallDir 'localreel.toml')"
    }
}
