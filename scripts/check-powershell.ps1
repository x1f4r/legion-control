<#
    Analyse every PowerShell script in the repository with PSScriptAnalyzer, as CI does:

        pwsh -NoProfile -File scripts/check-powershell.ps1

    Every rule PSScriptAnalyzer runs by default. A warning, an error or a script that does not
    parse fails the check; information is listed and passes. A script with a reason to break a
    rule says so where it does, with a SuppressMessageAttribute and a comment.

    PSScriptAnalyzer has to be installed already. CI installs the version it pins.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath (Split-Path -Parent $PSScriptRoot)

if (-not (Get-Module -Name PSScriptAnalyzer)) {
    Import-Module -Name PSScriptAnalyzer
}
Write-Output ('PSScriptAnalyzer ' + (Get-Module -Name PSScriptAnalyzer).Version)

# No -- before the patterns: PowerShell may take it for its own end of parameters, and git needs
# none here.
$scripts = @(git ls-files '*.ps1' '*.psm1' '*.psd1')
if ($LASTEXITCODE -ne 0) {
    throw 'git could not list the PowerShell scripts.'
}

$failing = 0
foreach ($file in $scripts) {
    $findings = @(Invoke-ScriptAnalyzer -Path $file)
    foreach ($finding in $findings) {
        Write-Output ('{0}:{1}:{2}: {3} {4}: {5}' -f $file, $finding.Line, $finding.Column, $finding.Severity, $finding.RuleName, $finding.Message)
        if ([string] $finding.Severity -ne 'Information') {
            $failing += 1
        }
    }
}

if ($failing -gt 0) {
    Write-Output "$failing findings of warning or error severity in $($scripts.Count) PowerShell scripts."
    exit 1
}
Write-Output "Analysed $($scripts.Count) PowerShell scripts: no warnings or errors."
