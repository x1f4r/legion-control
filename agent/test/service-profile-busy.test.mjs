// The profile busy probe: one consistent process snapshot, with the agent's
// own probes left out of it.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { checkServiceBusy } from '../src/probes/busy.mjs';
import { inspectProfileProcesses } from '../src/service-profile-probe.mjs';

const posix = process.platform !== 'win32';
const AGENT = 4100;
const IDLE = { busy: false, unknown: false, reason: 'No related application or CLI session is running.' };
const BUSY = { busy: true, unknown: false, reason: 'A related application or CLI session is still running.' };
const UNKNOWN = { busy: true, unknown: true, reason: 'Some process identities could not be inspected; waiting for a reliable idle result.' };

function withRoot(body) {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'legion profile busy ')));
  try { return body(root); } finally { fs.rmSync(root, { recursive: true, force: true }); }
}

function write(file, text = '', mode = 0o644) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, text, { mode });
  return file;
}

function link(target, file) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.symlinkSync(target, file);
  return file;
}

/** The file system as seen from inside `root` only, so the host's own applications never take part. */
function sandbox(root, overrides = {}) {
  const missing = () => Object.assign(new Error('missing'), { code: 'ENOENT' });
  const allowed = (file) => String(file) === root || String(file).startsWith(`${root}${path.sep}`);
  return {
    realpathSync: (file) => { if (!allowed(file)) throw missing(); return fs.realpathSync(file); },
    statSync: (file) => { if (!allowed(file)) throw missing(); return fs.statSync(file); },
    readFileSync: (file, encoding) => { if (!allowed(file)) throw missing(); return fs.readFileSync(file, encoding); },
    ...overrides,
  };
}

/**
 * A /proc for the probe to read. Each process gives its stat fields and argv;
 * `gone` makes it exit at that read, `argv: null` hides it as another user's.
 */
function procFs(root, processes) {
  const error = (code) => Object.assign(new Error(code), { code });
  const files = sandbox(root);
  const entry = (file) => {
    const found = /^\/proc\/(\d+)\/(stat|cmdline|exe|cwd)$/.exec(String(file));
    return found ? { process: processes[found[1]], pid: found[1], part: found[2] } : null;
  };
  const read = (file) => {
    const { process: proc, pid, part } = entry(file) ?? {};
    if (!proc || proc.gone === part) throw error('ENOENT');
    if (part === 'stat') {
      // `states` is what each read of stat in turn shows, over again for every snapshot.
      const state = proc.states ? proc.states[(proc.reads = (proc.reads ?? -1) + 1) % proc.states.length] : proc.state ?? 'S';
      return `${pid} (${proc.name}) ${state} ${proc.ppid ?? 1} ${pid} ${pid} 0 -1 ${proc.flags ?? 4194560} 0 0 0 0 1 2 0 0 20 0 1 0 100 1000 10`;
    }
    if (proc.argv === null) throw error('EACCES');
    return Buffer.from(proc.argv.length ? `${proc.argv.join('\0')}\0` : '');
  };
  return {
    ...files,
    readdirSync: (directory) => { assert.equal(directory, '/proc'); return ['self', 'meminfo', ...Object.keys(processes)]; },
    readFileSync: (file, encoding) => (entry(file) ? read(file) : files.readFileSync(file, encoding)),
    readlinkSync: (file) => {
      const { process: proc, part } = entry(file) ?? {};
      const value = part === 'exe' ? proc?.exe : proc?.cwd;
      if (!value) throw error('EACCES');
      return value;
    },
  };
}

/** A Mac's ps: `comm` and `args` are what the first and second full reads print; `again` what a later -p read does. */
function macPs(processes) {
  const calls = [];
  const run = (file, args) => {
    assert.equal(file, '/bin/ps');
    assert.equal(args[0], '-ww');
    const selected = args[1] === '-p' ? args[2].split(',').map(Number) : null;
    const names = args.includes('comm=');
    const pass = selected ? 'again' : names ? 'comm' : 'args';
    calls.push(pass);
    const lines = [];
    for (const [pid, proc] of Object.entries(processes)) {
      if (selected && !selected.includes(Number(pid))) continue;
      const seen = selected ? proc.again ?? proc : proc;
      if (names && seen.comm !== undefined) lines.push(`${pid.padStart(5)} ${String(seen.ppid ?? 1).padStart(5)} ${(seen.stat ?? 'S').padEnd(4)} ${seen.comm}`);
      if (!names && seen.args !== undefined) lines.push(`${pid.padStart(5)} ${seen.args}`);
    }
    if (selected && lines.length === 0) return { status: 1, stdout: '' };
    return { status: 0, stdout: `${lines.join('\n')}\n`, pid: 99999 };
  };
  return { run, calls };
}

test('Linux is read from /proc as one snapshot, and what the agent itself started is left out', { skip: !posix && 'POSIX paths and links' }, () => withRoot((root) => {
  const npx = path.join(root, 'home', '.npm', '_npx', '0123456789abcdef', 'node_modules');
  write(path.join(npx, '@playwright', 'mcp', 'cli.js'));
  link('../@playwright/mcp/cli.js', path.join(npx, '.bin', 'playwright-mcp'));
  const workdir = path.dirname(write(path.join(root, 'work', 'server', 'main')));
  const processes = {
    1: { name: 'systemd', ppid: 0, argv: ['/sbin/init', 'splash'], exe: '/usr/lib/systemd/systemd' },
    2: { name: 'kthreadd', ppid: 0, flags: 0x00208040, argv: [] },
    40: { name: 'kworker/0:1-events', ppid: 2, flags: 0x04208060, argv: [] },
    // The agent that asked, its probes and their children.
    [AGENT]: { name: 'node-MainThread', argv: ['/usr/bin/node', '/home/me/.legion-control/agent/src/index.mjs', 'update', 'opencode'], exe: '/usr/bin/node' },
    [process.pid]: { name: 'node-MainThread', ppid: AGENT, argv: ['/usr/bin/node', '/home/me/.legion-control/agent/src/service-profile-probe.mjs', 'busy', 'opencode'] },
    4111: { name: 'node-MainThread', ppid: AGENT, argv: ['/usr/bin/node', '/home/me/.legion-control/probes/t3-busy.mjs', '/home/me/.t3'] },
    4112: { name: 'ps', ppid: 4111, argv: [] },
    4113: { name: 'pgrep', ppid: AGENT, argv: ['pgrep', '-f', 'cloudflared tunnel run'] },
    4114: { name: 'node-MainThread', ppid: AGENT, state: 'Z', argv: [] },
    4115: { name: 'opencode', ppid: AGENT, argv: ['/home/me/.npm-global/bin/opencode', '--version'] },
    // Another client's status call, running at the same time.
    5200: { name: 'node-MainThread', ppid: 5199, argv: ['/usr/bin/node', '/home/me/.legion-control/bin/launcher.mjs', 'status'] },
    5201: { name: 'node-MainThread', ppid: 5200, state: 'Z', argv: [] },
    // Gone between the directory listing and its own files.
    5300: { name: 'sys-telemetry', gone: 'stat' },
    5301: { name: 'nvidia-smi', gone: 'cmdline' },
    // Between exit and zombie when first read, a zombie when read again.
    5302: { name: 'tr', states: ['R', 'Z'], argv: [] },
    6400: { name: 'node-MainThread', ppid: 6300, argv: ['node', path.join(npx, '.bin', 'playwright-mcp')] },
    6401: { name: 'node-MainThread', ppid: 1, argv: ['/usr/bin/node', '-e', 'setInterval(() => {}, 1000)'] },
    6402: { name: 'node-MainThread', ppid: 6300, argv: ['node', 'server/main', '--stdio'], cwd: path.dirname(workdir) },
    6403: { name: 'node-MainThread', ppid: 6300, argv: ['/usr/bin/node', '--max-old-space-size=4096', '/opt/tools/tsserver.js'] },
    6404: { name: 'bash', ppid: 6300, argv: ['/bin/bash', '-c', 'node /home/me/My Scripts/run.js'] },
  };
  const io = procFs(root, processes);
  const asked = { platform: 'linux', io, home: path.join(root, 'home'), env: { LEGIONCTL_AGENT_PID: String(AGENT) }, parentPid: AGENT };
  assert.deepEqual(inspectProfileProcesses('opencode', asked), IDLE);
  // A version probe the agent itself runs is the agent's, not a session.
  assert.deepEqual(inspectProfileProcesses('opencode', { ...asked, env: {} }), BUSY);
  assert.deepEqual(inspectProfileProcesses('opencode', { ...asked, parentPid: 1 }), BUSY, 'a pid the probe was not started by is not trusted');
  // A product session outside the agent is found as before.
  processes[7000] = { name: 'opencode', ppid: 1, argv: ['/home/me/.npm-global/bin/opencode', 'serve'] };
  assert.deepEqual(inspectProfileProcesses('opencode', asked), BUSY);
  delete processes[7000];
  // A process whose argv cannot be read, and one with none that is still
  // there when read again, stay uncertain.
  processes[7001] = { name: 'node', ppid: 1, argv: null };
  assert.deepEqual(inspectProfileProcesses('opencode', asked), UNKNOWN);
  delete processes[7001];
  processes[7002] = { name: 'node', ppid: 1, states: ['R', 'R'], argv: [] };
  assert.deepEqual(inspectProfileProcesses('opencode', asked), UNKNOWN);
}));

test('an unreadable /proc fails closed without disclosing anything', () => {
  const io = { readdirSync: () => { throw Object.assign(new Error('private-value'), { code: 'EACCES' }); } };
  const result = inspectProfileProcesses('codex-cli', { platform: 'linux', io, env: {} });
  assert.deepEqual(result, { busy: true, unknown: true, reason: 'Process inspection is unavailable for this profile.' });
  assert.doesNotMatch(JSON.stringify(result), /private-value/);
});

test('a Mac re-reads the processes its two ps reads saw only once, and splits argv0 by its full path', { skip: !posix && 'POSIX paths and links' }, () => withRoot((root) => {
  const npx = path.join(root, 'home', '.npm', '_npx', '0123456789abcdef', 'node_modules');
  write(path.join(npx, 'xcodebuildmcp', 'build', 'cli.js'));
  link('../xcodebuildmcp/build/cli.js', path.join(npx, '.bin', 'xcodebuildmcp'));
  const extension = write(path.join(root, 'home', 'Library', 'Application Support', 'Claude', 'Claude Extensions', 'demo', 'server', 'index.js'));
  const crashpad = (bundle) => `${bundle}/Contents/Frameworks/Electron Framework.framework/Helpers/chrome_crashpad_handler`;
  const claudePad = crashpad('/Applications/Claude.app');
  const helperPad = crashpad('/Applications/Example.app');
  const processes = {
    1: { ppid: 0, stat: 'Ss', comm: '/sbin/launchd', args: '/sbin/launchd' },
    [AGENT]: { ppid: 4000, comm: 'node', args: 'node /Users/me/.legion-control/agent/src/index.mjs update opencode' },
    [process.pid]: { ppid: AGENT, comm: '/opt/homebrew/bin/node', args: '/opt/homebrew/bin/node /Users/me/.legion-control/agent/src/service-profile-probe.mjs busy opencode' },
    // A sibling probe that exited before the second read, and one that
    // started after the first.
    4120: { ppid: AGENT, comm: '/opt/homebrew/bin/node' },
    4121: { ppid: AGENT, args: '/opt/homebrew/bin/node /Users/me/.legion-control/agent/src/service-profile-probe.mjs busy codex-cli', again: { ppid: AGENT, comm: '/opt/homebrew/bin/node', args: '/opt/homebrew/bin/node /Users/me/.legion-control/agent/src/service-profile-probe.mjs busy codex-cli' } },
    // Not the agent's: one gone by the time it is read again, one that is
    // still there, and one that replaced its program between the reads.
    5200: { args: '/usr/bin/tr -d x', again: {} },
    5201: { args: '/bin/sleep 1', again: { comm: '/bin/sleep', args: '/bin/sleep 1' } },
    5202: { comm: 'node', args: '/bin/ps -ww -ax -o pid= -o args=', again: { comm: '/bin/ps', args: '/bin/ps -ww -ax -o pid= -o args=' } },
    1223: { comm: helperPad, args: `${helperPad} --no-rate-limit --monitor-self-annotation=ptype=crashpad-handler` },
    1224: { comm: claudePad, args: `${claudePad} --no-rate-limit --database=/Users/me/Library/Application Support/Claude/Crashpad` },
    4199: { ppid: 4011, comm: 'node', args: `node ${path.join(npx, '.bin', 'xcodebuildmcp')} mcp` },
    12335: { comm: '/opt/homebrew/Cellar/node/26.12.0/bin/node', args: '/opt/homebrew/Cellar/node/26.12.0/bin/node -e setInterval(() => {}, 1000)' },
    4200: { comm: 'node', args: `node ${extension} --stdio` },
    4201: { stat: 'Z', comm: '<defunct>', args: '<defunct>' },
    20235: { comm: 'claude', args: 'claude --output-format stream-json --verbose' },
  };
  const ps = macPs(processes);
  const asked = { platform: 'darwin', run: ps.run, io: sandbox(root), home: path.join(root, 'home'), env: { LEGIONCTL_AGENT_PID: String(AGENT) }, parentPid: AGENT };
  assert.deepEqual(inspectProfileProcesses('opencode', asked), IDLE);
  assert.deepEqual(ps.calls, ['comm', 'args', 'again', 'again'], 'only the processes seen once are read again');
  // A process still seen only once after it is read again stays uncertain.
  processes[5203] = { args: '/opt/homebrew/bin/node /Users/me/tool.js', again: { args: '/opt/homebrew/bin/node /Users/me/tool.js' } };
  assert.deepEqual(inspectProfileProcesses('opencode', asked), UNKNOWN);
}));

test('a failed Mac re-read fails closed', () => {
  const { run } = macPs({ 5200: { args: '/bin/sleep 1' } });
  const failing = (file, args) => (args[1] === '-p' ? { status: 2, stdout: 'private-value' } : run(file, args));
  assert.equal(inspectProfileProcesses('opencode', { platform: 'darwin', run: failing, io: sandbox('/nowhere'), env: {} }).unknown, true);
});

test('a busy command is told which agent asked', async () => {
  const echo = [process.execPath, '-e', 'process.stdout.write(JSON.stringify({ busy: false, reason: String(process.env.LEGIONCTL_AGENT_PID) }))'];
  const answer = await checkServiceBusy({ id: 'opencode', name: 'OpenCode', kind: 'npm', busy: { type: 'command', command: echo } });
  assert.equal(answer.reason, String(process.pid));
});
