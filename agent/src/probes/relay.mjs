// The tunnel that makes a service reachable from outside the machine.
//
// A configured relay that is down makes the service unreachable from anywhere
// but the machine itself, so it counts against health. A service with no relay
// configured is not unhealthy for the lack of one: plenty of machines are only
// ever reached over a VPN, and absence there is the normal state, not a fault.

import { detectPlatform, runCommand } from '../config.mjs';
import { runPowerShell, parsePowerShellJson } from './process.mjs';

export const NO_RELAY = { configured: false, running: false };

function probeCloudflared() {
  const platform = detectPlatform();
  if (platform === 'windows') {
    const script = [
      "$ErrorActionPreference = 'SilentlyContinue'",
      "$relay = @(Get-CimInstance -ClassName Win32_Process -Filter 'Name = ''cloudflared.exe''' | Where-Object { $_.CommandLine -and $_.CommandLine.Contains('tunnel') -and $_.CommandLine.Contains('run') })",
      '$relayCommand = Get-Command cloudflared.exe -ErrorAction SilentlyContinue',
      '[pscustomobject]@{ configured = (($relayCommand -ne $null) -or ($relay.Count -gt 0)); running = ($relay.Count -gt 0) } | ConvertTo-Json -Compress',
    ].join('\n');
    const parsed = parsePowerShellJson(runPowerShell(script, 30000).stdout);
    if (!parsed) return NO_RELAY;
    return { configured: Boolean(parsed.configured), running: Boolean(parsed.running) };
  }

  const which = runCommand('which', ['cloudflared'], { timeoutMs: 5000 });
  const configured = which.ok && which.stdout.trim().length > 0;
  if (!configured) return NO_RELAY;
  const pgrep = runCommand('pgrep', ['-f', 'cloudflared.*tunnel[[:space:]]+run'], { timeoutMs: 5000 });
  return { configured, running: pgrep.ok && pgrep.stdout.trim().length > 0 };
}

/** { configured, running } for a service's relay, or NO_RELAY when it has none. */
export function probeRelay(service) {
  if (service.relay?.type !== 'cloudflared') return NO_RELAY;
  return probeCloudflared();
}
