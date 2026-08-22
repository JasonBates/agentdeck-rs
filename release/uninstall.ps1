[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$InstallDir = $(Join-Path $env:LOCALAPPDATA 'AgentDeck\bin'),
    [string]$ReceiptPath = $(Join-Path $env:LOCALAPPDATA 'AgentDeck\installation\receipt.json')
)

$ErrorActionPreference = 'Stop'
function Stop-Uninstall([string]$Message) { throw $Message }
function Resolve-FullPath([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path) -or $Path.Contains("`n") -or $Path.Contains("`r")) { Stop-Uninstall 'paths must be nonblank and may not contain newlines.' }
    return [System.IO.Path]::GetFullPath($Path)
}
function Get-Sha256([string]$Path) { return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }

$InstallDir = Resolve-FullPath $InstallDir
$ReceiptPath = Resolve-FullPath $ReceiptPath
$binaryPath = Join-Path $InstallDir 'agentdeck.exe'
if (-not (Test-Path -LiteralPath $ReceiptPath -PathType Leaf)) { Stop-Uninstall "no AgentDeck installation receipt at $ReceiptPath; refusing to remove files." }
try { $receipt = Get-Content -LiteralPath $ReceiptPath -Raw | ConvertFrom-Json -ErrorAction Stop }
catch { Stop-Uninstall "AgentDeck receipt at $ReceiptPath is invalid; refusing to remove files." }
if ($receipt.schema -ne 2 -or $receipt.install_dir -ne $InstallDir) { Stop-Uninstall 'receipt belongs to a different or unsupported installation; refusing to remove files.' }
if (-not ($receipt.binary_sha256 -match '^[0-9a-f]{64}$')) { Stop-Uninstall 'receipt hash is invalid; refusing to remove files.' }
if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) { Stop-Uninstall 'binary is absent; refusing to remove files.' }
if ((Get-Sha256 $binaryPath) -ne $receipt.binary_sha256) { Stop-Uninstall 'binary no longer matches the receipt; refusing to remove files.' }

if ($PSCmdlet.ShouldProcess($InstallDir, 'Remove receipt-proven AgentDeck binary')) {
    Remove-Item -Force -LiteralPath $binaryPath
    Remove-Item -Force -LiteralPath $ReceiptPath
    Write-Host "Removed AgentDeck from $InstallDir; retained config, state, caches, logs, and services."
}
