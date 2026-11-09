# End-to-end: start a real LocalReel server, sign and stage a plugin, install it,
# and verify the in-place restart. Windows PowerShell 7+; no admin required.
#
#   cargo build -p localreel-server --bin localreel
#   cargo build -p localreel-plugin --bin localreel-plugin-pack --bin plugin-demo
#   pwsh -File scripts/e2e-plugin-install.ps1
#
# The script uses a temp data directory and a random loopback port; it never
# touches the operator's real config or data.

$ErrorActionPreference = 'Stop'
$root = Join-Path $env:TEMP ("localreel-e2e-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $root, (Join-Path $root 'plugin/bin'), (Join-Path $root 'library') | Out-Null
$debug = if ($env:LOCALREEL_E2E_DEBUG) { $env:LOCALREEL_E2E_DEBUG } else { Join-Path $PSScriptRoot '..\target\debug' }
$server = Join-Path $debug 'localreel.exe'
$pack = Join-Path $debug 'localreel-plugin-pack.exe'
$demo = Join-Path $debug 'plugin-demo.exe'
if (!(Test-Path $server) -or !(Test-Path $pack) -or !(Test-Path $demo)) {
    throw "build localreel, localreel-plugin-pack and plugin-demo first; missing under $debug"
}

Copy-Item $demo (Join-Path $root 'plugin/bin/plugin-demo.exe')
$manifestJson = @{
    id = 'e2e-demo'
    name = 'E2E demo'
    version = '0.1.0'
    api_version = 'localreel.plugin/v1'
    kind = 'storage'
    entry = @{ program = 'bin/plugin-demo.exe'; args = @() }
    permissions = @('storage:read')
} | ConvertTo-Json -Depth 4
[IO.File]::WriteAllText((Join-Path $root 'plugin/manifest.json'), $manifestJson, [Text.UTF8Encoding]::new($false))
Set-Content -NoNewline -Encoding ascii (Join-Path $root 'publisher.seed') ('22' * 32)
$package = Join-Path $root 'e2e-demo.lrplug'
$packed = & $pack --dir (Join-Path $root 'plugin') --out $package --signing-key (Join-Path $root 'publisher.seed')
if ($LASTEXITCODE -ne 0) { throw "pack failed: $packed" }
$publicKey = ($packed | Select-String '^public_key:\s+(\S+)').Matches.Groups[1].Value
$signature = ($packed | Select-String '^signature:\s+(\S+)').Matches.Groups[1].Value
if (!$publicKey -or !$signature) { throw "cannot parse pack output: $packed" }
$sha = (Get-FileHash -Algorithm SHA256 $package).Hash.ToLowerInvariant()

$listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$listener.Start()
$port = ([Net.IPEndPoint]$listener.LocalEndpoint).Port
$listener.Stop()

$config = Join-Path $root 'localreel.toml'
$configText = @"
[server]
host = "127.0.0.1"
port = $port

[data]
dir = "$($root -replace '\\','/')/data"

[[publishers]]
id = "e2e"
public_key = "$publicKey"

[[plugins]]
package = "$($package -replace '\\','/')"
enabled = false
sha256 = "$sha"
publisher = "e2e"
signature = "$signature"

[plugins.config]
root = "$((Join-Path $root 'library') -replace '\\','/')"
"@
[IO.File]::WriteAllText($config, $configText, [Text.UTF8Encoding]::new($false))

$oldConfig = $env:LOCALREEL_CONFIG
$env:LOCALREEL_CONFIG = $config
# Windows can carry both NO_PROXY and no_proxy; Start-Process rejects the
# case-insensitive duplicate dictionary keys.
foreach ($name in 'NO_PROXY', 'no_proxy', 'HTTP_PROXY', 'http_proxy', 'HTTPS_PROXY', 'https_proxy', 'ALL_PROXY', 'all_proxy') {
    Remove-Item "Env:$name" -ErrorAction SilentlyContinue
}
$env:NO_PROXY = 'localhost,127.0.0.1'
$stdout = Join-Path $root 'server.out'
$stderr = Join-Path $root 'server.err'
$process = Start-Process -FilePath $server -ArgumentList '--mode','server','serve','--no-open','--port',$port -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr -WindowStyle Hidden
$base = "http://127.0.0.1:$port"
$result = [ordered]@{ root = $root; port = $port; pid = $process.Id }
try {
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $deadline) {
        try {
            Invoke-RestMethod -Uri "$base/api/v1/health" -TimeoutSec 2 | Out-Null
            break
        } catch {
            Start-Sleep -Milliseconds 250
        }
    }
    if ((Get-Date) -ge $deadline) { throw "server did not become healthy: $(Get-Content $stderr -Raw)" }

    $session = New-Object Microsoft.PowerShell.Commands.WebRequestSession
    $setup = Invoke-RestMethod -Method Post -Uri "$base/api/v1/auth/setup" -ContentType 'application/json' -Body (@{ username = 'owner'; password = 'e2e-password-123' } | ConvertTo-Json) -WebSession $session
    $csrf = $setup.csrf
    $staged = Invoke-RestMethod -Method Post -Uri "$base/api/v1/plugins/packages/upload" -InFile $package -ContentType 'application/octet-stream' -WebSession $session -Headers @{
        'x-localreel-csrf' = $csrf
        'x-localreel-publisher' = 'e2e'
        'x-localreel-signature' = $signature
        'x-localreel-sha256' = $sha
    }
    $result.staged = $staged.manifest.id
    $install = Invoke-RestMethod -Method Post -Uri "$base/api/v1/plugins/packages/install" -ContentType 'application/json' -WebSession $session -Headers @{ 'x-localreel-csrf' = $csrf } -Body (@{
        id = 'e2e-demo'
        version = '0.1.0'
        config = @{ root = (Join-Path $root 'library') }
    } | ConvertTo-Json -Depth 4)
    $result.install = $install
    if ($install.restarted -ne $true) { throw "install did not hot restart: $($install | ConvertTo-Json -Compress)" }

    $deadline = (Get-Date).AddSeconds(20)
    $descriptor = $null
    while ((Get-Date) -lt $deadline) {
        $plugins = Invoke-RestMethod -Uri "$base/api/v1/plugins" -WebSession $session
        $descriptor = $plugins.plugins | Where-Object { $_.id -eq 'e2e-demo' } | Select-Object -First 1
        if ($descriptor -and $descriptor.version -eq '0.1.0' -and $descriptor.state -eq 'running') { break }
        Start-Sleep -Milliseconds 250
    }
    $result.plugin = $descriptor
    $result.configUpdated = (Select-String -Path $config -Pattern 'plugins\packages' -SimpleMatch).Count -gt 0
    if (!$descriptor -or $descriptor.state -ne 'running') { throw "plugin did not become running: $($descriptor | ConvertTo-Json -Compress)" }
    if (!$result.configUpdated) { throw 'config still points at the old package' }
} finally {
    if (!$process.HasExited) {
        & taskkill /PID $process.Id /T /F 2>$null | Out-Null
    }
    if ($null -eq $oldConfig) { Remove-Item Env:LOCALREEL_CONFIG -ErrorAction SilentlyContinue } else { $env:LOCALREEL_CONFIG = $oldConfig }
}
$result | ConvertTo-Json -Depth 8
