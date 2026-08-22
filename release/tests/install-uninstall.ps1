$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$scratch = Join-Path ([System.IO.Path]::GetTempPath()) ("agentdeck-release-test-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $scratch | Out-Null

function Assert-Throws([scriptblock]$Block, [string]$Message) {
    try { & $Block; throw $Message } catch { if ($_.Exception.Message -eq $Message) { throw } }
}
function New-ReleaseFixture([string]$Directory) {
    $package = Join-Path $Directory 'package'
    New-Item -ItemType Directory -Force -Path $package | Out-Null
    Set-Content -NoNewline -LiteralPath (Join-Path $package 'agentdeck.exe') -Value 'agentdeck-test'
    $archive = Join-Path $Directory 'agentdeck-x86_64-pc-windows-msvc.zip'
    Compress-Archive -Path (Join-Path $package '*') -DestinationPath $archive
    $checksum = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
    Set-Content -NoNewline -LiteralPath (Join-Path $Directory 'SHA256SUMS') -Value "$checksum  agentdeck-x86_64-pc-windows-msvc.zip"
    return @{ Archive = $archive; Checksums = (Join-Path $Directory 'SHA256SUMS') }
}

try {
    $release = New-ReleaseFixture (Join-Path $scratch 'release assets')
    $installDir = Join-Path $scratch 'install directory'
    $receipt = Join-Path $scratch 'receipts/installation.json'
    $install = {
        & (Join-Path $root 'release/install.ps1') -ArchivePath $release.Archive -ChecksumsPath $release.Checksums -InstallDir $installDir -ReceiptPath $receipt -Version v9.9.9 @args
    }
    $uninstall = { & (Join-Path $root 'release/uninstall.ps1') -InstallDir $installDir -ReceiptPath $receipt @args }

    & $install
    if (-not (Test-Path -LiteralPath (Join-Path $installDir 'agentdeck.exe'))) { throw 'installer did not create agentdeck.exe' }
    $receiptData = Get-Content -Raw $receipt | ConvertFrom-Json
    if ($receiptData.schema -ne 2 -or $receiptData.version -ne 'v9.9.9') { throw 'receipt did not record schema and version' }
    & $install
    & $uninstall -WhatIf
    & $uninstall

    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Set-Content -NoNewline -LiteralPath (Join-Path $installDir 'agentdeck.exe') -Value 'foreign'
    Assert-Throws { & $install } 'foreign collision was overwritten without -Force'
    Assert-Throws { & $uninstall } 'receipt-less uninstall removed foreign files'
    if ((Get-Content -Raw (Join-Path $installDir 'agentdeck.exe')) -ne 'foreign') { throw 'foreign binary changed' }
    & $install -Force
    & $uninstall

    $legacyDir = Join-Path $scratch 'schema 1 install'
    $legacyReceipt = Join-Path $scratch 'schema-1-receipt.json'
    & (Join-Path $root 'release/install.ps1') -ArchivePath $release.Archive -ChecksumsPath $release.Checksums -InstallDir $legacyDir -ReceiptPath $legacyReceipt -Version v9.9.9
    $legacyData = Get-Content -Raw $legacyReceipt | ConvertFrom-Json
    $legacyData.schema = 1
    [System.IO.File]::WriteAllText($legacyReceipt, ($legacyData | ConvertTo-Json), [System.Text.UTF8Encoding]::new($false))
    Assert-Throws { & (Join-Path $root 'release/install.ps1') -ArchivePath $release.Archive -ChecksumsPath $release.Checksums -InstallDir $legacyDir -ReceiptPath $legacyReceipt -Version v9.9.9 } 'schema 1 receipt was trusted for upgrade'
    Assert-Throws { & (Join-Path $root 'release/uninstall.ps1') -InstallDir $legacyDir -ReceiptPath $legacyReceipt } 'schema 1 receipt was trusted for removal'
    if ((Get-Content -Raw (Join-Path $legacyDir 'agentdeck.exe')) -ne 'agentdeck-test') { throw 'schema 1 refusal changed the binary' }
    & (Join-Path $root 'release/install.ps1') -ArchivePath $release.Archive -ChecksumsPath $release.Checksums -InstallDir $legacyDir -ReceiptPath $legacyReceipt -Version v9.9.9 -Force
    & (Join-Path $root 'release/uninstall.ps1') -InstallDir $legacyDir -ReceiptPath $legacyReceipt

    $badChecksums = Join-Path (Split-Path $release.Checksums) 'bad-SHA256SUMS'
    Set-Content -NoNewline -LiteralPath $badChecksums -Value (('0' * 64) + '  agentdeck-x86_64-pc-windows-msvc.zip')
    $badDir = Join-Path $scratch 'checksum failure'
    $badReceipt = Join-Path $scratch 'bad-receipt.json'
    Assert-Throws { & (Join-Path $root 'release/install.ps1') -ArchivePath $release.Archive -ChecksumsPath $badChecksums -InstallDir $badDir -ReceiptPath $badReceipt } 'bad checksum was accepted'
    if (Test-Path -LiteralPath (Join-Path $badDir 'agentdeck.exe')) { throw 'bad checksum created a binary' }

    & $install
    Set-Content -NoNewline -LiteralPath (Join-Path $installDir 'agentdeck.exe') -Value 'modified'
    Assert-Throws { & $uninstall } 'modified binary was removed'
    if ((Get-Content -Raw (Join-Path $installDir 'agentdeck.exe')) -ne 'modified') { throw 'modified binary changed' }

    foreach ($script in 'install.ps1', 'uninstall.ps1', 'service.ps1', 'package-windows.ps1', 'tests/install-uninstall.ps1', 'tests/service.ps1', 'tests/archive-layout.ps1') {
        $tokens = $null
        $parseErrors = $null
        [System.Management.Automation.Language.Parser]::ParseFile((Join-Path $root "release/$script"), [ref]$tokens, [ref]$parseErrors) | Out-Null
        if ($parseErrors.Count -gt 0) { throw "PowerShell parse error in ${script}: $($parseErrors[0].Message)" }
    }
    $plan = & (Join-Path $root 'release/service.ps1') install -Binary (Join-Path $installDir 'agentdeck.exe') -Config (Join-Path $scratch 'config & spaces.toml') -TaskName AgentDeckTest -ReceiptPath (Join-Path $scratch 'service-receipt.json') -Plan | ConvertFrom-Json
    if ($plan.task_name -ne 'AgentDeckTest' -or $plan.arguments -notmatch 'config & spaces') { throw 'service plan did not preserve arguments safely' }
} finally {
    Remove-Item -Recurse -Force -LiteralPath $scratch -ErrorAction SilentlyContinue
}
