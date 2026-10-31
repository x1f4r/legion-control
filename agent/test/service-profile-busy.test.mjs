// The profile busy probe: one consistent process snapshot, the agent's own
// probes left out of it, and an npm service guarded by the install npm replaces.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { checkServiceBusy } from '../src/probes/busy.mjs';
import { inspectProfileProcesses, processExitVerdict, profileInstall } from '../src/service-profile-probe.mjs';

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

test('Windows leaves out what the agent started only where each parent was created before its child', () => {
  const node = 'C:\\Program Files\\nodejs\\node.exe';
  const opencode = 'C:\\Users\\me\\AppData\\Roaming\\npm\\node_modules\\opencode-ai\\bin\\opencode.exe';
  const at = (time) => `2026-10-31T${time}.0000000Z`;
  const row = (pid, parent, name, command, created) => ({ ProcessId: pid, ParentProcessId: parent, SessionId: 1, Name: name, CommandLine: command, ExecutablePath: name === 'node.exe' ? node : opencode, CreatedAt: created });
  const rows = {
    agent: row(AGENT, 4000, 'node.exe', `"${node}" "C:\\Users\\me\\.legion-control\\agent\\src\\index.mjs" update opencode`, at('08:00:00')),
    probe: row(process.pid, AGENT, 'node.exe', `"${node}" "C:\\Users\\me\\.legion-control\\agent\\src\\service-profile-probe.mjs" busy opencode`, at('11:00:00.1')),
    sibling: row(4116, AGENT, 'node.exe', `"${node}" "C:\\Users\\me\\.legion-control\\agent\\src\\service-profile-probe.mjs" busy codex-cli`, at('11:00:00.2')),
    version: row(4115, AGENT, 'opencode.exe', `"${opencode}" --version`, at('11:00:00.3')),
  };
  const inspect = (entries) => inspectProfileProcesses('opencode', {
    platform: 'win32', run: () => ({ status: 0, stdout: JSON.stringify(entries) }), io: sandbox('/nowhere'), home: 'C:\\Users\\me', env: { LEGIONCTL_AGENT_PID: String(AGENT) }, parentPid: AGENT,
  });
  assert.deepEqual(inspect(Object.values(rows)), IDLE, 'a version probe the agent runs is the agent\'s, not a session');
  // A session whose parent has exited, and whose parent's id was given to the
  // agent or one of its probes after the session had started.
  assert.deepEqual(inspect([...Object.values(rows), row(7000, AGENT, 'opencode.exe', `"${opencode}" serve`, at('07:00:00'))]), BUSY);
  assert.deepEqual(inspect([...Object.values(rows), row(7001, 4116, 'opencode.exe', `"${opencode}" serve`, at('09:00:00'))]), BUSY);
  // Without creation times nothing is proven, and neither is a probe the
  // named agent cannot have started.
  assert.deepEqual(inspect([rows.agent, rows.probe, { ...rows.version, CreatedAt: null }]), BUSY);
  assert.deepEqual(inspect([rows.agent, { ...rows.probe, CreatedAt: at('07:00:00') }, rows.version]), BUSY);
});

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
  // The claude CLI is not the Claude app, and the Claude app is not the CLI.
  assert.deepEqual(inspectProfileProcesses('claude-code', asked), BUSY);
  delete processes[20235];
  assert.deepEqual(inspectProfileProcesses('claude-code', asked), IDLE);
  assert.deepEqual(inspectProfileProcesses('claude-desktop', asked), BUSY);
  // A process still seen only once after it is read again stays uncertain.
  processes[5203] = { args: '/opt/homebrew/bin/node /Users/me/tool.js', again: { args: '/opt/homebrew/bin/node /Users/me/tool.js' } };
  assert.deepEqual(inspectProfileProcesses('opencode', asked), UNKNOWN);
}));

test('a failed Mac re-read fails closed', () => {
  const { run } = macPs({ 5200: { args: '/bin/sleep 1' } });
  const failing = (file, args) => (args[1] === '-p' ? { status: 2, stdout: 'private-value' } : run(file, args));
  assert.equal(inspectProfileProcesses('opencode', { platform: 'darwin', run: failing, io: sandbox('/nowhere'), env: {} }).unknown, true);
});

test('an npm service is guarded by the processes of its own install, not of another one of the same product', { skip: !posix && 'POSIX paths and links' }, () => withRoot((root) => {
  const home = path.join(root, 'home');
  const prefix = path.join(root, 'opt', 'homebrew');
  const packageRoot = path.join(prefix, 'lib', 'node_modules', '@openai', 'codex');
  write(path.join(packageRoot, 'package.json'), JSON.stringify({ name: '@openai/codex', version: '0.159.1', bin: { codex: 'bin/codex.js' } }));
  write(path.join(packageRoot, 'bin', 'codex.js'));
  const vendor = write(path.join(packageRoot, 'node_modules', '@openai', 'codex-darwin-arm64', 'vendor', 'aarch64-apple-darwin', 'codex', 'codex'), '', 0o755);
  const npmBin = link('../lib/node_modules/@openai/codex/bin/codex.js', path.join(prefix, 'bin', 'codex'));
  const standalone = write(path.join(home, '.codex', 'packages', 'standalone', 'releases', '0.166.0-aarch64-apple-darwin', 'bin', 'codex'), '', 0o755);
  const shadow = link(standalone, path.join(home, '.local', 'bin', 'codex'));
  const env = { LEGIONCTL_AGENT_PID: String(AGENT), LEGIONCTL_NPM_PREFIX: prefix, LEGIONCTL_NPM_PACKAGE: '@openai/codex' };
  const io = sandbox(root);
  const install = profileInstall('codex-cli', { platform: 'darwin', env, home, io });
  assert.deepEqual(install.owned, [packageRoot]);
  assert.deepEqual(install.links, [npmBin]);
  const verdict = (rows, options = {}) => processExitVerdict('codex-cli', rows, 1, { platform: 'darwin', install, io, ...options });

  // A launchd job keeps the standalone install's app server running for good.
  const appServer = { pid: 2305, parentPid: 2224, name: shadow, command: `${shadow} app-server --listen unix:///tmp/codex.sock` };
  assert.deepEqual(verdict([appServer]), IDLE, 'npm never touches the standalone install');
  assert.deepEqual(verdict([appServer], { install: null }), BUSY, 'by name alone it is the same product');
  assert.deepEqual(verdict([{ pid: 9, name: 'node', command: `node ${npmBin} app-server` }]), BUSY);
  assert.deepEqual(verdict([{ pid: 9, name: vendor, command: `${vendor} app-server` }]), BUSY);
  assert.deepEqual(verdict([{ pid: 9, name: npmBin, command: npmBin }]), BUSY);
  // A path that cannot be read leaves only the name to go by.
  assert.deepEqual(verdict([{ pid: 9, name: 'codex', command: 'codex app-server' }]), BUSY);

  // The probe takes the prefix and package from the agent that ran it.
  const { run } = macPs({ 2305: { comm: shadow, args: appServer.command } });
  assert.deepEqual(inspectProfileProcesses('codex-cli', { platform: 'darwin', run, io, home, env, parentPid: AGENT }), IDLE);
  assert.deepEqual(inspectProfileProcesses('codex-cli', { platform: 'darwin', run, io, home, env: {}, parentPid: AGENT }), BUSY);
}));

test('an npm CLI that renames itself on Linux is still matched by the name it took', { skip: !posix && 'POSIX paths and links' }, () => withRoot((root) => {
  const prefix = path.join(root, 'usr', 'local');
  const packageRoot = path.join(prefix, 'lib', 'node_modules', '@anthropic-ai', 'claude-code');
  write(path.join(packageRoot, 'package.json'), JSON.stringify({ name: '@anthropic-ai/claude-code', bin: { claude: 'cli.js' } }));
  write(path.join(packageRoot, 'cli.js'));
  const bin = link('../lib/node_modules/@anthropic-ai/claude-code/cli.js', path.join(prefix, 'bin', 'claude'));
  // cli.js sets process.title to claude: comm says so, argv is that name
  // written over the old words, and exe is still the runtime.
  const processes = { 6500: { name: 'claude', ppid: 6400, argv: ['claude', '', '', ''], exe: '/usr/bin/node' } };
  const asked = { platform: 'linux', io: procFs(root, processes), home: path.join(root, 'home'), env: { LEGIONCTL_NPM_PREFIX: prefix, LEGIONCTL_NPM_PACKAGE: '@anthropic-ai/claude-code' }, parentPid: AGENT };
  assert.deepEqual(inspectProfileProcesses('claude-code', asked), BUSY, 'the runtime\'s own path is not where the session runs from');
  // Renamed to anything else, it is no session of this product.
  processes[6500] = { ...processes[6500], name: 'next-server', argv: ['next-server (v15.0.0)', '', ''] };
  assert.deepEqual(inspectProfileProcesses('claude-code', asked), IDLE);
  // Debian's nodejs is the same runtime under another name.
  processes[6500] = { name: 'nodejs', ppid: 6400, argv: ['nodejs', bin], exe: '/usr/bin/node' };
  assert.deepEqual(inspectProfileProcesses('claude-code', asked), BUSY);
}));

test('a bin link another package has taken over is no longer the package\'s own',{ skip: !posix && 'POSIX paths and links' }, () => withRoot((root) => {
  const prefix = path.join(root, 'prefix');
  const other = write(path.join(prefix, 'lib', 'node_modules', '@opencode', 'cli', 'bin', 'opencode.exe'), '', 0o755);
  const bin = link(other, path.join(prefix, 'bin', 'opencode'));
  const env = { LEGIONCTL_NPM_PREFIX: prefix, LEGIONCTL_NPM_PACKAGE: 'opencode-ai' };
  const install = profileInstall('opencode', { platform: 'linux', env, home: root, io: sandbox(root) });
  assert.deepEqual(install.links, []);
  const row = { pid: 9, name: 'opencode', argv: [bin], command: bin, executablePath: other };
  assert.deepEqual(processExitVerdict('opencode', [row], 1, { platform: 'linux', install, io: sandbox(root) }), IDLE);
}));

test('a Windows npm service is matched under <prefix>\\node_modules\\<package>, without case', () => {
  const prefix = 'C:\\Users\\me\\AppData\\Roaming\\npm';
  const install = profileInstall('codex-cli', { platform: 'win32', env: { LEGIONCTL_NPM_PREFIX: prefix, LEGIONCTL_NPM_PACKAGE: '@openai/codex' }, home: 'C:\\Users\\me', io: sandbox('C:\\nowhere') });
  assert.deepEqual(install.owned, ['C:\\Users\\me\\AppData\\Roaming\\npm\\node_modules\\@openai\\codex']);
  const verdict = (rows) => processExitVerdict('codex-cli', rows, 99999, { platform: 'win32', systemRoot: 'C:\\Windows', install, io: sandbox('C:\\nowhere') });
  const node = 'C:\\Program Files\\nodejs\\node.exe';
  assert.deepEqual(verdict([{ pid: 10, parentPid: 9, name: 'node.exe', command: `"${node}" "${prefix}\\\\node_modules\\@openai\\codex\\bin\\codex.js" app-server`, executablePath: node }]), BUSY);
  assert.deepEqual(verdict([{ pid: 10, parentPid: 9, name: 'codex.exe', command: '"C:\\Users\\me\\APPDATA\\ROAMING\\NPM\\node_modules\\@OpenAI\\Codex\\vendor\\codex.exe" app-server', executablePath: 'C:\\Users\\me\\APPDATA\\ROAMING\\NPM\\node_modules\\@OpenAI\\Codex\\vendor\\codex.exe' }]), BUSY);
  const elsewhere = 'C:\\Tools\\codex\\bin\\codex.js';
  assert.deepEqual(verdict([{ pid: 10, parentPid: 9, name: 'node.exe', command: `"${node}" "${elsewhere}" app-server`, executablePath: node }]), IDLE, 'a quoted script path decides even when it cannot be resolved');
  const standalone = 'C:\\Users\\me\\.codex\\packages\\standalone\\current\\codex.exe';
  assert.deepEqual(verdict([{ pid: 10, parentPid: 9, name: 'codex.exe', command: `"${standalone}" app-server`, executablePath: standalone }]), IDLE);
  assert.deepEqual(verdict([{ pid: 10, parentPid: 9, name: 'codex.exe', command: null, executablePath: null }]), BUSY, 'a protected process is matched by its name');
});

test('Windows desktop applications are told from the CLIs of the same name by where they are installed', () => {
  const files = new Set(['C:\\Users\\me\\AppData\\Local\\Claude\\Claude.exe']);
  const io = { realpathSync: () => { throw new Error('missing'); }, statSync: (file) => { if (!files.has(file)) throw new Error('missing'); return { isFile: () => true }; } };
  const env = { LOCALAPPDATA: 'C:\\Users\\me\\AppData\\Local', ProgramFiles: 'C:\\Program Files' };
  const desktop = profileInstall('claude-desktop', { platform: 'win32', env, home: 'C:\\Users\\me', io });
  const cli = profileInstall('claude-code', { platform: 'win32', env, home: 'C:\\Users\\me', io });
  assert.deepEqual(desktop.owned, ['C:\\Users\\me\\AppData\\Local\\Claude']);
  assert.deepEqual(desktop.foreign, []);
  assert.deepEqual(cli, { owned: [], links: [], foreign: ['C:\\Users\\me\\AppData\\Local\\Claude', 'C:\\Program Files\\WindowsApps'] });
  const app = { pid: 10, parentPid: 9, name: 'claude.exe', command: '"C:\\Users\\me\\AppData\\Local\\Claude\\app-1.0.0\\claude.exe"', executablePath: 'C:\\Users\\me\\AppData\\Local\\Claude\\app-1.0.0\\claude.exe' };
  const session = { pid: 11, parentPid: 9, name: 'claude.exe', command: '"C:\\Users\\me\\.local\\bin\\claude.exe" --print', executablePath: 'C:\\Users\\me\\.local\\bin\\claude.exe' };
  const verdict = (profile, install, rows) => processExitVerdict(profile, rows, 99999, { platform: 'win32', systemRoot: 'C:\\Windows', install, io });
  assert.deepEqual(verdict('claude-code', cli, [app]), IDLE);
  assert.deepEqual(verdict('claude-code', cli, [session]), BUSY);
  assert.deepEqual(verdict('claude-desktop', desktop, [app]), BUSY);
  assert.deepEqual(verdict('claude-desktop', desktop, [session]), IDLE);

  // A packaged application lives where discovery does not look, and is no CLI.
  const store = (name) => `C:\\Program Files\\WindowsApps\\Vendor.App_1.2.3.0_x64__abcdefghjkmnp\\app\\${name}`;
  const packaged = (name) => ({ pid: 12, parentPid: 9, name, command: `"${store(name)}"`, executablePath: store(name) });
  assert.deepEqual(verdict('claude-code', cli, [packaged('claude.exe')]), IDLE);
  const codexCli = profileInstall('codex-cli', { platform: 'win32', env, home: 'C:\\Users\\me', io });
  const codexApp = profileInstall('codex-desktop', { platform: 'win32', env, home: 'C:\\Users\\me', io });
  assert.deepEqual(verdict('codex-cli', codexCli, [packaged('Codex.exe')]), IDLE);
  assert.deepEqual(verdict('codex-cli', codexCli, [packaged('ChatGPT.exe')]), IDLE);
  assert.deepEqual(verdict('codex-desktop', codexApp, [packaged('Codex.exe')]), BUSY, 'the application still knows itself by name');
});

test('a Mac desktop profile is matched by its bundle, and the CLIs and apps of one vendor no longer overlap', { skip: !posix && 'POSIX paths' }, () => withRoot((root) => {
  const home = path.join(root, 'home');
  const bundle = path.join(home, 'Applications', 'Claude.app');
  const main = write(path.join(bundle, 'Contents', 'MacOS', 'Claude'), '', 0o755);
  const copy = write(path.join(root, 'Downloads', 'Claude.app', 'Contents', 'MacOS', 'Claude'), '', 0o755);
  const io = sandbox(root);
  const install = profileInstall('claude-desktop', { platform: 'darwin', env: {}, home, io });
  assert.deepEqual(install.owned, [bundle]);
  const verdict = (profile, rows, options = {}) => processExitVerdict(profile, rows, 1, { platform: 'darwin', io, ...options });
  assert.deepEqual(verdict('claude-desktop', [{ pid: 9, name: main, command: main }], { install }), BUSY);
  assert.deepEqual(verdict('claude-desktop', [{ pid: 9, name: copy, command: copy }], { install }), IDLE, 'an update replaces the installed bundle, not a copy elsewhere');
  assert.deepEqual(verdict('claude-code', [{ pid: 9, name: main, command: main }], { install: profileInstall('claude-code', { platform: 'darwin', env: {}, home, io }) }), IDLE);

  // Without a bundle to go by, names decide, each for its own profile only.
  const session = { pid: 9, name: 'claude', command: 'claude --output-format stream-json' };
  const app = { pid: 9, name: '/Applications/Claude.app/Contents/MacOS/Claude', command: '/Applications/Claude.app/Contents/MacOS/Claude' };
  const codex = { pid: 9, name: '/Users/me/.local/bin/codex', command: '/Users/me/.local/bin/codex app-server' };
  const chatgpt = { pid: 9, name: '/Applications/ChatGPT.app/Contents/MacOS/ChatGPT', command: '/Applications/ChatGPT.app/Contents/MacOS/ChatGPT' };
  assert.deepEqual(verdict('claude-code', [session]), BUSY);
  assert.deepEqual(verdict('claude-code', [app]), IDLE);
  assert.deepEqual(verdict('claude-desktop', [app]), BUSY);
  assert.deepEqual(verdict('claude-desktop', [session]), IDLE);
  assert.deepEqual(verdict('codex-cli', [codex]), BUSY);
  assert.deepEqual(verdict('codex-desktop', [codex]), IDLE);
  assert.deepEqual(verdict('codex-desktop', [chatgpt]), BUSY);
  assert.deepEqual(verdict('codex-cli', [chatgpt]), IDLE);
}));

test('a busy command is told which agent asked and, for an npm service, which install it guards', async () => {
  const echo = [process.execPath, '-e', "const e = process.env; process.stdout.write(JSON.stringify({ busy: false, reason: [e.LEGIONCTL_AGENT_PID, e.LEGIONCTL_NPM_PREFIX ?? '-', e.LEGIONCTL_NPM_PACKAGE ?? '-'].join(' ') }))"];
  const previous = process.env.LEGIONCTL_NPM_PREFIX;
  process.env.LEGIONCTL_NPM_PREFIX = '/inherited';
  try {
    const npm = await checkServiceBusy({ id: 'codex-cli', name: 'Codex CLI', kind: 'npm', package: '@openai/codex', npmPrefix: '/opt/homebrew', busy: { type: 'command', command: echo } });
    assert.equal(npm.reason, `${process.pid} /opt/homebrew @openai/codex`);
    const command = await checkServiceBusy({ id: 'claude-code', name: 'Claude Code', kind: 'command', busy: { type: 'command', command: echo } });
    assert.equal(command.reason, `${process.pid} - -`, 'an inherited value is never passed on as the install');
  } finally {
    if (previous === undefined) delete process.env.LEGIONCTL_NPM_PREFIX;
    else process.env.LEGIONCTL_NPM_PREFIX = previous;
  }
});
