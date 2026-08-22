[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string]$Binary,
    [Parameter(Mandatory)] [string]$Target,
    [Parameter(Mandatory)] [string]$OutputDir
)

$ErrorActionPreference = 'Stop'
if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) { throw "binary does not exist: $Binary" }
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$scratch = Join-Path ([System.IO.Path]::GetTempPath()) ("agentdeck-package-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $scratch | Out-Null
try {
    Copy-Item -LiteralPath $Binary -Destination (Join-Path $scratch 'agentdeck.exe')
    Copy-Item -LiteralPath README.md, LICENSE -Destination $scratch
    Copy-Item -LiteralPath release/install.ps1, release/uninstall.ps1, release/service.ps1 -Destination $scratch
    Copy-Item -Recurse -LiteralPath release/services -Destination (Join-Path $scratch 'services')
    Compress-Archive -Path (Join-Path $scratch '*') -DestinationPath (Join-Path $OutputDir "agentdeck-$Target.zip")
} finally {
    Remove-Item -Recurse -Force -LiteralPath $scratch -ErrorAction SilentlyContinue
}
