[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [string]$ReleaseBase,
    [string]$InstallDir = $(Join-Path $env:LOCALAPPDATA 'AgentDeck\bin'),
    [string]$ReceiptPath = $(Join-Path $env:LOCALAPPDATA 'AgentDeck\installation\receipt.json'),
    [string]$ArchivePath,
    [string]$ChecksumsPath,
    [switch]$Force
)

# Install only a checksum-verified release. Herdr, Tailscale, Ollama, CodexBar,
# and services are intentionally outside this script's scope.
$ErrorActionPreference = 'Stop'

function Stop-Install([string]$Message) { throw $Message }
function Resolve-FullPath([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path) -or $Path.Contains("`n") -or $Path.Contains("`r")) {
        Stop-Install 'paths must be nonblank and may not contain newlines.'
    }
    return [System.IO.Path]::GetFullPath($Path)
}
function Get-Sha256([string]$Path) { return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function Read-Receipt([string]$Path) {
    try { return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json -ErrorAction Stop }
    catch { Stop-Install "AgentDeck receipt at $Path is invalid; refusing to replace files." }
}
function Test-OwnedInstall($Receipt, [string]$BinaryPath, [string]$ExpectedInstallDir, [string]$Target) {
    if ($Receipt.schema -ne 2 -or $Receipt.install_dir -ne $ExpectedInstallDir -or $Receipt.target -ne $Target) { return $false }
    if (-not ($Receipt.binary_sha256 -match '^[0-9a-f]{64}$')) { return $false }
    if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) { return $false }
    return ((Get-Sha256 $BinaryPath) -eq $Receipt.binary_sha256)
}
function Move-Atomic([string]$Source, [string]$Destination) {
    if (Test-Path -LiteralPath $Destination -PathType Leaf) {
        [System.IO.File]::Replace($Source, $Destination, [System.Management.Automation.Language.NullString]::Value, $true)
    } else {
        [System.IO.File]::Move($Source, $Destination)
    }
}

$target = 'x86_64-pc-windows-msvc'
$archiveName = "agentdeck-$target.zip"
$InstallDir = Resolve-FullPath $InstallDir
$ReceiptPath = Resolve-FullPath $ReceiptPath
if ([string]::IsNullOrWhiteSpace($Version)) { Stop-Install 'version must be nonblank.' }
if (($ArchivePath -and -not $ChecksumsPath) -or ($ChecksumsPath -and -not $ArchivePath)) {
    Stop-Install '-ArchivePath and -ChecksumsPath must be supplied together.'
}
if (-not $ReleaseBase) {
    if ($Version -eq 'latest') {
        $ReleaseBase = 'https://github.com/JasonBates/agentdeck-rs/releases/latest/download'
    } else {
        $tag = if ($Version.StartsWith('v')) { $Version } else { "v$Version" }
        $ReleaseBase = "https://github.com/JasonBates/agentdeck-rs/releases/download/$tag"
    }
}

$binaryPath = Join-Path $InstallDir 'agentdeck.exe'
if (Test-Path -LiteralPath $binaryPath -PathType Container) {
    Stop-Install 'agentdeck.exe is a directory and cannot be replaced.'
}
$hasCollision = Test-Path -LiteralPath $binaryPath
if (Test-Path -LiteralPath $ReceiptPath -PathType Leaf) {
    $existingReceipt = Read-Receipt $ReceiptPath
    if (-not (Test-OwnedInstall $existingReceipt $binaryPath $InstallDir $target) -and -not $Force) {
        Stop-Install 'existing AgentDeck receipt or files do not match; refusing to replace them (use -Force only after inspection).'
    }
} elseif ($hasCollision -and -not $Force) {
    Stop-Install "$InstallDir already contains agentdeck.exe without an AgentDeck receipt; refusing to overwrite."
}

$scratch = Join-Path ([System.IO.Path]::GetTempPath()) ("agentdeck-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $scratch | Out-Null
try {
    $archive = Join-Path $scratch $archiveName
    $checksums = Join-Path $scratch 'SHA256SUMS'
    if ($ArchivePath) {
        if (-not (Test-Path -LiteralPath $ArchivePath -PathType Leaf) -or -not (Test-Path -LiteralPath $ChecksumsPath -PathType Leaf)) {
            Stop-Install 'local archive and checksum manifest must be regular files.'
        }
        Copy-Item -LiteralPath $ArchivePath -Destination $archive
        Copy-Item -LiteralPath $ChecksumsPath -Destination $checksums
    } else {
        Invoke-WebRequest -UseBasicParsing -Uri "$ReleaseBase/$archiveName" -OutFile $archive
        Invoke-WebRequest -UseBasicParsing -Uri "$ReleaseBase/SHA256SUMS" -OutFile $checksums
    }
    $checksumLine = (Get-Content -LiteralPath $checksums | Where-Object {
        $parts = $_.Trim() -split '\s+', 2
        $parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $archiveName
    } | Select-Object -Last 1)
    if (-not $checksumLine) { Stop-Install 'release checksum manifest does not contain the Windows archive.' }
    $expectedArchiveHash = (($checksumLine.Trim() -split '\s+', 2)[0]).ToLowerInvariant()
    if ($expectedArchiveHash -notmatch '^[0-9a-f]{64}$' -or (Get-Sha256 $archive) -ne $expectedArchiveHash) {
        Stop-Install 'release checksum verification failed.'
    }
    Expand-Archive -Force -LiteralPath $archive -DestinationPath $scratch
    $candidate = Join-Path $scratch 'agentdeck.exe'
    if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) { Stop-Install 'release archive lacks agentdeck.exe.' }
    $candidateHash = Get-Sha256 $candidate

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $newBinary = Join-Path $InstallDir ('.agentdeck.new-' + [guid]::NewGuid())
    Copy-Item -LiteralPath $candidate -Destination $newBinary
    Move-Atomic $newBinary $binaryPath

    $receiptDir = Split-Path -Parent $ReceiptPath
    New-Item -ItemType Directory -Force -Path $receiptDir | Out-Null
    $newReceipt = Join-Path $receiptDir ('.receipt.new-' + [guid]::NewGuid())
    $receiptData = [ordered]@{
        schema = 2; install_dir = $InstallDir; version = $Version; target = $target
        release_base = $ReleaseBase; archive_sha256 = $expectedArchiveHash
        binary_sha256 = $candidateHash
    } | ConvertTo-Json
    [System.IO.File]::WriteAllText($newReceipt, $receiptData, [System.Text.UTF8Encoding]::new($false))
    Move-Atomic $newReceipt $ReceiptPath

    Write-Host "Installed AgentDeck $Version ($target) to $InstallDir"
    Write-Host "Receipt: $ReceiptPath"
    Write-Host "Run: & '$binaryPath' version"
} finally {
    Remove-Item -Recurse -Force -LiteralPath $scratch -ErrorAction SilentlyContinue
}
