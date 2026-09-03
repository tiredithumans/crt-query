<#
.SYNOPSIS
    Install crt-query on Windows.

.DESCRIPTION
    Detects your target triple, resolves the newest release (so there is no
    version to keep up to date here or in the README), verifies the archive
    against that release's SHA256SUMS, installs the binary, and puts its
    directory on your user PATH. No admin rights needed.

    Re-run it to upgrade: it replaces the installed binary and clears the
    mark-of-the-web on the new download.

.PARAMETER Dir
    Install directory. Defaults to %LOCALAPPDATA%\Programs\crt-query.

.PARAMETER Version
    Install this release (e.g. v0.1.0) instead of the newest one.

.PARAMETER NoPathUpdate
    Do not touch the user PATH.

.EXAMPLE
    irm https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.ps1 | iex

.EXAMPLE
    # With arguments, the script has to become a scriptblock first:
    & ([scriptblock]::Create((irm https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.ps1))) -Dir C:\tools
#>
[CmdletBinding()]
param(
    [string] $Dir = (Join-Path $env:LOCALAPPDATA 'Programs\crt-query'),
    [string] $Version = '',
    [switch] $NoPathUpdate
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Repo = 'tiredithumans/crt-query'

# Windows PowerShell 5.1 still defaults to TLS 1.0 on some hosts, which
# github.com refuses outright.
try {
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {
    # PowerShell 7 manages this itself and the type may not be settable.
}

# --- Target triple ---------------------------------------------------------

$arch = $env:PROCESSOR_ARCHITECTURE
if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }

switch ($arch) {
    'AMD64' { $target = 'x86_64-pc-windows-msvc' }
    'ARM64' { $target = 'aarch64-pc-windows-msvc' }
    'x86'   { $target = 'i686-pc-windows-msvc' }
    default { throw "unsupported CPU architecture: $arch" }
}

# --- Release ---------------------------------------------------------------

# GitHub redirects /releases/latest/download/<asset> to the newest release's
# copy of that asset, so the version never has to be resolved through the API.
$base = if ($Version) {
    "https://github.com/$Repo/releases/download/$Version"
} else {
    "https://github.com/$Repo/releases/latest/download"
}

$label = if ($Version) { $Version } else { 'latest' }
Write-Host "Resolving the $label crt-query release..."

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("crt-query-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

# The inner try/finally clears the scratch directory; the outer try/catch
# turns a failure into a plain message, because PowerShell's default rendering
# of a multi-line exception is close to unreadable.
try {
  try {
    $sumsPath = Join-Path $tmp 'SHA256SUMS'
    Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sumsPath -UseBasicParsing

    # SHA256SUMS names every archive in the release, so it doubles as the
    # index mapping this machine's target triple to an archive -- and
    # therefore to the version, which is embedded in the archive name.
    # `*` marks a binary-mode entry and `./` is how the release workflow's
    # glob spells the names; neither is part of the file name.
    $entries = foreach ($line in Get-Content -Path $sumsPath) {
        if ($line -match '^\s*([0-9a-fA-F]{64})\s+\*?(?:\./)?(\S.*?)\s*$') {
            [pscustomobject]@{ Hash = $Matches[1].ToLower(); Name = $Matches[2] }
        }
    }

    $wanted = "-$target.zip"
    $entry = @($entries | Where-Object { $_.Name.EndsWith($wanted) })

    if ($entry.Count -eq 0) {
        $available = ($entries | ForEach-Object { "  " + $_.Name }) -join "`n"
        throw @"
the $label release has no build for ${target}.
It ships:
$available
Build from source instead:
  cargo install --git https://github.com/$Repo
"@
    }
    if ($entry.Count -gt 1) {
        throw "SHA256SUMS lists more than one archive for ${target}; refusing to guess"
    }

    $archive = $entry[0].Name
    $expected = $entry[0].Hash

    # crt-query-v0.1.0-x86_64-pc-windows-msvc.zip -> v0.1.0
    $stem = [System.IO.Path]::GetFileNameWithoutExtension($archive)
    $resolved = $stem.Substring('crt-query-'.Length)
    $resolved = $resolved.Substring(0, $resolved.Length - "-$target".Length)

    Write-Host "Downloading crt-query $resolved for $target..."
    $zipPath = Join-Path $tmp $archive
    Invoke-WebRequest -Uri "$base/$archive" -OutFile $zipPath -UseBasicParsing

    # --- Verify ------------------------------------------------------------

    $actual = (Get-FileHash -Path $zipPath -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) {
        throw "checksum mismatch for ${archive}: expected $expected, got $actual -- the download does not match the release's SHA256SUMS, so it was NOT installed"
    }
    Write-Host "Checksum verified against the release's SHA256SUMS."

    Expand-Archive -Path $zipPath -DestinationPath $tmp -Force
    $exe = Join-Path $tmp (Join-Path $stem 'crt-query.exe')
    if (-not (Test-Path -Path $exe)) {
        throw "$archive does not contain $stem\crt-query.exe"
    }

    # --- Install -----------------------------------------------------------

    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    $dest = Join-Path $Dir 'crt-query.exe'
    Copy-Item -Path $exe -Destination $dest -Force

    # Everything downloaded from the internet carries a mark-of-the-web, which
    # SmartScreen acts on. Clearing it is also what makes a re-run work as an
    # upgrade: the mark comes back with each new download.
    Unblock-File -Path $dest -ErrorAction SilentlyContinue

    $installed = (& $dest --version 2>$null | Select-Object -First 1)
    if ($LASTEXITCODE -ne 0 -or -not $installed) {
        throw "installed $dest but it would not run"
    }
    Write-Host "Installed $installed to $dest"

    # --- PATH --------------------------------------------------------------

    if (-not $NoPathUpdate) {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $onPath = $userPath -and (($userPath -split ';') -contains $Dir)
        if (-not $onPath) {
            $newPath = if ($userPath) { "$userPath;$Dir" } else { $Dir }
            [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
            Write-Host "Added $Dir to your user PATH. Open a new terminal for it to take effect."
        }
    }

    Write-Host "Shell completions: crt-query completions powershell  (see the README)"
  } finally {
    Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
  }
} catch {
    [Console]::Error.WriteLine("crt-query installer: error: " + $_.Exception.Message)
    exit 1
}
