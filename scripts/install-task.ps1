param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'LocalReel'),
    [string]$TaskName = 'LocalReel Server',
    [switch]$AtStartup,
    [switch]$StartNow,
    [switch]$Uninstall,
    [switch]$StopServer
)

$ErrorActionPreference = 'Stop'

function Get-LocalReelProcesses {
    param([string]$Exe)
    @(Get-Process localreel -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and ($_.Path -ieq $Exe) })
}

$existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue

if ($Uninstall) {
    if ($existing) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-Host "Removed scheduled task: $TaskName"
    } else {
        Write-Host "Scheduled task not found: $TaskName"
    }
    if ($StopServer) {
        $exe = Join-Path $InstallDir 'localreel.exe'
        Get-LocalReelProcesses -Exe $exe | ForEach-Object {
            Stop-Process -Id $_.Id -Force
            Write-Host "Stopped process: $($_.Id)"
        }
    }
    exit 0
}

$exe = Join-Path $InstallDir 'localreel.exe'
if (-not (Test-Path -LiteralPath $exe)) {
    throw "localreel.exe not found in $InstallDir; install LocalReel first"
}

$action = New-ScheduledTaskAction -Execute $exe -Argument 'serve' -WorkingDirectory $InstallDir
$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -DontStopOnIdleEnd `
    -StartWhenAvailable `
    -MultipleInstances IgnoreNew `
    -RestartCount 3 `
    -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit (New-TimeSpan -Seconds 0)

try {
    if ($AtStartup) {
        $trigger = New-ScheduledTaskTrigger -AtStartup
        $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
        $description = 'LocalReel media server (starts at boot, SYSTEM)'
    } else {
        $user = "$env:USERDOMAIN\$env:USERNAME"
        $trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
        $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
        $description = 'LocalReel media server (starts at logon)'
    }

    Register-ScheduledTask `
        -TaskName $TaskName `
        -Action $action `
        -Trigger $trigger `
        -Settings $settings `
        -Principal $principal `
        -Description $description `
        -Force | Out-Null
} catch {
    if ($AtStartup) {
        throw "registering -AtStartup needs an elevated PowerShell: $($_.Exception.Message)"
    }
    throw
}

if ($StartNow) {
    Start-ScheduledTask -TaskName $TaskName
    Start-Sleep -Seconds 1
}

$task = Get-ScheduledTask -TaskName $TaskName
Write-Host "Task:      $TaskName"
Write-Host "State:     $($task.State)"
Write-Host "Execute:   $exe serve"
Write-Host "Directory: $InstallDir"
Write-Host "Trigger:   $(if ($AtStartup) { 'at startup (SYSTEM)' } else { 'at logon (' + $env:USERNAME + ')' })"
Write-Host "Manage:    Get-ScheduledTask -TaskName '$TaskName' | Get-ScheduledTaskInfo"
Write-Host "Remove:    .\scripts\install-task.ps1 -Uninstall -TaskName '$TaskName'"
