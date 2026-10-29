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

import { deadline, describeFailure, detectPlatform, runArgv, runCommand, runCommandAsync } from '../config.mjs';
import { runMutation, runMutationArgv } from '../process-runner.mjs';
import * as appProvider from '../providers/app.mjs';
import { packageRoot } from '../providers/npm.mjs';

/** Single-quoted PowerShell literal; a literal quote is doubled. */
function psLiteral(value) {
  return `'${String(value).replace(/'/g, "''")}'`;
}

function powerShellArgs(script) {
  return ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-Command', script];
}

export function runPowerShell(script, timeoutMs = 30000) {
  return runCommand('powershell.exe', powerShellArgs(script), { timeoutMs });
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
export function taskMatch(service, { readOnly = false } = {}) {
  if (service.process?.match) return service.process.match;
  if (service.kind === 'npm') return packageRoot(service, { allowProbe: false, readOnly });
  return service.name;
}

/** The systemctl argv for one verb on the service's unit. */
function systemctlArgv(service, verb) {
  const unit = service.process.unit;
  // Non-interactive sudo on purpose: an ssh session has nowhere to type a
  // password and a prompt would hang here until the timeout.
  if (service.process.type === 'systemd-system') return ['sudo', '-n', 'systemctl', verb, unit];
  return ['systemctl', '--user', verb, unit];
}

function systemctl(service, verb, timeoutMs) {
  return runArgv(systemctlArgv(service, verb), { timeoutMs });
}

/** A systemctl verb that changes the unit, through the mutation runner. */
export function systemctlMutation(service, verb, timeoutMs) {
  return runMutationArgv(systemctlArgv(service, verb), { timeoutMs });
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

/**
 * When the service's process started, as an ISO timestamp, or null when this
 * platform cannot say.
 *
 * This is one of the two pieces of evidence that can retire a state-database row
 * still marked "running": a turn cannot survive the restart of the server that
 * was executing it, so a process that started after the turn began proves the
 * turn is a leftover. Null is not a guess — it means no evidence, and the row
 * keeps its protection.
 */
export function processStartedAt(service) {
  const type = service.process?.type;
  try {
    if (type === 'systemd-user' || type === 'systemd-system') {
      const unit = service.process.unit;
      const args = ['show', unit, '--property=ActiveEnterTimestamp', '--value'];
      const result =
        type === 'systemd-system'
          ? runCommand('systemctl', args, { timeoutMs: 10000 })
          : runCommand('systemctl', ['--user', ...args], { timeoutMs: 10000 });
      const text = result.stdout.trim();
      if (!result.ok || !text) return null;
      // systemd prints "Thu 2026-09-05 10:12:33 CEST"; Date.parse copes once the
      // leading weekday is dropped. An unparseable answer is no evidence.
      const parsed = Date.parse(text.replace(/^[A-Za-z]{3}\s+/, ''));
      return Number.isNaN(parsed) ? null : new Date(parsed).toISOString();
    }

    if (type === 'scheduled-task') {
      // A failed query says nothing, which is why it stops the script rather
      // than leaving an empty answer that reads like a process with no start.
      const script = [
        "$ErrorActionPreference = 'Stop'",
        `$needle = ${psLiteral(taskMatch(service))}`,
        "$procs = @(Get-CimInstance -ClassName Win32_Process | Where-Object { $_.ProcessId -ne $PID -and $_.CommandLine -and " +
          "$_.CommandLine.ToLower().Replace('\\\\', '\\').Contains($needle.ToLower().Replace('\\\\', '\\')) })",
        "if ($procs.Count -gt 0) { ($procs | Sort-Object CreationDate | Select-Object -First 1).CreationDate.ToUniversalTime().ToString('o') }",
      ].join('\n');
      const result = runPowerShell(script, 30000);
      const text = result.stdout.trim();
      if (!result.ok || !text) return null;
      const parsed = Date.parse(text);
      return Number.isNaN(parsed) ? null : new Date(parsed).toISOString();
    }

    if (type === 'app') {
      const { pids } = appProvider.appPids(service);
      if (pids.length === 0) return null;
      const result = runCommand('/bin/ps', ['-o', 'lstart=', '-p', String(Math.min(...pids))], { timeoutMs: 10000 });
      const text = result.stdout.trim();
      if (!result.ok || !text) return null;
      const parsed = Date.parse(text);
      return Number.isNaN(parsed) ? null : new Date(parsed).toISOString();
    }
  } catch {
    /* no evidence is a perfectly good answer here */
  }
  // A command-described service has no general way to say when it started, and
  // inventing one would be worse than admitting it.
  return null;
}

/**
 * The one PowerShell script that looks for a scheduled task's process, for
 * status and for everything that decides whether a service may be stopped.
 *
 * It stops at the first error. It used to carry on silently, so a process query
 * that was denied left an empty list behind it and the script reported, quite
 * correctly for that list, that nothing was running.
 */
function windowsTaskScript(service, { includeRelay, readOnly = false, startedAt = false }) {
  const needle = psLiteral(taskMatch(service, { readOnly }));
  const port = service.health?.type === 'http' ? service.health.port : null;
  const portFlag = port ? psLiteral(`--port ${port}`) : null;
  const lines = [
    "$ErrorActionPreference = 'Stop'",
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
  if (startedAt) {
    lines.push("$started = if ($procs.Count -gt 0) { ($procs | Sort-Object CreationDate | Select-Object -First 1).CreationDate.ToUniversalTime().ToString('o') } else { $null }");
  }
  lines.push(
    `[pscustomobject]@{ ${startedAt ? 'startedAt = $started; ' : ''}running = ($procs.Count -gt 0); pids = (($procs | ForEach-Object { [string]$_.ProcessId }) -join ',')` +
      (includeRelay
        ? '; relayConfigured = (($relayCommand -ne $null) -or ($relay.Count -gt 0)); relayRunning = ($relay.Count -gt 0)'
        : '') +
      ' } | ConvertTo-Json -Compress',
  );
  return lines.join('\n');
}

/** An ISO timestamp from a probe's text, or null when the text is not one. */
function timestamp(text) {
  const value = Date.parse(String(text ?? '').trim().replace(/^[A-Za-z]{3}\s+/, ''));
  return Number.isNaN(value) ? null : new Date(value).toISOString();
}

/**
 * What the scheduled-task script answered, read the same way by `status` and by
 * the stop loop, the busy gate and recovery, which all act on it.
 *
 * Only a script that succeeded and printed a typed Boolean is a reading. The
 * synchronous probe used to parse whatever was printed, so a PowerShell that
 * exited 1 after an access-denied query, having printed a perfectly
 * well-formed "running": false, made the service read as stopped with no error
 * at all: the stop loop let an installer at its files, and the busy gate retired
 * the turns it was running. A failed, timed-out or malformed answer is unknown
 * (contract section 6), and unknown is never stopped.
 */
export function readTaskProbe(result) {
  const parsed = result.ok ? parsePowerShellJson(result.stdout) : null;
  if (!parsed || typeof parsed.running !== 'boolean') {
    const error = result.ok ? 'PowerShell printed no process reading' : describeFailure(result);
    return { probe: { running: false, unitState: 'unknown', pids: [], startedAt: null, error }, parsed: null };
  }
  const pids = String(parsed.pids ?? '')
    .split(',')
    .map((value) => Number(value.trim()))
    .filter((value) => Number.isInteger(value) && value > 0);
  return {
    probe: {
      running: parsed.running,
      unitState: parsed.running ? 'running' : 'stopped',
      pids,
      startedAt: parsed.running ? timestamp(parsed.startedAt) : null,
      error: null,
    },
    parsed,
  };
}

function windowsTaskProbe(service, { includeRelay }) {
  const reading = readTaskProbe(runPowerShell(windowsTaskScript(service, { includeRelay }), 30000));
  // The synchronous probe has never reported a start time; processStartedAt does.
  const { startedAt, ...probe } = reading.probe;
  if (includeRelay && reading.parsed) {
    probe.relay = { configured: Boolean(reading.parsed.relayConfigured), running: Boolean(reading.parsed.relayRunning) };
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

/** Read-only process and start-time evidence under the status snapshot's deadline. */
export async function probeProcessAsync(service, { clock = deadline(15000), includeRelay = false, runner = runCommandAsync } = {}) {
  const unknown = (error) => ({ running: false, unitState: 'unknown', pids: [], startedAt: null, error });
  const run = (file, args, maximum = 15000) => {
    const timeoutMs = clock.slice(maximum);
    if (timeoutMs <= 0) return Promise.resolve({ ok: false, code: null, stdout: '', stderr: '', timedOut: true, command: file });
    return runner(file, args, { timeoutMs });
  };
  const powershell = (script) => run('powershell.exe', powerShellArgs(script), 30000);
  const type = service.process?.type;
  let probe;
  if (type === 'systemd-user' || type === 'systemd-system') {
    // One response supplies both liveness and its timestamp; a second process
    // query could observe a different service instance and used to double cost.
    const args = ['show', service.process.unit, '--property=ActiveState', '--property=ActiveEnterTimestamp'];
    const result = await run('systemctl', type === 'systemd-user' ? ['--user', ...args] : args, 10000);
    const fields = Object.fromEntries(result.stdout.split('\n').filter((line) => line.includes('=')).map((line) => {
      const split = line.indexOf('=');
      return [line.slice(0, split), line.slice(split + 1)];
    }));
    probe = result.ok && fields.ActiveState
      ? { running: fields.ActiveState === 'active', unitState: fields.ActiveState, pids: [],
          startedAt: fields.ActiveState === 'active' ? timestamp(fields.ActiveEnterTimestamp) : null, error: null }
      : unknown(describeFailure(result));
  } else if (type === 'scheduled-task') {
    const reading = readTaskProbe(await powershell(windowsTaskScript(service, { includeRelay, readOnly: true, startedAt: true })));
    probe = reading.probe;
    if (includeRelay && reading.parsed) probe.relay = { configured: true, running: reading.parsed.relayRunning === true };
  } else if (type === 'app') {
    if (!service.path) return unknown('no app path is configured for this service');
    const result = await run('/bin/ps', ['-Ao', 'pid=,comm=']);
    if (!result.ok) probe = unknown(describeFailure(result));
    else {
      const pids = result.stdout.split('\n').flatMap((line) => {
        const match = /^\s*(\d+)\s+(.*)$/.exec(line);
        return match && Number(match[1]) !== process.pid && match[2].startsWith(`${service.path}/Contents/`) ? [Number(match[1])] : [];
      });
      probe = { running: pids.length > 0, unitState: pids.length > 0 ? 'running' : 'stopped', pids, startedAt: null, error: null };
      if (pids.length && !clock.expired()) {
        const started = await run('/bin/ps', ['-o', 'lstart=', '-p', String(Math.min(...pids))], 10000);
        if (started.ok) probe.startedAt = timestamp(started.stdout);
      }
    }
  } else if (type === 'command' && service.process.running) {
    const result = await run(service.process.running[0], service.process.running.slice(1));
    probe = result.timedOut || result.code === null
      ? unknown(describeFailure(result))
      : { running: result.ok, unitState: result.ok ? 'running' : 'stopped', pids: [], startedAt: null, error: null };
  } else {
    probe = { running: true, unitState: type === 'command' ? 'unknown' : 'n/a', pids: [], startedAt: null, error: null };
  }
  if (includeRelay && !probe.relay) {
    // A configured relay stays configured when it cannot be measured.
    let result;
    if (detectPlatform() === 'windows') {
      result = await powershell([
        "$ErrorActionPreference = 'Stop'",
        "$relay = @(Get-CimInstance -ClassName Win32_Process -Filter 'Name = ''cloudflared.exe''' | Where-Object { $_.CommandLine -and $_.CommandLine.Contains('tunnel') -and $_.CommandLine.Contains('run') })",
        "if ($relay.Count -gt 0) { exit 0 } else { exit 1 }",
      ].join('\n'));
    } else {
      result = await run('pgrep', ['-f', 'cloudflared.*tunnel[[:space:]]+run'], 5000);
    }
    probe.relay = { configured: true, running: result.ok };
    if (result.timedOut || result.code === null || (result.code !== 0 && result.code !== 1)) {
      probe.error = [probe.error, `relay: ${describeFailure(result)}`].filter(Boolean).join('; ');
    }
  }
  return probe;
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

/**
 * Start and stop go through the mutation runner, which answers only once the
 * command has ended or its whole tree has been stopped. A start command is done
 * when it exits, and the daemon it leaves running is the whole point of it, so
 * it is never stopped for outliving the command that started it.
 */
export async function startProcess(service) {
  const type = service.process?.type;
  if (type === 'systemd-user' || type === 'systemd-system') {
    const result = await systemctlMutation(service, 'start', 60000);
    return { ok: result.ok, message: result.ok ? null : describeFailure(result) };
  }
  if (type === 'scheduled-task') {
    const result = await runMutation('powershell.exe', powerShellArgs(`Start-ScheduledTask -TaskName ${psLiteral(service.process.task)}`), { timeoutMs: 60000 });
    return { ok: result.ok, message: result.ok ? null : describeFailure(result) };
  }
  if (type === 'app') return appProvider.launchApp(service);
  if (type === 'command') {
    if (!service.process.start) return { ok: false, message: `${service.name} has no start command configured` };
    const result = await runMutationArgv(service.process.start, { timeoutMs: 60000 });
    return { ok: result.ok, message: result.ok ? null : describeFailure(result) };
  }
  return { ok: true, message: `${service.name} has nothing to start` };
}

export async function stopProcess(service) {
  const type = service.process?.type;
  if (type === 'systemd-user' || type === 'systemd-system') {
    const result = await systemctlMutation(service, 'stop', 60000);
    return { ok: result.ok, message: result.ok ? null : describeFailure(result) };
  }
  if (type === 'scheduled-task') {
    const result = await runMutation('powershell.exe', powerShellArgs(`Stop-ScheduledTask -TaskName ${psLiteral(service.process.task)}`), { timeoutMs: 60000 });
    return { ok: result.ok, message: result.ok ? null : describeFailure(result) };
  }
  // Asking is all we ever do to an app. There is no forced stop here.
  if (type === 'app') return appProvider.askAppToQuit(service);
  if (type === 'command') {
    if (!service.process.stop) return { ok: false, message: `${service.name} has no stop command configured` };
    const result = await runMutationArgv(service.process.stop, { timeoutMs: 60000 });
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
