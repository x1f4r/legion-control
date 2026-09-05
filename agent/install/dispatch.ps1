# The forced command for a restricted Legion Control key on Windows.
#
# In administrators_authorized_keys or authorized_keys:
#
#   command="powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File C:\Users\<you>\.legion-control\bin\dispatch.ps1",restrict ssh-ed25519 AAAA... phone
#
# What the key asked for arrives in SSH_ORIGINAL_COMMAND. THIS SCRIPT NEVER
# INTERPRETS IT: the variable is left in the environment for the agent's own
# `dispatch` to parse with its grammar, and is never expanded into a command
# line here, so PowerShell cannot be persuaded to evaluate any part of it.
#
# The grammar the agent accepts is POSIX-shaped (single quotes, `\'` for a
# literal quote) on every platform. A client talking to a restricted key
# serialises that way regardless of what the remote login shell would have been,
# because on this path there is no shell at all.

$ErrorActionPreference = 'Stop'

$installDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$launcher = Join-Path $installDir 'legionctl.ps1'

function Fail([string] $message) {
    # One JSON object, like every other reply.
    $payload = @{ ok = $false; contract = 3; reasonCode = 'internal'; message = $message } | ConvertTo-Json -Compress
    [Console]::Out.Write($payload + "`n")
    exit 1
}

if (-not (Test-Path -LiteralPath $launcher -PathType Leaf)) {
    Fail "the stable agent launcher is not installed at $launcher"
}

$env:LEGIONCTL_RESTRICTED = '1'
$env:LEGIONCTL_CLIENT = 'restricted-ssh'

# The argument list is built from values this file chose; SSH_ORIGINAL_COMMAND is
# passed through the environment and never appears here.
& $launcher dispatch
exit $LASTEXITCODE
