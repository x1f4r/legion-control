# Rendered by install-windows.ps1 into <LEGIONCTL_HOME>\bin\legionctl.ps1.
$ErrorActionPreference = 'Stop'
$env:LEGIONCTL_HOME = (Resolve-Path (Join-Path $PSScriptRoot '..')).ProviderPath
# Node reads these before the launcher's first line, and NODE_OPTIONS can name
# code to run (--import, --require). Nothing the agent needs comes from them.
Remove-Item Env:NODE_OPTIONS, Env:NODE_PATH -ErrorAction SilentlyContinue
& @NODE_POWERSHELL@ (Join-Path $PSScriptRoot 'launcher.mjs') @args
exit $LASTEXITCODE
