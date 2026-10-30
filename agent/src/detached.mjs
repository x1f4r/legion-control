// Detached work must survive the SSH session and keep the same private state directory.
import { spawn, spawnSync } from 'node:child_process';
import { powerShellLiteral as psString } from './process-runner.mjs';

// On Linux a worker started from an ssh session stays in that session's scope
// however it is detached, and where logind runs with KillUserProcesses=yes it
// ends the scope, worker and all, when the session closes. A scope of the
// user's own systemd manager outlives the session; linger keeps the manager
// up, which install-linux.sh already asks for. systemd-run --scope execs the
// worker in place, so the pid it starts with is the worker's own, and the
// worker keeps the environment it was given.
const USER_SCOPE = ['--user', '--scope', '--quiet', '--collect', '--'];

/**
 * Whether a worker can start in a scope of the user's systemd manager. That
 * takes systemd-run and a manager that answers, and the surest proof of both
 * is a scope that runs `true`. When it fails, or does not answer in time, the
 * worker starts as it always has, in the session's own scope.
 */
export function userScopeAvailable({ platform = process.platform, environment = process.env, run = spawnSync } = {}) {
  if (platform !== 'linux') return false;
  const probe = run('systemd-run', [...USER_SCOPE, 'true'], { env: environment, stdio: 'ignore', timeout: 5000 });
  return !probe.error && probe.status === 0;
}

/** The program and arguments that start the worker for one operation outside Windows. */
export function posixWorkerCommand({ node, entry, id, userScope }) {
  const worker = [entry, 'op-run', id];
  return userScope ? { file: 'systemd-run', args: [...USER_SCOPE, node, ...worker] } : { file: node, args: worker };
}

export function windowsWorkerScript({ node, entry, id, home, environment = process.env }) {
  const assignments = { LEGIONCTL_HOME: home };
  for (const name of ['LEGIONCTL_CLIENT', 'LEGIONCTL_DEVICE', 'LEGIONCTL_USER', 'LEGIONCTL_RESTRICTED']) {
    if (environment[name] !== undefined) assignments[name] = environment[name];
  }
  return [
    "$ErrorActionPreference = 'Stop'",
    ...Object.entries(assignments).map(([name, value]) => `$env:${name} = ${psString(value)}`),
    `Set-Location -LiteralPath ${psString(home)}`,
    `& ${psString(node)} ${psString(entry)} 'op-run' ${psString(id)}`,
    'exit $LASTEXITCODE',
  ].join('\n');
}

export async function spawnDetached({ node = process.execPath, entry, id, home, environment = process.env }) {
  if (process.platform === 'win32') {
    // CIM starts a process outside sshd's session job. The launcher stays alive
    // until Node exits, so its PID also provides a liveness guard during handoff.
    const encoded = Buffer.from(windowsWorkerScript({ node, entry, id, home, environment }), 'utf16le').toString('base64');
    const command = `powershell.exe -NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand ${encoded}`;
    const script = [
      "$ErrorActionPreference = 'Stop'",
      `$created = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = ${psString(command)}; CurrentDirectory = ${psString(home)} }`,
      'if ($created.ReturnValue -ne 0 -or $created.ProcessId -le 0) { throw "Detached launch failed: $($created.ReturnValue)" }',
      '[Console]::Out.Write([string]$created.ProcessId)',
    ].join('\n');
    const result = spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], {
      encoding: 'utf8', windowsHide: true, timeout: 15000, stdio: ['ignore', 'pipe', 'pipe'],
    });
    const pid = Number(result.stdout?.trim());
    if (result.status !== 0 || !Number.isSafeInteger(pid) || pid <= 0) {
      // A timed-out RPC may have created the worker. Retrying synchronously could
      // duplicate a mutation, so ambiguity is distinct from a definite refusal.
      return { ok: false, uncertain: Boolean(result.error || result.signal), error: result.error?.message || result.stderr?.trim() || 'CIM did not confirm a process id' };
    }
    return { ok: true, pid };
  }
  const env = { ...environment, LEGIONCTL_HOME: home };
  const { file, args } = posixWorkerCommand({ node, entry, id, userScope: userScopeAvailable({ environment: env }) });
  return new Promise((resolve) => {
    const child = spawn(file, args, { detached: true, stdio: 'ignore', env });
    child.once('error', (error) => resolve({ ok: false, error: error.message }));
    child.once('spawn', () => { child.unref(); resolve({ ok: true, pid: child.pid }); });
  });
}
