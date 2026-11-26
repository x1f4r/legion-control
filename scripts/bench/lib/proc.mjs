// Process inspection, load checks, throwaway Legion node setup, and server/sshd lifecycles.

import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';

const CLK_TCK = (() => {
  try {
    const out = execFileSync('getconf', ['CLK_TCK'], { encoding: 'utf8' }).trim();
    const n = Number(out);
    if (Number.isFinite(n) && n > 0) return n;
  } catch {
    // Fallback to standard Linux USER_HZ.
  }
  return 100;
})();

export function readUptime() {
  let raw = '';
  try {
    raw = execFileSync('uptime', [], { encoding: 'utf8' }).trim();
  } catch {
    raw = `loadavg ${os.loadavg().map((n) => n.toFixed(2)).join(', ')}`;
  }
  const [load1, load5, load15] = os.loadavg();
  return { raw, load1, load5, load15, ts: new Date().toISOString() };
}

export async function waitForQuietLoad(maxLoad, maxWaitMs = 90_000) {
  const deadline = Date.now() + maxWaitMs;
  let snap = readUptime();
  let waited = false;
  while (snap.load1 > maxLoad && Date.now() < deadline) {
    if (!waited) {
      console.error(
        `[bench] 1-min load ${snap.load1.toFixed(2)} > ${maxLoad.toFixed(1)}, waiting for quieter machine...`,
      );
      waited = true;
    }
    await new Promise((r) => setTimeout(r, 2000));
    snap = readUptime();
  }
  return snap;
}

export function collectMachineInfo(repoRoot) {
  const cpus = os.cpus();
  const cpuModel = cpus[0]?.model?.trim() || 'unknown';
  let physicalCores = null;
  let kernel = os.release();
  try {
    const lscpu = execFileSync('lscpu', [], { encoding: 'utf8' });
    const c = lscpu.match(/Core\(s\) per socket:\s+(\d+)/);
    const s = lscpu.match(/Socket\(s\):\s+(\d+)/);
    if (c && s) physicalCores = Number(c[1]) * Number(s[1]);
  } catch {
    // Optional on non-util-linux hosts.
  }
  let gitRev = 'unknown';
  let gitBranch = 'unknown';
  let gitDirty = false;
  try {
    gitRev = execFileSync('git', ['rev-parse', '--short=8', 'HEAD'], {
      cwd: repoRoot,
      encoding: 'utf8',
    }).trim();
    gitBranch = execFileSync('git', ['rev-parse', '--abbrev-ref', 'HEAD'], {
      cwd: repoRoot,
      encoding: 'utf8',
    }).trim();
    const st = execFileSync('git', ['status', '--porcelain'], {
      cwd: repoRoot,
      encoding: 'utf8',
    }).trim();
    gitDirty = st.length > 0;
  } catch {
    // Ignore outside git.
  }
  return {
    os: `${os.platform()} ${os.arch()}`,
    kernel,
    cpuModel,
    logicalCpus: cpus.length,
    physicalCores,
    totalRamGiB: Number((os.totalmem() / (1024 ** 3)).toFixed(1)),
    nodeVersion: process.version,
    gitRev,
    gitBranch,
    gitDirty,
    timestamp: new Date().toISOString(),
  };
}

/**
 * Reads `/proc/<pid>/status` and `/proc/<pid>/stat` on Linux.
 * Returns `{ vmHwmMiB, vmRssMiB, threads, cpuSecs }` or nulls if process already exited.
 */
export function readProcMetrics(pid) {
  if (!pid || process.platform !== 'linux') {
    return { vmHwmMiB: null, vmRssMiB: null, threads: null, cpuSecs: null };
  }
  let vmHwmMiB = null;
  let vmRssMiB = null;
  let threads = null;
  let cpuSecs = null;
  try {
    const status = fs.readFileSync(`/proc/${pid}/status`, 'utf8');
    const hwm = status.match(/^VmHWM:\s+(\d+)\s+kB/m);
    const rss = status.match(/^VmRSS:\s+(\d+)\s+kB/m);
    const thr = status.match(/^Threads:\s+(\d+)/m);
    if (hwm) vmHwmMiB = Number(hwm[1]) / 1024;
    if (rss) vmRssMiB = Number(rss[1]) / 1024;
    if (thr) threads = Number(thr[1]);
  } catch {
    // Process may have exited.
  }
  try {
    const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
    const closeParen = stat.lastIndexOf(')');
    if (closeParen !== -1) {
      const fields = stat.slice(closeParen + 2).trim().split(/\s+/);
      // After "(comm)", field 0 is state (3rd overall), utime is 11 (14th), stime is 12 (15th).
      const utime = Number(fields[11] || 0);
      const stime = Number(fields[12] || 0);
      cpuSecs = (utime + stime) / CLK_TCK;
    }
  } catch {
    // Process may have exited.
  }
  return { vmHwmMiB, vmRssMiB, threads, cpuSecs };
}

export function cleanLegionEnv(legionHome, userHome, extra = {}) {
  const env = {
    ...process.env,
    LEGION_HOME: legionHome,
    HOME: userHome,
    USERPROFILE: userHome,
  };
  delete env.XDG_CONFIG_HOME;
  delete env.XDG_STATE_HOME;
  delete env.SSH_ORIGINAL_COMMAND;
  return { ...env, ...extra };
}

export function runLegionSync(bin, args, legionHome, userHome, extraEnv = {}) {
  return execFileSync(bin, args, {
    env: cleanLegionEnv(legionHome, userHome, extraEnv),
    encoding: 'utf8',
  });
}

export function writeThrowawayNode({ legionHome, userHome, name, fleetToml = '', extraPolicy = '' }) {
  fs.mkdirSync(path.join(legionHome, 'config'), { recursive: true });
  fs.mkdirSync(path.join(legionHome, 'state'), { recursive: true });
  fs.mkdirSync(userHome, { recursive: true });

  const system = { linux: 'linux', darwin: 'macos', win32: 'windows' }[process.platform] || 'linux';
  fs.writeFileSync(
    path.join(legionHome, 'config', 'node.toml'),
    `name = ${JSON.stringify(name)}\nsystem = ${JSON.stringify(system)}\nbusy = "none"\n`,
  );

  const policy = [
    'default = "owner"',
    '',
    '[principals.operator]',
    'role = "operator"',
    '',
    '[roles.operator]',
    'allow = ["effect:read", "effect:write", "effect:exec", "effect:destroy", "effect:power"]',
    'deny = ["effect:admin"]',
    '',
    extraPolicy,
  ].join('\n');
  fs.writeFileSync(path.join(legionHome, 'config', 'policy.toml'), policy);

  if (fleetToml) {
    fs.writeFileSync(path.join(legionHome, 'config', 'fleet.toml'), fleetToml);
  }
}

export function envCommandArgv(bin, legionHome, userHome) {
  return [
    '/usr/bin/env',
    '-u',
    'XDG_CONFIG_HOME',
    '-u',
    'XDG_STATE_HOME',
    `LEGION_HOME=${legionHome}`,
    `HOME=${userHome}`,
    `USERPROFILE=${userHome}`,
    bin,
    'rpc',
  ];
}

export async function startLegionServe(bin, legionHome, userHome, options = {}) {
  const { listen = '127.0.0.1:0', extraEnv = {} } = options;
  const child = spawn(bin, ['serve', '--listen', listen], {
    env: cleanLegionEnv(legionHome, userHome, extraEnv),
    stdio: ['ignore', 'ignore', 'pipe'],
  });

  let stderr = '';
  const url = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new Error(`legion serve did not announce its port. stderr:\n${stderr}`));
    }, 15_000);
    const rl = readline.createInterface({ input: child.stderr });
    rl.on('line', (line) => {
      stderr += `${line}\n`;
      const prefix = 'legion serve: listening on ';
      if (line.startsWith(prefix)) {
        clearTimeout(timer);
        resolve(line.slice(prefix.length).trim());
      }
    });
    child.once('exit', (code) => {
      clearTimeout(timer);
      reject(new Error(`legion serve exited early (${code}). stderr:\n${stderr}`));
    });
  });

  async function stop() {
    if (child.exitCode !== null) return;
    child.kill('SIGTERM');
    await new Promise((resolve) => {
      const t = setTimeout(() => {
        try {
          child.kill('SIGKILL');
        } catch {
          // Already exited.
        }
        resolve();
      }, 6000);
      child.once('exit', () => {
        clearTimeout(t);
        resolve();
      });
    });
  }

  return { pid: child.pid, url, stop, getStderr: () => stderr };
}

export async function freeLoopbackPort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.unref();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

async function waitTcpOpen(host, port, timeoutMs = 8000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      await new Promise((resolve, reject) => {
        const s = net.connect({ host, port }, () => {
          s.end();
          resolve();
        });
        s.setTimeout(500);
        s.on('timeout', () => {
          s.destroy();
          reject(new Error('timeout'));
        });
        s.on('error', (err) => {
          s.destroy();
          reject(err);
        });
      });
      return;
    } catch {
      await new Promise((r) => setTimeout(r, 50));
    }
  }
  throw new Error(`Timed out waiting for ${host}:${port}`);
}

/**
 * Starts a throwaway unprivileged loopback `sshd` forcing `legion gate --as operator --home <home>`.
 * Touches nothing under `~/.ssh`.
 */
export async function startLoopbackSshd({ workDir, bin, legionHome }) {
  const sshdBin = fs.existsSync('/usr/bin/sshd') ? '/usr/bin/sshd' : '/usr/sbin/sshd';
  if (!fs.existsSync(sshdBin)) {
    throw new Error('sshd binary not found');
  }
  fs.mkdirSync(workDir, { recursive: true, mode: 0o700 });
  const hostKey = path.join(workDir, 'host_ed25519');
  const clientKey = path.join(workDir, 'client_ed25519');
  if (!fs.existsSync(hostKey)) {
    execFileSync('ssh-keygen', ['-q', '-t', 'ed25519', '-N', '', '-f', hostKey]);
    fs.chmodSync(hostKey, 0o600);
  }
  if (!fs.existsSync(clientKey)) {
    execFileSync('ssh-keygen', ['-q', '-t', 'ed25519', '-N', '', '-f', clientKey]);
    fs.chmodSync(clientKey, 0o600);
  }
  const clientPub = fs.readFileSync(`${clientKey}.pub`, 'utf8').trim();
  const authKeys = path.join(workDir, 'authorized_keys');
  fs.writeFileSync(
    authKeys,
    `command="${bin} gate --as operator --home ${legionHome}",restrict ${clientPub}\n`,
    { mode: 0o600 },
  );

  const port = await freeLoopbackPort();
  const user = os.userInfo().username;
  const pidFile = path.join(workDir, 'sshd.pid');
  const logFile = path.join(workDir, 'sshd.log');
  const cfgFile = path.join(workDir, 'sshd_config');

  fs.writeFileSync(
    cfgFile,
    [
      'ListenAddress 127.0.0.1',
      `Port ${port}`,
      `HostKey ${hostKey}`,
      `PidFile ${pidFile}`,
      `AuthorizedKeysFile ${authKeys}`,
      `AllowUsers ${user}`,
      'PubkeyAuthentication yes',
      'PasswordAuthentication no',
      'KbdInteractiveAuthentication no',
      'PermitRootLogin no',
      'UsePAM no',
      'StrictModes no',
      'LogLevel ERROR',
      '',
    ].join('\n'),
    { mode: 0o600 },
  );

  const child = spawn(sshdBin, ['-D', '-e', '-f', cfgFile], {
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let stderr = '';
  child.stderr.on('data', (chunk) => {
    stderr += chunk.toString('utf8');
  });

  try {
    await waitTcpOpen('127.0.0.1', port, 8000);
  } catch (err) {
    child.kill('SIGKILL');
    throw new Error(`sshd failed to listen on 127.0.0.1:${port}: ${err.message}\n${stderr}`);
  }

  async function stop() {
    if (child.exitCode !== null) return;
    child.kill('SIGTERM');
    await new Promise((resolve) => {
      const t = setTimeout(() => {
        try {
          child.kill('SIGKILL');
        } catch {
          // Ignore.
        }
        resolve();
      }, 4000);
      child.once('exit', () => {
        clearTimeout(t);
        resolve();
      });
    });
  }

  return {
    port,
    user,
    clientKey,
    stop,
    logFile,
  };
}
