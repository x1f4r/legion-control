#Requires -Version 5.1
<#
    Run a package or tool download command on PowerShell and retry only on
    transient network, mirror, or HTTP 429/5xx fetch failures. Real packaging,
    compilation, and local failures exit immediately on the first attempt.

    Usage:
        ./scripts/retry-fetch.ps1 [--] <command> [args...]
        ./scripts/retry-fetch.ps1 -VerifyCommand ffmpeg -- choco install ffmpeg -y --no-progress
        ./scripts/retry-fetch.ps1 -ScriptBlock { Install-Module -Name PSScriptAnalyzer -RequiredVersion 1.25.0 -Scope CurrentUser -Force }
#>
[CmdletBinding(DefaultParameterSetName = 'Command')]
param(
    [ValidateRange(1, 20)]
    [int] $Attempts = 4,

    [string] $Delays = '10,30,60',

    [string] $VerifyCommand = '',

    [Parameter(ParameterSetName = 'ScriptBlock', Mandatory = $true)]
    [scriptblock] $ScriptBlock,

    [Parameter(ParameterSetName = 'Command', Position = 0, Mandatory = $true)]
    [string] $Command,

    [Parameter(ParameterSetName = 'Command', Position = 1, ValueFromRemainingArguments = $true)]
    [string[]] $Arguments = @()
)

$ErrorActionPreference = 'Stop'

function Resolve-DelayList {
    param([Parameter(Mandatory = $true)][string] $Csv)

    if ([string]::IsNullOrWhiteSpace($Csv)) {
        throw 'retry-fetch: -Delays must not be empty.'
    }

    $parts = $Csv.Split(',')
    $values = New-Object -TypeName 'System.Collections.Generic.List[int]'
    foreach ($part in $parts) {
        $trimmed = $part.Trim()
        $parsed = 0
        if ([string]::IsNullOrEmpty($trimmed) -or -not [int]::TryParse($trimmed, [ref] $parsed) -or $parsed -lt 0) {
            throw "retry-fetch: delay must be a non-negative integer in seconds (got '$part')."
        }
        $values.Add($parsed)
    }
    return $values.ToArray()
}

function Test-NonFetchFailure {
    param([Parameter(Mandatory = $true)][string] $Output)

    $patterns = @(
        'error: could not compile',
        '(?m)^error\[E\d+\]:',
        'npm (err!|error) code EUSAGE',
        'can only install packages when your package\.json and package-lock\.json',
        'the package was not found',
        'no match was found for the specified search criteria',
        'capability name .* was not recognized',
        'failed to find package',
        'is not recognized as (a|the) name of a cmdlet',
        'unknown (target|toolchain|command|option)'
    )
    foreach ($pattern in $patterns) {
        if ($Output -imatch $pattern) {
            return $true
        }
    }
    return $false
}

function Test-RetryableFetchError {
    param([Parameter(Mandatory = $true)][string] $Output)

    if ([string]::IsNullOrWhiteSpace($Output)) {
        return $false
    }
    if (Test-NonFetchFailure -Output $Output) {
        return $false
    }

    $patterns = @(
        'failed to fetch',
        'failed to download',
        'download failed',
        'error downloading',
        'download of .* failed',
        'could not download',
        'error sending request',
        'spurious network error',
        'fetch failed',
        'unable to resolve package source',
        'temporary failure resolving',
        'temporary failure in name resolution',
        'could not resolve',
        'name or service not known',
        'the remote name could not be resolved',
        'no such host',
        'dns error',
        'could not connect',
        'unable to connect',
        'connection timed out',
        'connect timed out',
        'read timed out',
        'operation timed out',
        'timed out',
        'connection reset',
        'connection refused',
        'connection failed',
        'connection closed',
        'connection was forcibly closed',
        'underlying connection was closed',
        'unable to read data from the transport connection',
        'unexpected eof',
        'premature close',
        'socket hang up',
        'empty reply from server',
        'broken pipe',
        'network is unreachable',
        'no route to host',
        'ETIMEDOUT|ECONNRESET|ECONNREFUSED|EAI_AGAIN|ENOTFOUND|ESOCKETTIMEDOUT|ERR_SOCKET_TIMEOUT|EHOSTUNREACH|ENETUNREACH',
        'sockettimeoutexception|connectexception|unknownhostexception|sslhandshakeexception|sslexception',
        '(ssl|tls).*(reset|eof|timeout|timed out|connect|handshake|wrong version|unexpected)',
        '(http(/[0-9.]+)?|status( code)?|returned error:?|response code:?|server returned)\s*(429|500|502|503|504)([^0-9]|$)',
        '(^|[^0-9])(429|500|502|503|504)\s*\(?(too many requests|internal server error|bad gateway|service unavailable|gateway timeout)',
        '(too many requests|bad gateway|service unavailable|gateway timeout)',
        'curl: \((6|7|18|28|35|52|55|56|92)\)',
        '0x800f0954|0x80072ee2|0x80072ee7|0x80072efd|0x80072efe|0x80072f78|0x802440'
    )
    foreach ($pattern in $patterns) {
        if ($Output -imatch $pattern) {
            return $true
        }
    }
    return $false
}

function Test-CommandPresent {
    param([Parameter(Mandatory = $true)][string] $Name)

    if ($null -ne (Get-Command -Name $Name -ErrorAction SilentlyContinue)) {
        return $true
    }

    # On Windows, installers such as Chocolatey update Machine/User PATH in the registry;
    # merge any newly added entries into the current process PATH before checking again.
    if ($env:OS -eq 'Windows_NT') {
        $currentEntries = @($env:PATH -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        $seen = New-Object -TypeName 'System.Collections.Generic.HashSet[string]' -ArgumentList @([System.StringComparer]::OrdinalIgnoreCase)
        $merged = New-Object -TypeName 'System.Collections.Generic.List[string]'
        foreach ($entry in $currentEntries) {
            if ($seen.Add($entry)) {
                $merged.Add($entry)
            }
        }
        foreach ($scope in @('Machine', 'User')) {
            $scopePath = [Environment]::GetEnvironmentVariable('Path', $scope)
            if (-not [string]::IsNullOrWhiteSpace($scopePath)) {
                foreach ($entry in ($scopePath -split ';')) {
                    if (-not [string]::IsNullOrWhiteSpace($entry) -and $seen.Add($entry)) {
                        $merged.Add($entry)
                    }
                }
            }
        }
        $env:PATH = $merged -join ';'
        if ($null -ne (Get-Command -Name $Name -ErrorAction SilentlyContinue)) {
            return $true
        }
    }

    return $false
}

function Format-ExceptionText {
    param([Parameter(Mandatory = $true)][System.Management.Automation.ErrorRecord] $ErrorRecord)

    $parts = New-Object -TypeName 'System.Collections.Generic.List[string]'
    if ($null -ne $ErrorRecord.ErrorDetails -and -not [string]::IsNullOrWhiteSpace($ErrorRecord.ErrorDetails.Message)) {
        $parts.Add([string] $ErrorRecord.ErrorDetails.Message)
    }
    $ex = $ErrorRecord.Exception
    while ($null -ne $ex) {
        if (-not [string]::IsNullOrWhiteSpace($ex.Message)) {
            $parts.Add([string] $ex.Message)
        }
        $ex = $ex.InnerException
    }
    if ($parts.Count -eq 0) {
        $parts.Add([string] $ErrorRecord)
    }
    return ($parts -join "`n")
}

$delayValues = @(Resolve-DelayList -Csv $Delays)

if ($PSCmdlet.ParameterSetName -eq 'Command' -and $Command -eq '--') {
    if ($Arguments.Count -lt 1) {
        throw 'retry-fetch: missing command after --.'
    }
    $Command = $Arguments[0]
    if ($Arguments.Count -gt 1) {
        $Arguments = @($Arguments[1..($Arguments.Count - 1)])
    } else {
        $Arguments = @()
    }
}

for ($attempt = 1; $attempt -le $Attempts; $attempt++) {
    $lines = New-Object -TypeName 'System.Collections.Generic.List[string]'
    $rc = 0
    $prevEap = $ErrorActionPreference

    try {
        if ($PSCmdlet.ParameterSetName -eq 'ScriptBlock') {
            $ErrorActionPreference = 'Stop'
            & $ScriptBlock 2>&1 | ForEach-Object {
                if ($_ -is [System.Management.Automation.ErrorRecord]) {
                    $text = $_.ToString()
                    $lines.Add($text)
                    [Console]::Error.WriteLine($text)
                } else {
                    $text = [string] $_
                    $lines.Add($text)
                    Write-Output $text
                }
            }
        } else {
            $cmdInfo = Get-Command -Name $Command -ErrorAction Stop
            $isNativeApp = $cmdInfo.CommandType -eq [System.Management.Automation.CommandTypes]::Application
            $ErrorActionPreference = if ($isNativeApp) { 'Continue' } else { 'Stop' }
            & $Command @Arguments 2>&1 | ForEach-Object {
                if ($_ -is [System.Management.Automation.ErrorRecord]) {
                    $text = $_.ToString()
                    $lines.Add($text)
                    [Console]::Error.WriteLine($text)
                } else {
                    $text = [string] $_
                    $lines.Add($text)
                    Write-Output $text
                }
            }
            if ($isNativeApp -and $null -ne $LASTEXITCODE) {
                $rc = [int] $LASTEXITCODE
            }
        }
    } catch {
        $errText = Format-ExceptionText -ErrorRecord $_
        $lines.Add($errText)
        [Console]::Error.WriteLine($errText)
        $rc = 1
    } finally {
        $ErrorActionPreference = $prevEap
    }

    if ($rc -eq 0 -and -not [string]::IsNullOrWhiteSpace($VerifyCommand)) {
        if (-not (Test-CommandPresent -Name $VerifyCommand)) {
            [Console]::Error.WriteLine("retry-fetch: expected command '$VerifyCommand' was not found on PATH after attempt $attempt/$Attempts.")
            $rc = 1
        }
    }

    if ($rc -eq 0) {
        exit 0
    }

    $combinedOutput = $lines -join "`n"
    if (-not (Test-RetryableFetchError -Output $combinedOutput)) {
        [Console]::Error.WriteLine("retry-fetch: failure on attempt $attempt/$Attempts is not a transient fetch error; not retrying.")
        exit $rc
    }

    if ($attempt -ge $Attempts) {
        [Console]::Error.WriteLine("retry-fetch: attempt $attempt/$Attempts failed with transient fetch error; giving up.")
        exit $rc
    }

    $delayIdx = $attempt - 1
    if ($delayIdx -ge $delayValues.Count) {
        $delayIdx = $delayValues.Count - 1
    }
    $delay = $delayValues[$delayIdx]
    [Console]::Error.WriteLine("retry-fetch: attempt $attempt/$Attempts failed with transient fetch error; retrying in ${delay}s...")
    if ($delay -gt 0) {
        Start-Sleep -Seconds $delay
    }
}
