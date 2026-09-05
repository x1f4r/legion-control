// Detached work must survive the SSH session and keep the same private state directory.
import { spawn, spawnSync } from 'node:child_process';

const psString = (value) => `'${String(value).replace(/'/g, "''")}'`;

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
  return new Promise((resolve) => {
    const child = spawn(node, [entry, 'op-run', id], { detached: true, stdio: 'ignore', env: { ...environment, LEGIONCTL_HOME: home } });
    child.once('error', (error) => resolve({ ok: false, error: error.message }));
    child.once('spawn', () => { child.unref(); resolve({ ok: true, pid: child.pid }); });
  });
}
