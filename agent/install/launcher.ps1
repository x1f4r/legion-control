# Rendered by install-windows.ps1 into <LEGIONCTL_HOME>\bin\legionctl.ps1.
$ErrorActionPreference = 'Stop'
$env:LEGIONCTL_HOME = (Resolve-Path (Join-Path $PSScriptRoot '..')).ProviderPath
& @NODE_POWERSHELL@ (Join-Path $PSScriptRoot 'launcher.mjs') @args
exit $LASTEXITCODE
