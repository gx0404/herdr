# PowerShell 5.1/7 的隔离回放；GUI 模式仅打开本脚本创建的命名会话。
param(
    [Parameter(Mandatory = $true)][string]$ExePath,
    [ValidateSet('powershell', 'pwsh')][string]$Shell = 'powershell',
    [switch]$Interactive
)
$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path -LiteralPath $ExePath).Path
$shellPath = (Get-Command $Shell -ErrorAction Stop).Source
$session = 'tui-compat-' + [guid]::NewGuid().ToString('N').Substring(0, 12)
$root = Join-Path ([IO.Path]::GetTempPath()) $session
[IO.Directory]::CreateDirectory($root) | Out-Null
$keys = @('HERDR_SESSION', 'HERDR_SOCKET_PATH', 'HERDR_CLIENT_SOCKET_PATH', 'HERDR_CONFIG_PATH',
    'HERDR_WORKSPACE_ID', 'HERDR_TAB_ID', 'HERDR_PANE_ID', 'XDG_CONFIG_HOME', 'XDG_STATE_HOME')
$saved = @{}
foreach ($key in $keys) { $saved[$key] = [Environment]::GetEnvironmentVariable($key); [Environment]::SetEnvironmentVariable($key, $null) }
$env:HERDR_SESSION = $session
$env:XDG_CONFIG_HOME = Join-Path $root 'config'
$env:XDG_STATE_HOME = Join-Path $root 'state'
$env:HERDR_CONFIG_PATH = Join-Path $root 'config.toml'
$shellToml = $shellPath.Replace('\', '\\').Replace('"', '\"')
$config = @"
onboarding = false
[experimental]
allow_nested = true
[terminal]
default_shell = "$shellToml"
[update]
version_check = false
manifest_check = false
"@
[IO.File]::WriteAllText($env:HERDR_CONFIG_PATH, $config, (New-Object Text.UTF8Encoding($false)))
function Invoke-Herdr([string[]]$Argv) {
    $output = & $exe @Argv
    if ($LASTEXITCODE -ne 0) { throw "Herdr failed ($LASTEXITCODE): $($Argv -join ' ')" }
    return $output
}
$server = $null
$result = [ordered]@{ shell = $Shell; session = $session; runtime = 'PENDING'; gui = 'PENDING'; cleanup = 'PENDING' }
try {
    Invoke-Herdr @('config', 'check') | Out-Null
    if ($Interactive) {
        Write-Host '请在测试会话内检查：Ctrl+B Space 主菜单；Ctrl+B / 搜索；设置页；拖动窗口边框。'
        Write-Host '持续输出时按住左键选择；后台应继续，松开后复制所见。Shift 拖选由 WezTerm 处理。'
        Write-Host '依次缩放到 60x16、80x24、120x40、160x50；中文、输入法、滚动、右键菜单均须检查。'
        Write-Host '使用 Ctrl+B d 分离返回；此脚本随后仅清理自己的会话。'
        & $exe --session $session
        if ($LASTEXITCODE -ne 0) { throw '测试 TUI 异常退出' }
        $result.runtime = 'PASS'
        $result.gui = 'PENDING: 请将人工观察结果附到此报告'
    } else {
        $server = Start-Process -FilePath $exe -ArgumentList 'server' -PassThru -WindowStyle Hidden
        $ready = $false
        for ($attempt = 0; $attempt -lt 40; $attempt++) {
            $null = & $exe pane list 2>$null
            if ($LASTEXITCODE -eq 0) { $ready = $true; break }
            Start-Sleep -Milliseconds 250
        }
        if (-not $ready) { throw '测试 server 未就绪' }
        $created = (Invoke-Herdr @('workspace', 'create', '--cwd', $root) | Out-String | ConvertFrom-Json)
        $pane = $created.result.root_pane.pane_id
        if (-not $pane) { throw '未返回 pane_id' }
        $marker = 'HERDR_TUI_' + [guid]::NewGuid().ToString('N')
        $first = $marker.Substring(0, 10)
        $last = $marker.Substring(10)
        Invoke-Herdr @('pane', 'run', $pane, "[Console]::WriteLine(('{0}{1}' -f '$first','$last')); [Console]::WriteLine(('PS_MAJOR=' + `$PSVersionTable.PSVersion.Major))") | Out-Null
        $matched = $false
        for ($attempt = 0; $attempt -lt 40; $attempt++) {
            $text = Invoke-Herdr @('pane', 'read', $pane, '--source', 'recent-unwrapped', '--lines', '40', '--format', 'text') | Out-String
            $expectedMajor = if ($Shell -eq 'pwsh') { '7' } else { '5' }
            $lines = $text -split "`r?`n" | ForEach-Object { $_.Trim() }
            if (($lines -contains $marker) -and ($lines -contains "PS_MAJOR=$expectedMajor")) { $matched = $true; break }
            Start-Sleep -Milliseconds 250
        }
        if (-not $matched) { throw 'PowerShell ConPTY 未返回标记' }
        [IO.File]::WriteAllText((Join-Path $root 'pane.txt'), $text)
        $result.runtime = 'PASS'
    }
} finally {
    try {
        $null = & $exe session stop $session --json
        if ($null -ne $server) {
            if (-not $server.WaitForExit(10000)) { Stop-Process -Id $server.Id -Force }
        }
        $null = & $exe session delete $session
        $result.cleanup = if ($LASTEXITCODE -eq 0) { 'PASS' } else { 'FAIL' }
    } catch { $result.cleanup = 'FAIL: ' + $_.Exception.Message }
    $result | ConvertTo-Json | Set-Content -Encoding utf8 (Join-Path $root 'result.json')
    foreach ($key in $keys) { [Environment]::SetEnvironmentVariable($key, $saved[$key]) }
    Write-Host "回放报告：$root"
}
