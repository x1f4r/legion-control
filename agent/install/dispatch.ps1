# The forced command for a restricted Legion Control key on Windows.
#
# In administrators_authorized_keys or authorized_keys:
#
#   command="powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File C:\Users\<you>\.legion-control\bin\dispatch.ps1",restrict ssh-ed25519 AAAA... phone
#
# What the key asked for arrives in SSH_ORIGINAL_COMMAND. THIS SCRIPT NEVER
# INTERPRETS IT: the value is copied, as one string, into the agent's
# environment under the same fixed name for the agent's own `dispatch` to parse
# with its grammar, and is never expanded into a command line here, so
# PowerShell cannot be persuaded to evaluate any part of it.
#
# The grammar the agent accepts is POSIX-shaped (single quotes, `\'` for a
# literal quote) on every platform. A client talking to a restricted key
# serialises that way regardless of what the remote login shell would have been,
# because on this path there is no shell at all.
#
# The agent starts with an environment built from a fixed list and nothing
# else. A key's session can carry variables the client sent (sshd's AcceptEnv),
# and some of them run code: NODE_OPTIONS=--import=data:... would execute
# whatever it names before the agent's first line.

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

# What the agent and the Windows tools it runs need from the session: where
# things are, the locale, the connection it answers, and the command itself.
# Nothing here names code to load.
$allowed = @(
    'PATH', 'HOME', 'USER', 'LOGNAME', 'SHELL', 'LANG', 'LC_ALL', 'LC_CTYPE', 'LC_MESSAGES', 'TZ', 'TMPDIR',
    'SSH_CONNECTION', 'SSH_CLIENT', 'SSH_ORIGINAL_COMMAND',
    'SYSTEMROOT', 'WINDIR', 'COMSPEC', 'PATHEXT', 'USERPROFILE', 'HOMEDRIVE', 'HOMEPATH', 'APPDATA', 'LOCALAPPDATA',
    'PROGRAMDATA', 'PROGRAMFILES', 'TEMP', 'TMP', 'USERNAME', 'USERDOMAIN'
)
$process = [System.EnvironmentVariableTarget]::Process
$kept = @{}
foreach ($name in $allowed) {
    $value = [Environment]::GetEnvironmentVariable($name, $process)
    if ($null -ne $value) { $kept[$name] = $value }
}
# Cleared and refilled in this process, so the agent inherits exactly this and
# its standard streams stay the ones sshd gave this script.
foreach ($name in @([Environment]::GetEnvironmentVariables($process).Keys)) {
    [Environment]::SetEnvironmentVariable([string] $name, $null, $process)
}
foreach ($name in $kept.Keys) {
    [Environment]::SetEnvironmentVariable($name, $kept[$name], $process)
}

# This describes the access path. It is not an authenticated key identity.
$env:LEGIONCTL_RESTRICTED = '1'
$env:LEGIONCTL_CLIENT = 'restricted-ssh'

# The argument list is built from values this file chose; SSH_ORIGINAL_COMMAND is
# passed through the environment and never appears here.
& $launcher dispatch
exit $LASTEXITCODE
