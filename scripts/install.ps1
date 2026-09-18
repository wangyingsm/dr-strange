<#
.SYNOPSIS
  Dr Strange installer for Windows.

.DESCRIPTION
  Downloads a released archive from GitHub, verifies its SHA-256 against the
  published .sha256 sidecar, and installs the binary. A missing or mismatching
  sidecar is a hard failure unless -InsecureSkipChecksum is given. Nothing is
  compiled; no toolchain is required. Works with both Windows PowerShell 5.1
  and PowerShell 7+.

    irm https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.ps1 | iex

  To pass options, run the script as a block rather than piping it to iex:

    & ([scriptblock]::Create((irm https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.ps1))) -Bin drsg-mcp

.PARAMETER Bin
  Binary to install: drsg (default), drsg-mcp, or all.

.PARAMETER Version
  Release to install, e.g. v1.1.0. Defaults to the latest release.

.PARAMETER Dir
  Installation directory. Defaults to %LOCALAPPDATA%\Programs\drsg\bin.

.PARAMETER InsecureSkipChecksum
  Install even when the archive's .sha256 sidecar is missing or does not
  match. Insecure, as the name says; DRSG_INSECURE_SKIP_CHECKSUM=1 is the same.
#>
param(
    [ValidateSet('drsg', 'drsg-mcp', 'all')]
    [string]$Bin = $(if ($env:DRSG_INSTALL_BIN) { $env:DRSG_INSTALL_BIN } else { 'drsg' }),
    [string]$Version = $(if ($env:DRSG_VERSION) { $env:DRSG_VERSION } else { 'latest' }),
    [string]$Dir = $(if ($env:DRSG_INSTALL_DIR) { $env:DRSG_INSTALL_DIR } else { "$env:LOCALAPPDATA\Programs\drsg\bin" }),
    [switch]$InsecureSkipChecksum = $($env:DRSG_INSECURE_SKIP_CHECKSUM -and $env:DRSG_INSECURE_SKIP_CHECKSUM -ne '0')
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'  # a visible progress bar makes Invoke-WebRequest far slower
# Windows PowerShell 5.1 defaults to TLS 1.0, which GitHub refuses.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$repo = 'wangyingsm/dr-strange'
$bins = if ($Bin -eq 'all') { @('drsg', 'drsg-mcp') } else { @($Bin) }

# --- target triple ----------------------------------------------------------
# The x64 build runs on Arm64 Windows through the OS emulation layer.
$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -notin 'AMD64', 'ARM64', 'x86') {
    throw "unsupported architecture: $arch - build from source instead (https://github.com/$repo)"
}
$target = 'x86_64-pc-windows-msvc'

# --- release version --------------------------------------------------------
if ($Version -eq 'latest') {
    $Version = (Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest" `
            -Headers @{ 'User-Agent' = 'drsg-installer' }).tag_name
    if ($Version -notmatch '^v\d') {
        throw 'could not determine the latest release; pass -Version vX.Y.Z'
    }
}

$archive = "dr-strange-$Version-$target.zip"
$base = "https://github.com/$repo/releases/download/$Version"
$tmp = Join-Path ([IO.Path]::GetTempPath()) ('drsg-install-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

try {
    Write-Host "Dr Strange $Version ($target)"
    Write-Host "  downloading $archive"
    try {
        Invoke-WebRequest -Uri "$base/$archive" -OutFile (Join-Path $tmp $archive) -UseBasicParsing
    } catch {
        throw "download failed - $Version may not ship an asset for ${target}: $base/$archive"
    }

    if ($InsecureSkipChecksum) {
        Write-Warning 'InsecureSkipChecksum - installing without verifying the archive'
    } else {
        $sumFile = Join-Path $tmp "$archive.sha256"
        try {
            Invoke-WebRequest -Uri "$base/$archive.sha256" -OutFile $sumFile -UseBasicParsing
        } catch {
            throw "no checksum published for $archive ($base/$archive.sha256) - refusing to install an unverified archive (pass -InsecureSkipChecksum to override)"
        }
        $expected = ((Get-Content $sumFile -Raw).Trim() -split '\s+')[0]
        if ($expected -notmatch '^[0-9a-fA-F]{64}$') {
            throw "the published checksum for $archive is not a SHA-256: $expected"
        }
        $actual = (Get-FileHash (Join-Path $tmp $archive) -Algorithm SHA256).Hash
        if ($actual -ne $expected) { throw "checksum mismatch for ${archive}: expected $expected, got $actual" }
        Write-Host '  checksum verified'
    }

    Expand-Archive -Path (Join-Path $tmp $archive) -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Path $Dir -Force | Out-Null

    foreach ($b in $bins) {
        $src = Get-ChildItem -Path $tmp -Filter "$b.exe" -Recurse -File | Select-Object -First 1
        if (-not $src) { throw "$b.exe is not present in $archive" }
        Copy-Item $src.FullName (Join-Path $Dir "$b.exe") -Force
        Write-Host "  installed $Dir\$b.exe"
    }
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

# --- PATH -------------------------------------------------------------------
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $Dir) {
    [Environment]::SetEnvironmentVariable('Path', "$userPath;$Dir".Trim(';'), 'User')
    $env:Path = "$env:Path;$Dir"
    Write-Host "  added $Dir to your user PATH (open a new terminal for it to take effect)"
}

foreach ($b in $bins) {
    switch ($b) {
        'drsg' { Write-Host 'Run: drsg --db graph.drsg serve' }
        'drsg-mcp' { Write-Host 'Run: drsg-mcp --db C:\path\to\graph.drsg  (normally launched by an MCP host; no argument in a repository prepared by drsg init)' }
    }
}
