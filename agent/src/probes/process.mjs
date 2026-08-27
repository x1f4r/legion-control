// How a service is started, stopped and looked at.
//
// systemd-user   a user unit we own.
// systemd-system a system unit, reached with non-interactive sudo.
// scheduled-task a Windows task. Stop-ScheduledTask only *asks* the task to
//                stop, so every stop has to be followed by polling until the
//                process is really gone, and "running" is decided by looking for
//                the process rather than by asking the scheduler.
// app            a desktop app. There is no service and no supervisor, so "up"
//                is simply the app being open. Stopping means asking it to quit
//                with AppleScript, which is also exactly what lets a staged
//                update apply. The same "only ask, never force" rule as the
//                Windows task applies, only more so, because the app in question
//                is the one the user is sitting in front of.
// command        three argv arrays.
// none           nothing to start or stop.

import { describeFailure, detectPlatform, runArgv, runCommand } from '../config.mjs';
import * as appProvider from '../providers/app.mjs';
import { packageRoot } from '../providers/npm.mjs';

/** Single-quoted PowerShell literal; a literal quote is doubled. */
function psLiteral(value) {
  return `'${String(value).replace(/'/g, "''")}'`;
}

export function runPowerShell(script, timeoutMs = 30000) {
  return runCommand(
    'powershell.exe',
    ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-Command', script],
    { timeoutMs },
  );
}

/** PowerShell can prepend warnings, so pick the line that actually looks like JSON. */
export function parsePowerShellJson(stdout) {
  const text = (stdout || '').trim();
  if (!text) return null;
  const candidates = [text, ...text.split('\n').map((line) => line.trim())];
  for (const candidate of candidates) {
    if (!candidate.startsWith('{') && !candidate.startsWith('[')) continue;
    try {
      return JSON.parse(candidate);
    } catch {
      /* try the next line */
    }
  }
  return null;
}

/**
 * What a scheduled task's process is recognised by. The configured `match`
 * wins; without one, an npm service is recognised by the directory npm
 * installed it into, which every invocation of it carries in its command line.
 */
export function taskMatch(service) {
  if (service.process?.match) return service.process.match;
  if (service.kind === 'npm') return packageRoot(service, { allowProbe: false });
  return service.name;
}

function systemctl(service, verb, timeoutMs) {
  const unit = service.process.unit;
  if (service.process.type === 'systemd-system') {
    // Non-interactive sudo on purpose: an ssh session has nowhere to type a
    // password and a prompt would hang here until the timeout.
    return runCommand('sudo', ['-n', 'systemctl', verb, unit], { timeoutMs });
  }
  return runCommand('systemctl', ['--user', verb, unit], { timeoutMs });
}

function systemdProbe(service) {
  const active = systemctl(service, 'is-active', 10000);
  const state = active.stdout.trim();
  return {
    running: state === 'active',
    unitState: state || 'unknown',
    pids: [],
    // An empty answer means systemctl told us nothing at all, which is not the
    // same as "inactive" and must not be read as "safe to replace the files".
    error: state ? null : describeFailure(active),
  };
}

function windowsTaskScript(service, { includeRelay }) {
  const needle = psLiteral(taskMatch(service));
  const port = service.health?.type === 'http' ? service.health.port : null;
  const portFlag = port ? psLiteral(`--port ${port}`) : null;
  const lines = [
    "$ErrorActionPreference = 'SilentlyContinue'",
    `$needle = ${needle}`,
    // The command line is matched with its runs of backslashes collapsed. An npm
    // cmd shim builds the script path as "%dp0%\node_modules\<pkg>\..." and
    // %~dp0 already ends in a backslash, so the real command line carries
    // "...\npm\\node_modules\<pkg>\..." — a plain substring test against the
    // joined package root would never match and the service would look
    // permanently stopped.
    // $PID is excluded because this script carries the needle in its own
    // command line: without that, the probe would find itself and every service
    // would look permanently running.
    '$procs = @(Get-CimInstance -ClassName Win32_Process | Where-Object { $_.ProcessId -ne $PID -and $_.CommandLine -and ' +
      "$_.CommandLine.ToLower().Replace('\\\\', '\\').Contains($needle.ToLower().Replace('\\\\', '\\'))" +
      (portFlag
        ? ` -and ((-not $_.CommandLine.Contains('--port')) -or $_.CommandLine.Contains(${portFlag}))`
        : '') +
      ' })',
  ];
  if (includeRelay) {
    lines.push(
      "$relay = @(Get-CimInstance -ClassName Win32_Process -Filter 'Name = ''cloudflared.exe''' | Where-Object { $_.CommandLine -and $_.CommandLine.Contains('tunnel') -and $_.CommandLine.Contains('run') })",
      '$relayCommand = Get-Command cloudflared.exe -ErrorAction SilentlyContinue',
    );
  }
  lines.push(
    '[pscustomobject]@{ running = ($procs.Count -gt 0); pids = (($procs | ForEach-Object { [string]$_.ProcessId }) -join \',\')' +
      (includeRelay
        ? '; relayConfigured = (($relayCommand -ne $null) -or ($relay.Count -gt 0)); relayRunning = ($relay.Count -gt 0)'
        : '') +
      ' } | ConvertTo-Json -Compress',
  );
  return lines.join('\n');
}

function windowsTaskProbe(service, { includeRelay }) {
  const result = runPowerShell(windowsTaskScript(service, { includeRelay }), 30000);
  const parsed = parsePowerShellJson(result.stdout);
  if (!parsed) {
    return { running: false, unitState: 'unknown', pids: [], error: describeFailure(result) };
  }
  const pids = String(parsed.pids ?? '')
    .split(',')
    .map((value) => Number(value.trim()))
    .filter((value) => Number.isInteger(value) && value > 0);

  const probe = {
    running: Boolean(parsed.running),
    unitState: parsed.running ? 'running' : 'stopped',
    pids,
    error: null,
  };
  if (includeRelay) {
    probe.relay = { configured: Boolean(parsed.relayConfigured), running: Boolean(parsed.relayRunning) };
  }
  return probe;
}

function appProbe(service) {
  const { pids, error } = appProvider.appPids(service);
  return {
    running: error ? false : pids.length > 0,
    unitState: error ? 'unknown' : pids.length > 0 ? 'running' : 'stopped',
    pids,
    error,
  };
}

function commandProbe(service) {
  if (!service.process.running) {
    return { running: true, unitState: 'unknown', pids: [], error: null };
  }
  const result = runArgv(service.process.running, { timeoutMs: 15000 });
  if (result.timedOut || result.code === null) {
    return { running: false, unitState: 'unknown', pids: [], error: describeFailure(result) };
  }
  return { running: result.ok, unitState: result.ok ? 'running' : 'stopped', pids: [], error: null };
}

/**
 * Is the service's process there. `includeRelay` is only honoured by the Windows
 * task probe, and only because each PowerShell start costs about half a second
 * and `status` has a budget: asking about the process and the relay in one
 * script saves a whole invocation.
 */
export function probeProcess(service, { includeRelay = false } = {}) {
  switch (service.process?.type) {
    case 'systemd-user':
    case 'systemd-system':
      return systemdProbe(service);
    case 'scheduled-task':
      return windowsTaskProbe(service, { includeRelay });
    case 'app':
      return appProbe(service);
    case 'command':
      return commandProbe(service);
    default:
      // Nothing to start or stop, so there is nothing that can be down either.
      return { running: true, unitState: 'n/a', pids: [], error: null };
  }
}

/**
 * Just "is it up", and it FAILS CLOSED. The stop loop uses this to decide when
 * it is safe to let an installer rewrite files underneath the service, so a
 * probe we could not complete has to read as "still running": timing out costs
 * one deferred update, guessing "stopped" corrupts a live service.
 */
export function isRunning(service) {
  const type = service.process?.type;
  if (type === 'systemd-user' || type === 'systemd-system') {
    const active = systemctl(service, 'is-active', 10000);
    const state = active.stdout.trim();
    if (!state) return true;
    return state !== 'inactive' && state !== 'failed' && state !== 'unknown';
  }
  // isAppRunning already fails closed on a probe it could not complete.
  if (type === 'app') return appProvider.isAppRunning(service);
  const probe = probeProcess(service);
  if (probe.error) return true;
  return probe.running;
}

export function startProcess(service) {
  const type = service.process?.type;
  if (type === 'systemd-user' || type === 'systemd-system') {
    const result = systemctl(service, 'start', 60000);
    return { ok: result.ok, message: result.ok ? null : describeFailure(result) };
  }
  if (type === 'scheduled-task') {
    const result = runPowerShell(`Start-ScheduledTask -TaskName ${psLiteral(service.process.task)}`, 60000);
    return { ok: result.ok, message: result.ok ? null : describeFailure(result) };
  }
  if (type === 'app') return appProvider.launchApp(service);
  if (type === 'command') {
    if (!service.process.start) return { ok: false, message: `${service.name} has no start command configured` };
    const result = runArgv(service.process.start, { timeoutMs: 60000 });
    return { ok: result.ok, message: result.ok ? null : describeFailure(result) };
  }
  return { ok: true, message: `${service.name} has nothing to start` };
}

export function stopProcess(service) {
  const type = service.process?.type;
  if (type === 'systemd-user' || type === 'systemd-system') {
    const result = systemctl(service, 'stop', 60000);
    return { ok: result.ok, message: result.ok ? null : describeFailure(result) };
  }
  if (type === 'scheduled-task') {
    const result = runPowerShell(`Stop-ScheduledTask -TaskName ${psLiteral(service.process.task)}`, 60000);
    return { ok: result.ok, message: result.ok ? null : describeFailure(result) };
  }
  // Asking is all we ever do to an app. There is no forced stop here.
  if (type === 'app') return appProvider.askAppToQuit(service);
  if (type === 'command') {
    if (!service.process.stop) return { ok: false, message: `${service.name} has no stop command configured` };
    const result = runArgv(service.process.stop, { timeoutMs: 60000 });
    return { ok: result.ok, message: result.ok ? null : describeFailure(result) };
  }
  return { ok: true, message: `${service.name} has nothing to stop` };
}

/** Whether this platform can act on the service's process at all. */
export function processIsActionable(service) {
  const type = service.process?.type;
  if (!type || type === 'none') return false;
  const platform = detectPlatform();
  if (type === 'app') return platform === 'mac';
  if (type === 'scheduled-task') return platform === 'windows';
  if (type === 'systemd-user' || type === 'systemd-system') return platform === 'linux';
  return true;
}
