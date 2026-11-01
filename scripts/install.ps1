param(
    [string]$ZipPath,
    [string]$SourceDir,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'LocalReel'),
    [switch]$DesktopShortcut,
    [switch]$StartOnLogin,
    [switch]$NoShortcuts,
    [string]$StartMenuDir = [Environment]::GetFolderPath('Programs'),
    [string]$DesktopDir = [Environment]::GetFolderPath('Desktop'),
    [string]$StartupDir = [Environment]::GetFolderPath('Startup')
)

$ErrorActionPreference = 'Stop'

function Find-PackageZip {
    param([string]$Requested)
    if ($Requested) {
        $resolved = Resolve-Path -LiteralPath $Requested
        return $resolved.Path
    }
    $root = Split-Path -Parent $PSScriptRoot
    $latest = Get-ChildItem -LiteralPath (Join-Path $root 'dist') -Filter 'localreel-*-x64.zip' -File |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1
    if (-not $latest) {
        throw "no package zip found; run scripts\package.ps1 first or pass -ZipPath"
    }
    return $latest.FullName
}

function New-Shortcut {
    param(
        [string]$Path,
        [string]$Target,
        [string]$WorkingDirectory,
        [string]$Arguments = ''
    )
    $parent = Split-Path -Parent $Path
    if ($parent) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($Path)
    $shortcut.TargetPath = $Target
    $shortcut.WorkingDirectory = $WorkingDirectory
    $shortcut.Arguments = $Arguments
    $shortcut.IconLocation = "$Target,0"
    $shortcut.Description = 'LocalReel local media server'
    $shortcut.Save()
}

$temp = $null
$zip = $null
try {
    $stage = $null
    if (-not $SourceDir -and (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'localreel.exe'))) {
        # Running from an extracted package: install in place.
        $SourceDir = $PSScriptRoot
    }
    if ($SourceDir) {
        $stage = (Resolve-Path -LiteralPath $SourceDir).Path
        if (-not (Test-Path -LiteralPath (Join-Path $stage 'localreel.exe'))) {
            throw "source directory does not contain localreel.exe: $stage"
        }
        Write-Host "Source: $stage"
    } else {
        $zip = Find-PackageZip $ZipPath
        Write-Host "Package: $zip"
        $temp = Join-Path ([System.IO.Path]::GetTempPath()) ("localreel-install-" + [guid]::NewGuid())
        New-Item -ItemType Directory -Path $temp | Out-Null
        Expand-Archive -LiteralPath $zip -DestinationPath $temp -Force
        $exe = Get-ChildItem -LiteralPath $temp -Recurse -Filter 'localreel.exe' -File |
            Select-Object -First 1
        if (-not $exe) {
            throw 'package does not contain localreel.exe'
        }
        $stage = $exe.Directory.FullName
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Get-ChildItem -LiteralPath $stage | Where-Object { $_.Name -ne 'install.ps1' } | ForEach-Object {
        Copy-Item -LiteralPath $_.FullName -Destination $InstallDir -Recurse -Force
    }

    $installedExe = Join-Path $InstallDir 'localreel.exe'
    $example = Join-Path $InstallDir 'localreel.toml.example'
    $config = Join-Path $InstallDir 'localreel.toml'
    if ((Test-Path $example) -and -not (Test-Path $config)) {
        Copy-Item -LiteralPath $example -Destination $config
    }

    $version = & $installedExe --version
    if ($LASTEXITCODE -ne 0) {
        throw 'installed binary failed --version'
    }
    Write-Host "Installed: $version -> $InstallDir"

    $shortcuts = @()
    if (-not $NoShortcuts) {
        $startMenu = Join-Path $StartMenuDir 'LocalReel.lnk'
        New-Shortcut -Path $startMenu -Target $installedExe -WorkingDirectory $InstallDir -Arguments 'serve'
        $shortcuts += $startMenu

        if ($DesktopShortcut) {
            $desktop = Join-Path $DesktopDir 'LocalReel.lnk'
            New-Shortcut -Path $desktop -Target $installedExe -WorkingDirectory $InstallDir -Arguments 'serve'
            $shortcuts += $desktop
        }

        if ($StartOnLogin) {
            $startup = Join-Path $StartupDir 'LocalReel.lnk'
            New-Shortcut -Path $startup -Target $installedExe -WorkingDirectory $InstallDir -Arguments 'serve'
            $shortcuts += $startup
        }
    }

    $manifest = [ordered]@{
        version = $version.ToString()
        installed_at = (Get-Date).ToString('o')
        install_dir = $InstallDir
        package = $(if ($zip) { $zip } else { $stage })
        shortcuts = $shortcuts
    }
    $manifest | ConvertTo-Json -Depth 4 |
        Set-Content -LiteralPath (Join-Path $InstallDir 'install.json') -Encoding ASCII

    Write-Host ''
    Write-Host "Start with: $installedExe serve"
    Write-Host "Then open: http://127.0.0.1:3001"
    if ($shortcuts.Count -gt 0) {
        Write-Host 'Shortcuts:'
        $shortcuts | ForEach-Object { Write-Host "  $_" }
    }
} finally {
    if ($temp) {
        Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
    }
}
