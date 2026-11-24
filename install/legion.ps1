#Requires -Version 5.1
<#
    Install the legion binary into %LOCALAPPDATA%\legion\legion.exe without requiring
    administrator privileges.

    Usage:
        .\install\legion.ps1 [-Version <version>] [-From <file>]

    By default, downloads the latest published legion-v* release matching this machine's
    architecture, verifies its SHA-256 checksum against SHA256SUMS, replaces the binary
    atomically, and adds %LOCALAPPDATA%\legion to the current user's PATH if missing.
    Pass -From <file> to install a binary already on disk (air-gapped installs and CI).
#>
[CmdletBinding()]
param(
    [string] $Version = '',
    [string] $From = '',
    [switch] $InsecureSkipVerify
)

$ErrorActionPreference = 'Stop'

# Windows PowerShell 5.1 defaults to TLS 1.0/1.1, which GitHub rejects.
if ($PSVersionTable.PSVersion.Major -lt 6) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
}

$RepoUrl = 'https://github.com/x1f4r/legion-control'
$RepoSlug = $RepoUrl -replace '^https://github\.com/', ''

# Pinned Ed25519 public key from contract/release-public-key.pem (same key pinned in legion).
$ReleasePublicKeyPem = @"
-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAB8q1DNamFF0ShMi6Wzps/0UIGEkLmHDDvQqOvuv98zg=
-----END PUBLIC KEY-----
"@

function Get-TargetTriple {
    $arch = ''
    try {
        $arch = [string] [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    } catch {
        $arch = ''
    }
    if ([string]::IsNullOrWhiteSpace($arch)) {
        $arch = [string] $env:PROCESSOR_ARCHITECTURE
    }
    switch ($arch.Trim().ToUpperInvariant()) {
        'X64'   { return 'x86_64-pc-windows-msvc' }
        'AMD64' { return 'x86_64-pc-windows-msvc' }
        'ARM64' {
            throw 'Native Windows ARM64 binaries are not published yet; pass -From with a local binary or use the x86_64 build under emulation.'
        }
        default {
            throw "Unsupported Windows architecture '$arch' (supported: x86_64)."
        }
    }
}

function Resolve-NormalizedVersion {
    param([Parameter(Mandatory = $true)][string] $Raw)
    $clean = $Raw.Trim()
    if ($clean.StartsWith('legion-v')) {
        $clean = $clean.Substring('legion-v'.Length)
    } elseif ($clean.StartsWith('v')) {
        $clean = $clean.Substring(1)
    }
    if ([string]::IsNullOrWhiteSpace($clean) -or $clean -notmatch '^[0-9A-Za-z._+-]+$') {
        throw "Invalid version '$Raw': only letters, digits, '.', '_', '+', and '-' are allowed."
    }
    return $clean
}

function Resolve-LatestVersion {
    param([Parameter(Mandatory = $true)][string] $Slug)
    $apiUrl = "https://api.github.com/repos/$Slug/releases?per_page=30"
    $headers = @{
        'Accept'     = 'application/vnd.github+json'
        'User-Agent' = 'legion-installer'
    }
    # The repository also carries non-legion releases under v* tags, so /releases/latest cannot be used.
    $releases = Invoke-RestMethod -Uri $apiUrl -Headers $headers -UseBasicParsing
    foreach ($release in $releases) {
        if (-not $release.draft -and -not $release.prerelease -and ([string] $release.tag_name).StartsWith('legion-v')) {
            return Resolve-NormalizedVersion -Raw ([string] $release.tag_name)
        }
    }
    throw "No published legion-v* release was found at $RepoUrl/releases."
}

function Get-ExpectedChecksum {
    param(
        [Parameter(Mandatory = $true)][string] $SumsPath,
        [Parameter(Mandatory = $true)][string] $AssetName
    )
    $lines = Get-Content -LiteralPath $SumsPath
    foreach ($line in $lines) {
        $trimmed = $line.Trim()
        if ([string]::IsNullOrEmpty($trimmed) -or $trimmed.StartsWith('#')) {
            continue
        }
        if ($trimmed -match '^([0-9A-Fa-f]{64})\s+\*?(.+)$') {
            $hash = $Matches[1].ToLowerInvariant()
            $file = $Matches[2].Trim()
            if ($file -eq $AssetName) {
                return $hash
            }
        }
    }
    throw "SHA256SUMS does not contain a valid SHA-256 entry for $AssetName."
}

if (-not [string]::IsNullOrWhiteSpace($From) -and -not [string]::IsNullOrWhiteSpace($Version)) {
    throw 'Pass either -From or -Version, not both.'
}

if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
    throw 'LOCALAPPDATA is not set; cannot determine the installation directory.'
}

$installDir = Join-Path -Path $env:LOCALAPPDATA -ChildPath 'legion'
$destPath = Join-Path -Path $installDir -ChildPath 'legion.exe'

$tempDir = $null
$stagedPath = $null

try {
    if (-not [string]::IsNullOrWhiteSpace($From)) {
        if (-not (Test-Path -LiteralPath $From -PathType Leaf)) {
            throw "Source file does not exist or is not a regular file: $From"
        }
        $info = Get-Item -LiteralPath $From
        if ($info.Length -le 0) {
            throw "Source file is empty: $From"
        }
        $sourceBin = $info.FullName
    } else {
        $target = Get-TargetTriple
        $resolvedVersion = if (-not [string]::IsNullOrWhiteSpace($Version)) {
            Resolve-NormalizedVersion -Raw $Version
        } else {
            Resolve-LatestVersion -Slug $RepoSlug
        }
        $tag = "legion-v$resolvedVersion"
        $asset = "legion-$resolvedVersion-$target.exe"
        $baseUrl = "$RepoUrl/releases/download/$tag"

        $tempDir = Join-Path -Path ([System.IO.Path]::GetTempPath()) -ChildPath ('legion-install-' + [Guid]::NewGuid().ToString('N'))
        $null = New-Item -ItemType Directory -Path $tempDir -Force

        $sumsPath = Join-Path -Path $tempDir -ChildPath 'SHA256SUMS'
        $downloadedBin = Join-Path -Path $tempDir -ChildPath $asset
        $headers = @{ 'User-Agent' = 'legion-installer' }

        Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS" -OutFile $sumsPath -Headers $headers -UseBasicParsing
        Invoke-WebRequest -Uri "$baseUrl/$asset" -OutFile $downloadedBin -Headers $headers -UseBasicParsing

        $expectedHash = Get-ExpectedChecksum -SumsPath $sumsPath -AssetName $asset
        $actualHash = (Get-FileHash -LiteralPath $downloadedBin -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualHash -ne $expectedHash) {
            throw "Checksum mismatch for $asset (expected $expectedHash, got $actualHash)."
        }

        if ($InsecureSkipVerify) {
            Write-Warning 'Skipping release provenance/signature verification (-InsecureSkipVerify); SHA-256 was still verified.'
        } elseif ($null -ne (Get-Command -Name 'gh' -ErrorAction SilentlyContinue)) {
            & gh attestation verify $downloadedBin `
                --repo $RepoSlug `
                --signer-workflow "$RepoSlug/.github/workflows/legion-release.yml" `
                --source-ref "refs/tags/$tag" | Out-Null
            if ($LASTEXITCODE -ne 0) {
                throw "GitHub build provenance verification failed for $tag; refusing to install."
            }
        } else {
            $opensslCmd = Get-Command -Name 'openssl' -ErrorAction SilentlyContinue
            $verifiedWithOpenSsl = $false
            if ($null -ne $opensslCmd) {
                $pubPemPath = Join-Path -Path $tempDir -ChildPath 'release-public-key.pem'
                [System.IO.File]::WriteAllText($pubPemPath, ($ReleasePublicKeyPem + "`n"), [System.Text.Encoding]::ASCII)
                $verLine = (& $opensslCmd.Source version 2>$null | Out-String).Trim()
                $pkeyCheck = & $opensslCmd.Source pkey -pubin -in $pubPemPath -noout 2>$null
                $pkeyOk = ($LASTEXITCODE -eq 0)
                $null = $pkeyCheck
                $helpText = (& $opensslCmd.Source pkeyutl -help 2>&1 | Out-String)
                if ($verLine.StartsWith('OpenSSL') -and $pkeyOk -and $helpText.Contains('-rawin')) {
                    $sigB64Path = Join-Path -Path $tempDir -ChildPath 'SHA256SUMS.sig'
                    Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS.sig" -OutFile $sigB64Path -Headers $headers -UseBasicParsing
                    $sigText = ([System.IO.File]::ReadAllText($sigB64Path)).Trim()
                    $sigBytes = [System.Convert]::FromBase64String($sigText)
                    if ($sigBytes.Length -ne 64) {
                        throw 'SHA256SUMS.sig did not decode to a 64-byte Ed25519 signature.'
                    }
                    $sigRawPath = Join-Path -Path $tempDir -ChildPath 'SHA256SUMS.sig.bin'
                    [System.IO.File]::WriteAllBytes($sigRawPath, $sigBytes)
                    & $opensslCmd.Source pkeyutl -verify -pubin -inkey $pubPemPath -rawin -in $sumsPath -sigfile $sigRawPath 2>$null | Out-Null
                    if ($LASTEXITCODE -ne 0) {
                        throw "Ed25519 signature verification failed for $tag; refusing to install."
                    }
                    $verifiedWithOpenSsl = $true
                }
            }
            if (-not $verifiedWithOpenSsl) {
                throw @"
Cannot verify who published $tag: neither 'gh' (for attestation verification) nor OpenSSL with Ed25519 -rawin support is installed.

Verify manually before installing:
  1. Install GitHub CLI (gh) and re-run this script, or run:
       gh attestation verify <file> --repo $RepoSlug --signer-workflow $RepoSlug/.github/workflows/legion-release.yml --source-ref refs/tags/$tag
  2. Or ensure OpenSSL 3 (for example from Git for Windows) is on PATH to verify SHA256SUMS.sig against contract/release-public-key.pem.
  3. Or re-run with -InsecureSkipVerify to install with SHA-256 verification only.
"@
            }
        }
        $sourceBin = $downloadedBin
    }

    if (-not (Test-Path -LiteralPath $installDir)) {
        $null = New-Item -ItemType Directory -Path $installDir -Force
    }

    # Stage beside the destination so replacement stays on the same volume. On Windows a running
    # executable cannot be overwritten directly, so rename the previous binary aside first and
    # restore it if placing the new binary fails.
    $stagedPath = Join-Path -Path $installDir -ChildPath ('legion.exe.tmp.' + [Guid]::NewGuid().ToString('N'))
    Copy-Item -LiteralPath $sourceBin -Destination $stagedPath -Force

    if (Test-Path -LiteralPath $destPath) {
        $backupPath = Join-Path -Path $installDir -ChildPath ('legion.exe.old.' + [Guid]::NewGuid().ToString('N'))
        [System.IO.File]::Move($destPath, $backupPath)
        try {
            [System.IO.File]::Move($stagedPath, $destPath)
            $stagedPath = $null
        } catch {
            if (Test-Path -LiteralPath $backupPath) {
                [System.IO.File]::Move($backupPath, $destPath)
            }
            throw
        }
        try {
            Remove-Item -LiteralPath $backupPath -Force -ErrorAction Stop
        } catch {
            # If the old executable is still running it cannot be deleted yet; legion cleans up
            # stale .old files on its next startup.
            $null = $_
        }
    } else {
        [System.IO.File]::Move($stagedPath, $destPath)
        $stagedPath = $null
    }

    # Add %LOCALAPPDATA%\legion to the user PATH once, comparing case-insensitively without trailing
    # backslashes so repeated runs never duplicate entries.
    $normalizedInstall = $installDir.TrimEnd('\')
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @()
    if (-not [string]::IsNullOrEmpty($userPath)) {
        $entries = @($userPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    }
    $alreadyOnPath = $false
    foreach ($entry in $entries) {
        if ($entry.Trim().TrimEnd('\').Equals($normalizedInstall, [System.StringComparison]::OrdinalIgnoreCase)) {
            $alreadyOnPath = $true
            break
        }
    }
    if (-not $alreadyOnPath) {
        $updatedEntries = @($entries) + @($installDir)
        $newUserPath = $updatedEntries -join ';'
        [Environment]::SetEnvironmentVariable('Path', $newUserPath, 'User')
        if ([string]::IsNullOrEmpty($env:Path)) {
            $env:Path = $installDir
        } else {
            $env:Path = "$env:Path;$installDir"
        }
    }

    $installedVersion = & $destPath --version
    if ($LASTEXITCODE -ne 0) {
        throw "Installed $destPath, but running it with --version exited with code $LASTEXITCODE."
    }
    Write-Output "Installed $installedVersion to $destPath"
    if (-not $alreadyOnPath) {
        Write-Output "Added $installDir to your user PATH (open a new terminal to use 'legion' by name)."
    }
} finally {
    if (-not [string]::IsNullOrEmpty($stagedPath) -and (Test-Path -LiteralPath $stagedPath)) {
        Remove-Item -LiteralPath $stagedPath -Force -ErrorAction SilentlyContinue
    }
    if (-not [string]::IsNullOrEmpty($tempDir) -and (Test-Path -LiteralPath $tempDir)) {
        Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
