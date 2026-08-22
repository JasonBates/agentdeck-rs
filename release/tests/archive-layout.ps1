param(
    [Parameter(Mandatory)] [string]$Archive,
    [Parameter(Mandatory)] [ValidateSet('x86_64-pc-windows-msvc')] [string]$Target
)

$ErrorActionPreference = 'Stop'

function Get-PeMachine([string]$Path) {
    [byte[]]$bytes = [System.IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 0x40 -or $bytes[0] -ne 0x4d -or $bytes[1] -ne 0x5a) {
        throw 'archive agentdeck.exe is not a DOS/PE executable.'
    }
    $offset = [System.BitConverter]::ToInt32($bytes, 0x3c)
    if ($offset -lt 0 -or $offset -gt ($bytes.Length - 6)) {
        throw 'archive agentdeck.exe has an invalid PE header offset.'
    }
    if ($bytes[$offset] -ne 0x50 -or $bytes[$offset + 1] -ne 0x45 -or
        $bytes[$offset + 2] -ne 0 -or $bytes[$offset + 3] -ne 0) {
        throw 'archive agentdeck.exe does not contain a PE header.'
    }
    return [System.BitConverter]::ToUInt16($bytes, $offset + 4)
}

function Assert-ArchiveBinaryArchitecture([string]$Path, [string]$ExpectedTarget) {
    $machine = Get-PeMachine $Path
    switch ($ExpectedTarget) {
        'x86_64-pc-windows-msvc' {
            if ($machine -ne 0x8664) {
                throw ('expected AMD64 PE agentdeck.exe for {0}; got machine 0x{1:X4}' -f $ExpectedTarget, $machine)
            }
        }
        default { throw "unsupported Windows release target: $ExpectedTarget" }
    }
}

$scratch = Join-Path ([System.IO.Path]::GetTempPath()) ("agentdeck-archive-test-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $scratch | Out-Null
try {
    Expand-Archive -LiteralPath $Archive -DestinationPath $scratch
    foreach ($required in 'agentdeck.exe', 'README.md', 'LICENSE', 'install.ps1', 'uninstall.ps1', 'service.ps1', 'services') {
        if (-not (Test-Path -LiteralPath (Join-Path $scratch $required))) { throw "archive missing $required" }
    }
    Assert-ArchiveBinaryArchitecture (Join-Path $scratch 'agentdeck.exe') $Target
} finally {
    Remove-Item -Recurse -Force -LiteralPath $scratch -ErrorAction SilentlyContinue
}
