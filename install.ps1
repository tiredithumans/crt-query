<#
.SYNOPSIS
    Install crt-query on Windows.

.DESCRIPTION
    Detects your target triple, resolves the newest release (so there is no
    version to keep up to date here or in the README), verifies the archive
    against that release's SHA256SUMS, installs the binary, and puts its
    directory on your user PATH. No admin rights needed.

    Re-run it to upgrade: the new binary is staged beside the installed one,
    cleared of the mark-of-the-web and shown to run before it replaces it.

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

# ARM64 takes the native aarch64 build; the x64 entry behind it serves a
# -Version pin predating it -- Windows on ARM runs x64 under emulation, so
# the pin still installs something that works. No 'x86' case: a 32-bit build
# has never shipped, so that branch could only resolve to an archive that
# does not exist.
switch ($arch) {
    'AMD64' { $targets = @('x86_64-pc-windows-msvc') }
    'ARM64' { $targets = @('aarch64-pc-windows-msvc', 'x86_64-pc-windows-msvc') }
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
# flattens a failure into a single-line terminating error, because PowerShell's
# default rendering of a multi-line exception is close to unreadable.
try {
  try {
    $sumsPath = Join-Path $tmp 'SHA256SUMS'
    Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sumsPath -UseBasicParsing

    # SHA256SUMS doubles as the index mapping this triple to an archive --
    # and to the version embedded in the archive name. `*` marks a binary-mode
    # entry and `./` is how the release workflow's glob spells the names;
    # neither is part of the file name.
    $entries = foreach ($line in Get-Content -Path $sumsPath) {
        if ($line -match '^\s*([0-9a-fA-F]{64})\s+\*?(?:\./)?(\S.*?)\s*$') {
            [pscustomobject]@{ Hash = $Matches[1].ToLower(); Name = $Matches[2] }
        }
    }

    # Take the first target this release actually ships. On ARM64 that is the
    # native build, and the emulated x64 build only for an older release that
    # has no native one.
    $target = $null
    $entry = @()
    foreach ($candidate in $targets) {
        $match = @($entries | Where-Object { $_.Name.EndsWith("-$candidate.zip") })
        if ($match.Count -gt 0) {
            if ($candidate -ne $targets[0]) {
                Write-Host "The $label release has no native $($targets[0]) build; installing the $candidate build, which Windows runs under emulation."
            }
            $target = $candidate
            $entry = $match
            break
        }
    }

    if ($entry.Count -eq 0) {
        $target = $targets[0]
        $available = ($entries | ForEach-Object { "  " + $_.Name }) -join "`n"
        throw @"
the $label release has no build for ${target}.
It ships:
$available
Build from source instead:
  cargo install --locked --git https://github.com/$Repo
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

    # Capture .FullName: a relative -Dir would otherwise reach the PATH write
    # verbatim, and a relative entry in HKCU\Environment\Path is resolved by
    # every future process against its own working directory, surviving
    # reboots. Also normalises a trailing separator, so `-Dir C:\tools\` no
    # longer appends a second C:\tools.
    $Dir = (New-Item -ItemType Directory -Path $Dir -Force).FullName
    $dest = Join-Path $Dir 'crt-query.exe'

    # Stage, verify, then swap -- the same shape as install.sh. Copying over
    # $dest first and checking afterwards means a binary that cannot run here
    # has already replaced a working one, with $tmp cleared by the finally below
    # and nothing left to restore.
    $staged = Join-Path $Dir (".crt-query.new." + [Guid]::NewGuid().ToString('N') + ".exe")
    try {
        Copy-Item -Path $exe -Destination $staged -Force

        # Internet downloads carry a mark-of-the-web, which SmartScreen acts
        # on and which can block execution -- clear it before the check or it
        # fails the very check meant to prove the download is good. Clearing
        # it is also what makes a re-run work as an upgrade; the mark comes
        # back each time.
        Unblock-File -Path $staged -ErrorAction SilentlyContinue

        # Capture first, slice after: `Select-Object -First 1` short-circuits
        # the pipeline, and a short-circuited native command never sets
        # $LASTEXITCODE -- a terminating error under the StrictMode above, so
        # the check meant to confirm a good install was the thing that failed
        # it.
        #
        # stderr goes to a file so the loader's own words reach the message.
        # Not 2>&1: under $ErrorActionPreference = 'Stop' a native command's
        # stderr becomes error records that terminate here.
        $errPath = Join-Path $tmp 'runerr.txt'
        $versionOutput = & $staged --version 2>$errPath
        if ($LASTEXITCODE -ne 0 -or -not $versionOutput) {
            $why = if (Test-Path -Path $errPath) {
                Get-Content -Path $errPath -TotalCount 1
            } else { $null }
            if (-not $why) { $why = 'no output' }
            throw "the downloaded crt-query.exe does not run on this system, so nothing was changed: $why"
        }
        $installed = @($versionOutput)[0]

        Move-Item -Path $staged -Destination $dest -Force
        $staged = $null
    } finally {
        if ($staged) { Remove-Item -Path $staged -Force -ErrorAction SilentlyContinue }
    }
    Write-Host "Installed $installed to $dest"

    # --- PATH --------------------------------------------------------------

    if (-not $NoPathUpdate) {
        # Go to the registry rather than [Environment]::Get/SetEnvironmentVariable:
        # Get expands %USERPROFILE% and friends, Set writes the expanded result
        # back as REG_SZ, and together they silently flatten unexpanded entries
        # another installer put there on purpose -- rustup's
        # %USERPROFILE%\.cargo\bin is the one people hit, and the flattened
        # copy stops following the account it was written for.
        $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
        try {
            $hasPath = @($key.GetValueNames()) -contains 'Path'
            $userPath = $key.GetValue(
                'Path', '',
                [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            $onPath = $userPath -and (($userPath -split ';') -contains $Dir)
            if (-not $onPath) {
                # A new value is REG_EXPAND_SZ so a later %VAR% entry still works.
                $kind = if ($hasPath) { $key.GetValueKind('Path') }
                        else { [Microsoft.Win32.RegistryValueKind]::ExpandString }
                $newPath = if ($userPath) { "$userPath;$Dir" } else { $Dir }
                $key.SetValue('Path', $newPath, $kind)
                # Writing the key directly skips the WM_SETTINGCHANGE broadcast
                # SetEnvironmentVariable sends, so a new terminal is required
                # rather than merely recommended.
                Write-Host "Added $Dir to your user PATH. Open a new terminal for it to take effect."
            }
        } finally {
            if ($key) { $key.Dispose() }
        }
    }

    Write-Host "Shell completions: crt-query completions powershell  (see the README)"
  } finally {
    Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
  }
} catch {
    # `throw` a flat string, never `exit`: both documented invocations run
    # this script inside the caller's own session (`irm ... | iex` and the
    # .EXAMPLE scriptblock), where a top-level `exit` unwinds the host
    # itself -- closing the window or killing an interactive shell. The
    # success path had no `exit`, so the asymmetry pointed the wrong way: a
    # clean install left the console alive and the checksum mismatch, the one
    # outcome the user most needs to read, was the one that took it away.
    #
    # Re-throwing the caught exception object would restore the unreadable
    # multi-line rendering this catch exists to avoid, hence the flat string.
    # That still terminates with exit code 1 under `pwsh -File`, and returns
    # an interactive host to its prompt.
    throw "crt-query installer: error: " + $_.Exception.Message
}
