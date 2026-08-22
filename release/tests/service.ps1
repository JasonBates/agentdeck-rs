$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$scratch = Join-Path ([System.IO.Path]::GetTempPath()) ("agentdeck-service-test-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $scratch | Out-Null
$taskName = "AgentDeckTest-$([guid]::NewGuid().ToString('N'))"
$foreignTaskName = "AgentDeckForeign-$([guid]::NewGuid().ToString('N'))"
$receipt = Join-Path $scratch 'receipt.json'
$logDir = Join-Path $scratch 'logs with spaces'
try {
    $config = Join-Path $scratch 'config & spaces.toml'
    Set-Content -NoNewline -LiteralPath $config -Value ''
    $binary = $env:ComSpec
    & (Join-Path $root 'release/service.ps1') install -Binary $binary -Config $config -TaskName $taskName -ReceiptPath $receipt -LogDir $logDir
    if (-not (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue)) { throw 'service script did not register a task' }
    if (-not (Test-Path -LiteralPath $receipt -PathType Leaf)) { throw 'service script did not write a receipt' }
    if (-not (Test-Path -LiteralPath $logDir -PathType Container)) { throw 'service script did not create its log directory' }
    & (Join-Path $root 'release/service.ps1') uninstall -TaskName $taskName -ReceiptPath $receipt
    if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) { throw 'service script did not remove its task' }

    $foreignAction = New-ScheduledTaskAction -Execute $binary
    $foreignTrigger = New-ScheduledTaskTrigger -AtLogOn
    Register-ScheduledTask -TaskName $foreignTaskName -Action $foreignAction -Trigger $foreignTrigger | Out-Null
    try {
        & (Join-Path $root 'release/service.ps1') install -Binary $binary -Config $config -TaskName $foreignTaskName -ReceiptPath (Join-Path $scratch 'foreign-receipt.json')
        throw 'service script replaced a foreign task'
    } catch {
        if ($_.Exception.Message -eq 'service script replaced a foreign task') { throw }
    }
} finally {
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $foreignTaskName -Confirm:$false -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force -LiteralPath $scratch -ErrorAction SilentlyContinue
}
