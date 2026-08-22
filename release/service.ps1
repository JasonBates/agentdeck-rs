[CmdletBinding(SupportsShouldProcess)]
param(
    [Parameter(Mandatory, Position = 0)]
    [ValidateSet('install', 'uninstall')]
    [string]$Action,
    [string]$Binary = $(Join-Path $env:LOCALAPPDATA 'AgentDeck\bin\agentdeck.exe'),
    [string]$Config = $(Join-Path $env:APPDATA 'agentdeck\config.toml'),
    [string]$TaskName = 'AgentDeck',
    [string]$ReceiptPath = $(Join-Path $env:LOCALAPPDATA 'AgentDeck\service\receipt.json'),
    [string]$LogDir = $(Join-Path $env:LOCALAPPDATA 'AgentDeck\logs'),
    [switch]$Plan
)

# Register-ScheduledTask receives structured action/trigger objects, avoiding XML or
# shell substitution. The ownership receipt prevents this script from replacing or
# deleting a foreign task with the same name.
$ErrorActionPreference = 'Stop'
function Stop-ServiceAction([string]$Message) { throw $Message }
function Resolve-FullPath([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path) -or $Path.Contains("`n") -or $Path.Contains("`r")) { Stop-ServiceAction 'paths must be nonblank and may not contain newlines.' }
    return [System.IO.Path]::GetFullPath($Path)
}
function Get-StringSha256([string]$Value) {
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($Value)
    $hash = [System.Security.Cryptography.SHA256]::Create().ComputeHash($bytes)
    return ([System.BitConverter]::ToString($hash) -replace '-', '').ToLowerInvariant()
}
function Quote-WindowsArgument([string]$Value) {
    $builder = [System.Text.StringBuilder]::new()
    [void]$builder.Append('"')
    $slashes = 0
    foreach ($character in $Value.ToCharArray()) {
        if ($character -eq '\') { $slashes++; continue }
        if ($character -eq '"') {
            [void]$builder.Append(('\' * ($slashes * 2 + 1)))
            [void]$builder.Append('"')
            $slashes = 0
            continue
        }
        if ($slashes -gt 0) { [void]$builder.Append(('\' * $slashes)); $slashes = 0 }
        [void]$builder.Append($character)
    }
    if ($slashes -gt 0) { [void]$builder.Append(('\' * ($slashes * 2))) }
    [void]$builder.Append('"')
    return $builder.ToString()
}
function Read-ServiceReceipt([string]$Path) {
    try { return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json -ErrorAction Stop }
    catch { Stop-ServiceAction "AgentDeck service receipt at $Path is invalid; refusing to modify a task." }
}
function Get-TaskXml([string]$Name) { return Export-ScheduledTask -TaskName $Name -ErrorAction Stop }

$Binary = Resolve-FullPath $Binary
$Config = Resolve-FullPath $Config
$ReceiptPath = Resolve-FullPath $ReceiptPath
$LogDir = Resolve-FullPath $LogDir
if ([string]::IsNullOrWhiteSpace($TaskName) -or $TaskName.Contains('\') -or $TaskName.Contains("`n") -or $TaskName.Contains("`r")) {
    Stop-ServiceAction 'TaskName must be a nonblank, single task name.'
}
if ($Action -eq 'install' -and -not (Test-Path -LiteralPath $Binary -PathType Leaf)) { Stop-ServiceAction "AgentDeck executable does not exist: $Binary" }

$arguments = 'serve --config ' + (Quote-WindowsArgument $Config)
$scheduledAction = New-ScheduledTaskAction -Execute $Binary -Argument $arguments
$trigger = New-ScheduledTaskTrigger -AtLogOn
$settings = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -StartWhenAvailable
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited
if ($Plan) {
    [pscustomobject]@{ task_name = $TaskName; executable = $Binary; arguments = $arguments; trigger = 'logon'; run_level = 'limited'; log_directory = $LogDir } | ConvertTo-Json
    exit 0
}

$existingTask = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if ($Action -eq 'install') {
    if ($existingTask) {
        if (-not (Test-Path -LiteralPath $ReceiptPath -PathType Leaf)) { Stop-ServiceAction "task $TaskName already exists without an AgentDeck receipt; refusing to replace it." }
        $receipt = Read-ServiceReceipt $ReceiptPath
        $currentTaskHash = Get-StringSha256 (Get-TaskXml $TaskName)
        if ($receipt.schema -ne 1 -or $receipt.task_name -ne $TaskName -or $receipt.task_xml_sha256 -ne $currentTaskHash) {
            Stop-ServiceAction "task $TaskName does not match its AgentDeck receipt; refusing to replace it."
        }
    } elseif (Test-Path -LiteralPath $ReceiptPath -PathType Leaf) {
        Stop-ServiceAction "an AgentDeck service receipt exists but task $TaskName does not; inspect and remove the stale receipt explicitly."
    }
    if ($PSCmdlet.ShouldProcess($TaskName, 'Register AgentDeck per-user logon task')) {
        New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
        Register-ScheduledTask -TaskName $TaskName -Action $scheduledAction -Trigger $trigger -Settings $settings -Principal $principal -Force | Out-Null
        $receiptDir = Split-Path -Parent $ReceiptPath
        New-Item -ItemType Directory -Force -Path $receiptDir | Out-Null
        $receiptData = [ordered]@{
            schema = 1; task_name = $TaskName; binary = $Binary; config = $Config; log_dir = $LogDir
            task_xml_sha256 = Get-StringSha256 (Get-TaskXml $TaskName)
        } | ConvertTo-Json
        [System.IO.File]::WriteAllText($ReceiptPath, $receiptData, [System.Text.UTF8Encoding]::new($false))
        Write-Host "Installed AgentDeck Task Scheduler task: $TaskName"
    }
} else {
    if (-not $existingTask) { Stop-ServiceAction "AgentDeck task $TaskName is not present; refusing to modify a receipt or any other task." }
    if (-not (Test-Path -LiteralPath $ReceiptPath -PathType Leaf)) { Stop-ServiceAction "no AgentDeck service receipt at $ReceiptPath; refusing to delete task $TaskName." }
    $receipt = Read-ServiceReceipt $ReceiptPath
    $currentTaskHash = Get-StringSha256 (Get-TaskXml $TaskName)
    if ($receipt.schema -ne 1 -or $receipt.task_name -ne $TaskName -or $receipt.task_xml_sha256 -ne $currentTaskHash) {
        Stop-ServiceAction "task $TaskName does not match its AgentDeck receipt; refusing to delete it."
    }
    if ($PSCmdlet.ShouldProcess($TaskName, 'Unregister receipt-proven AgentDeck task')) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Remove-Item -Force -LiteralPath $ReceiptPath
        Write-Host 'Removed AgentDeck Task Scheduler task; retained binary, config, state, and logs.'
    }
}
