import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { PassThrough } from 'node:stream';
import { spawn, spawnSync } from 'node:child_process';
import zlib from 'node:zlib';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { readBundle, resolveBootstrapLayout, selfTest } from '../src/selfupdate.mjs';
import { acquireOperationLock } from '../src/lock.mjs';

const AGENT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const CURRENT_VERSION = JSON.parse(fs.readFileSync(path.join(AGENT, 'package.json'), 'utf8')).version;

function signingPair() { return crypto.generateKeyPairSync('ed25519'); }
const sha = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');

function signTree(dir, privateKey) {
  const files = [];
  const walk = (relative) => {
    const absolute = path.join(dir, relative);
    for (const name of fs.readdirSync(absolute).sort()) {
      const child = path.posix.join(relative, name);
      const stat = fs.lstatSync(path.join(dir, child));
      if (stat.isDirectory()) walk(child);
      else if (child !== 'MANIFEST.json' && child !== 'MANIFEST.json.sig') {
        const bytes = fs.readFileSync(path.join(dir, child));
        files.push({ path: child, sha256: sha(bytes), size: bytes.length });
      }
    }
  };
  for (const relative of ['src', 'install']) if (fs.existsSync(path.join(dir, relative))) walk(relative);
  for (const relative of ['package.json']) {
    if (!fs.existsSync(path.join(dir, relative))) continue;
    const bytes = fs.readFileSync(path.join(dir, relative));
    files.push({ path: relative, sha256: sha(bytes), size: bytes.length });
  }
  files.sort((a, b) => a.path.localeCompare(b.path));
  const version = JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8')).version;
  const manifest = Buffer.from(`${JSON.stringify({ schema: 1, version, contract: 3, files }, null, 2)}\n`);
  fs.writeFileSync(path.join(dir, 'MANIFEST.json'), manifest);
  fs.writeFileSync(path.join(dir, 'MANIFEST.json.sig'), `${crypto.sign(null, manifest, privateKey).toString('base64')}\n`);
}

function patchTrust(src, publicKey) {
  const file = path.join(src, 'trust.mjs');
  const pemBody = publicKey.export({ type: 'spki', format: 'pem' }).toString().split('\n')[1];
  const original = fs.readFileSync(file, 'utf8');
  fs.writeFileSync(file, original.replace(/  'MCowBQYDK2VwAyEA[^']+',/, `  '${pemBody}',`));
}

function bundleTree(directory) {
  const parts = [];
  const walk = (relative = '') => {
    for (const name of fs.readdirSync(path.join(directory, relative))) {
      const child = relative ? `${relative}/${name}` : name;
      if (fs.statSync(path.join(directory, child)).isDirectory()) { walk(child); continue; }
      const bytes = fs.readFileSync(path.join(directory, child));
      const header = Buffer.alloc(512);
      header.write(`agent/${child}`, 0, 100);
      header.write('0000644\0', 100);
      header.write(`${bytes.length.toString(8).padStart(11, '0')}\0`, 124);
      header.write('        ', 148);
      header.write('0', 156);
      header.write('ustar\0', 257);
      header.write('00', 263);
      const checksum = header.reduce((sum, byte) => sum + byte, 0);
      header.write(`${checksum.toString(8).padStart(6, '0')}\0 `, 148);
      parts.push(header, bytes, Buffer.alloc((512 - bytes.length % 512) % 512));
    }
  };
  walk();
  return zlib.gzipSync(Buffer.concat([...parts, Buffer.alloc(1024)]));
}

async function isolatedModule() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-selfupdate-module-'));
  const src = path.join(root, 'src');
  fs.cpSync(path.join(AGENT, 'src'), src, { recursive: true });
  const pair = signingPair();
  patchTrust(src, pair.publicKey);
  const module = await import(`${pathToFileURL(path.join(src, 'selfupdate.mjs')).href}?${Date.now()}-${Math.random()}`);
  return { root, src, pair, module };
}

test('stdin has an idle deadline and closes the readable', async () => {
  const input = new PassThrough();
  const result = await readBundle({ stdin: true, input, idleTimeoutMs: 20, totalTimeoutMs: 200 });
  assert.equal(result.ok, false);
  assert.equal(result.reasonCode, 'timed-out');
  assert.equal(input.destroyed, true);
});

test('stdin total deadline wins even when bytes keep arriving', async () => {
  const input = new PassThrough();
  const ticker = setInterval(() => input.write('x'), 5);
  try {
    const result = await readBundle({ stdin: true, input, idleTimeoutMs: 50, totalTimeoutMs: 25 });
    assert.equal(result.ok, false);
    assert.equal(result.reasonCode, 'timed-out');
    assert.match(result.error, /finish within/);
  } finally { clearInterval(ticker); input.destroy(); }
});

test('timing out process stdin closes the pipe and lets the process exit', async () => {
  const moduleUrl = pathToFileURL(path.join(AGENT, 'src', 'selfupdate.mjs')).href;
  const child = spawn(process.execPath, ['--input-type=module', '-e', `
    import { readBundle } from ${JSON.stringify(moduleUrl)};
    const result = await readBundle({ stdin: true, idleTimeoutMs: 25, totalTimeoutMs: 100 });
    process.stdout.write(JSON.stringify(result));
  `], { stdio: ['pipe', 'pipe', 'pipe'] });
  let stdout = '';
  child.stdout.setEncoding('utf8');
  child.stdout.on('data', (chunk) => { stdout += chunk; });
  const exited = await new Promise((resolve, reject) => {
    const deadline = setTimeout(() => { child.kill('SIGKILL'); reject(new Error('child kept stdin open after timeout')); }, 1000);
    child.once('exit', (code) => { clearTimeout(deadline); resolve(code); });
  });
  assert.equal(exited, 0);
  assert.equal(JSON.parse(stdout).reasonCode, 'timed-out');
});

test('the self-update CLI exits on a stalled upload without taking the operation mutex', async () => {
  const isolated = await isolatedModule();
  let child;
  try {
    const modulePath = path.join(isolated.src, 'selfupdate.mjs');
    fs.writeFileSync(modulePath, fs.readFileSync(modulePath, 'utf8').replace('STDIN_IDLE_TIMEOUT_MS = 15_000', 'STDIN_IDLE_TIMEOUT_MS = 25'));
    const home = path.join(isolated.root, 'data');
    child = spawn(process.execPath, [path.join(isolated.src, 'index.mjs'), 'self-update', '--stdin', '--op', 'stalled-cli-0001'], {
      env: { ...process.env, LEGIONCTL_HOME: home }, stdio: ['pipe', 'pipe', 'pipe'],
    });
    let stdout = '';
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    const code = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => { child.kill('SIGKILL'); reject(new Error('CLI did not exit after the upload deadline')); }, 2000);
      child.once('close', (status) => { clearTimeout(timer); resolve(status); });
    });
    assert.equal(code, 1);
    assert.equal(JSON.parse(stdout).reasonCode, 'timed-out');
    assert.equal(fs.existsSync(path.join(home, 'op.lock.sqlite')), false);
  } finally {
    child?.kill('SIGKILL');
    fs.rmSync(isolated.root, { recursive: true, force: true });
  }
});

test('a stalled self-update upload does not hold the machine operation lock', async () => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-upload-lock-'));
  const oldHome = process.env.LEGIONCTL_HOME;
  const input = new PassThrough();
  try {
    process.env.LEGIONCTL_HOME = base;
    const module = await import(`../src/selfupdate.mjs?lock=${Date.now()}-${Math.random()}`);
    const pending = module.runSelfUpdate({ system: { id: 'test' } }, { stdin: true, input, idleTimeoutMs: 40, totalTimeoutMs: 200 });
    await new Promise((resolve) => setTimeout(resolve, 10));
    const acquired = acquireOperationLock({ kind: 'restart', opId: 'parallel-lock-check' });
    assert.equal(acquired.ok, true, acquired.message);
    acquired.lock.release();
    const result = await pending;
    assert.equal(result.reasonCode, 'timed-out');
  } finally {
    input.destroy();
    if (oldHome === undefined) delete process.env.LEGIONCTL_HOME; else process.env.LEGIONCTL_HOME = oldHome;
    fs.rmSync(base, { recursive: true, force: true });
  }
});

test('bootstrap layout is derived from incoming/agent and rejects a different configured base', () => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-layout-'));
  try {
    const entry = path.join(base, 'incoming', 'agent', 'src', 'index.mjs');
    fs.mkdirSync(path.dirname(entry), { recursive: true });
    fs.writeFileSync(entry, '');
    const layout = resolveBootstrapLayout(pathToFileURL(entry).href, null);
    assert.equal(layout.ok, true);
    assert.equal(layout.base, base);
    assert.equal(resolveBootstrapLayout(pathToFileURL(entry).href, path.join(base, 'other')).ok, false);
    assert.equal(resolveBootstrapLayout(pathToFileURL(path.join(base, 'random', 'index.mjs')).href, null).ok, false);
  } finally { fs.rmSync(base, { recursive: true, force: true }); }
});

test('self-test must report the version and contract from the signed manifest', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-selftest-'));
  try {
    fs.mkdirSync(path.join(root, 'src'));
    fs.writeFileSync(path.join(root, 'src', 'index.mjs'), 'console.log(JSON.stringify({ok:true,selfTest:{ok:true},contract:3,agentVersion:"9.9.9"}))\n');
    const result = selfTest(root, { expectedVersion: '3.0.0', expectedContract: 3 });
    assert.equal(result.ok, false);
    assert.match(result.output, /signed manifest requires 3\.0\.0/);
    fs.writeFileSync(path.join(root, 'src', 'index.mjs'), 'console.log(JSON.stringify({ok:true,selfTest:{ok:true},contract:4,agentVersion:"3.0.0"}))\n');
    const contract = selfTest(root, { expectedVersion: '3.0.0', expectedContract: 3 });
    assert.equal(contract.ok, false);
    assert.match(contract.output, /signed manifest requires 3/);
    fs.writeFileSync(path.join(root, 'src', 'index.mjs'), 'console.log(JSON.stringify({ok:true,selfTest:{ok:false,failed:["missing module"]},contract:3,agentVersion:"3.0.0"}))\n');
    const modules = selfTest(root, { expectedVersion: '3.0.0', expectedContract: 3 });
    assert.equal(modules.ok, false);
    assert.match(modules.output, /module and configuration checks/);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});

test('version --check fails when an imported module is missing', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-module-check-'));
  try {
    fs.cpSync(path.join(AGENT, 'src'), path.join(root, 'src'), { recursive: true });
    const entry = path.join(root, 'src', 'index.mjs');
    fs.writeFileSync(entry, fs.readFileSync(entry, 'utf8').replace("'http.mjs', 'lock.mjs'", "'missing-check-module.mjs', 'lock.mjs'"));
    const result = spawnSync(process.execPath, [path.join(root, 'src', 'index.mjs'), 'version', '--check'], {
      encoding: 'utf8', env: { ...process.env, LEGIONCTL_HOME: path.join(root, 'data') }, timeout: 5_000,
    });
    assert.equal(result.status, 1);
    const reply = JSON.parse(result.stdout);
    assert.equal(reply.ok, false);
    assert.equal(reply.selfTest.ok, false);
    assert.match(reply.selfTest.failed.join(' '), /missing-check-module.mjs/);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});

for (const worker of ['status-probe-worker.mjs', 'service-profile-probe.mjs']) {
  for (const damage of ['missing', 'syntax', 'nested-import']) {
    test(`staged self-test rejects ${worker} with ${damage} damage`, () => {
      const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-worker-check-'));
      const oldHome = process.env.LEGIONCTL_HOME;
      try {
        process.env.LEGIONCTL_HOME = path.join(root, 'data');
        fs.cpSync(path.join(AGENT, 'src'), path.join(root, 'src'), { recursive: true });
        const entry = path.join(root, 'src', worker);
        if (damage === 'missing') fs.unlinkSync(entry);
        else if (damage === 'syntax') fs.writeFileSync(entry, 'this is deliberately invalid syntax !!!');
        else {
          fs.writeFileSync(entry, "import './worker-only-dependency.mjs';\n");
          fs.writeFileSync(path.join(root, 'src', 'worker-only-dependency.mjs'), "import './missing-nested-module.mjs';\n");
        }
        const result = selfTest(root, { expectedVersion: CURRENT_VERSION, expectedContract: 3 });
        assert.equal(result.ok, false, result.output);
        assert.match(result.output, /worker modules/);
        assert.ok(result.output.includes(worker), result.output);
        if (damage === 'nested-import') assert.match(result.output, /missing-nested-module/);
      } finally {
        if (oldHome === undefined) delete process.env.LEGIONCTL_HOME; else process.env.LEGIONCTL_HOME = oldHome;
        fs.rmSync(root, { recursive: true, force: true });
      }
    });
  }
}

test('staged self-test parses worker entry points and dependencies without executing them', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-worker-no-execute-'));
  const oldHome = process.env.LEGIONCTL_HOME;
  try {
    process.env.LEGIONCTL_HOME = path.join(root, 'data');
    fs.cpSync(path.join(AGENT, 'src'), path.join(root, 'src'), { recursive: true });
    const marker = path.join(root, 'worker-executed');
    const sentinel = `import fs from 'node:fs'; fs.writeFileSync(${JSON.stringify(marker)}, 'executed'); throw new Error('worker must not execute');\n`;
    for (const worker of ['status-probe-worker.mjs', 'service-profile-probe.mjs']) {
      fs.writeFileSync(path.join(root, 'src', worker), `import './worker-only-dependency.mjs';\n${sentinel}`);
    }
    fs.writeFileSync(path.join(root, 'src', 'worker-only-dependency.mjs'), sentinel);
    const result = selfTest(root, { expectedVersion: CURRENT_VERSION, expectedContract: 3 });
    assert.equal(result.ok, true, result.output);
    assert.equal(fs.existsSync(marker), false);
  } finally {
    if (oldHome === undefined) delete process.env.LEGIONCTL_HOME; else process.env.LEGIONCTL_HOME = oldHome;
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('a correctly signed candidate with a broken child worker never replaces the live agent', async () => {
  const isolated = await isolatedModule();
  const oldHome = process.env.LEGIONCTL_HOME;
  try {
    process.env.LEGIONCTL_HOME = isolated.root;
    const live = path.join(isolated.root, 'agent');
    fs.mkdirSync(path.join(live, 'src'), { recursive: true });
    const liveBytes = 'retained live agent bytes\n';
    fs.writeFileSync(path.join(live, 'src', 'index.mjs'), liveBytes);
    const candidate = path.join(isolated.root, 'candidate');
    fs.cpSync(isolated.src, path.join(candidate, 'src'), { recursive: true });
    fs.copyFileSync(path.join(AGENT, 'package.json'), path.join(candidate, 'package.json'));
    fs.writeFileSync(path.join(candidate, 'src', 'status-probe-worker.mjs'), 'this is deliberately invalid syntax !!!');
    signTree(candidate, isolated.pair.privateKey);
    const result = await isolated.module.runSelfUpdate({ system: { id: 'test' } }, {
      stdin: true, input: PassThrough.from([bundleTree(candidate)]), opId: 'broken-worker-install-0001',
    });
    assert.equal(result.ok, false, result.message);
    assert.equal(result.selfTest?.ok, false, result.message);
    assert.match(result.message, /status-probe-worker/);
    assert.equal(fs.readFileSync(path.join(live, 'src', 'index.mjs'), 'utf8'), liveBytes);
    assert.equal(fs.existsSync(path.join(isolated.root, 'agent.prev')), false);
    assert.equal(fs.existsSync(path.join(isolated.root, 'agent.new')), false);
  } finally {
    if (oldHome === undefined) delete process.env.LEGIONCTL_HOME; else process.env.LEGIONCTL_HOME = oldHome;
    fs.rmSync(isolated.root, { recursive: true, force: true });
  }
});

test('installed manifest is reported verified only while the signed tree is complete', async () => {
  const isolated = await isolatedModule();
  try {
    const tree = path.join(isolated.root, 'tree');
    fs.mkdirSync(path.join(tree, 'src'), { recursive: true });
    fs.writeFileSync(path.join(tree, 'src', 'index.mjs'), 'console.log("ok")\n');
    fs.writeFileSync(path.join(tree, 'package.json'), '{"version":"3.0.0"}\n');
    signTree(tree, isolated.pair.privateKey);
    assert.equal(isolated.module.installedManifest(tree)?.signatureVerified, true);
    fs.appendFileSync(path.join(tree, 'src', 'index.mjs'), '// tampered\n');
    assert.equal(isolated.module.installedManifest(tree), null);
  } finally { fs.rmSync(isolated.root, { recursive: true, force: true }); }
});

test('staged signed bootstrap installs the intended base, replays, and conflicts with rollback', async () => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-bootstrap-'));
  try {
    const pair = signingPair();
    const incoming = path.join(base, 'incoming', 'agent');
    fs.mkdirSync(incoming, { recursive: true });
    fs.cpSync(path.join(AGENT, 'src'), path.join(incoming, 'src'), { recursive: true });
    fs.cpSync(path.join(AGENT, 'install'), path.join(incoming, 'install'), { recursive: true });
    fs.copyFileSync(path.join(AGENT, 'package.json'), path.join(incoming, 'package.json'));
    patchTrust(path.join(incoming, 'src'), pair.publicKey);
    signTree(incoming, pair.privateKey);

    const live = path.join(base, 'agent');
    fs.mkdirSync(live, { recursive: true });
    fs.writeFileSync(path.join(live, 'package.json'), '{"version":"2.1.0"}\n');
    const entry = path.join(incoming, 'src', 'index.mjs');
    const env = { ...process.env };
    delete env.LEGIONCTL_HOME;
    const first = spawnSync(process.execPath, [entry, 'self-update', '--install', '--op', 'bootstrap-install-0001'], {
      encoding: 'utf8', cwd: os.tmpdir(), env, timeout: 120_000,
    });
    assert.equal(first.status, 0, first.stderr || first.stdout);
    const installed = JSON.parse(first.stdout);
    assert.equal(installed.action, 'installed');
    assert.equal(installed.current.version, CURRENT_VERSION);
    assert.equal(fs.readFileSync(path.join(base, 'bin', 'launcher.mjs'), 'utf8'), fs.readFileSync(path.join(incoming, 'install', 'launcher.mjs'), 'utf8'));
    const wrapper = path.join(base, 'bin', process.platform === 'win32' ? 'legionctl.ps1' : 'legionctl');
    assert.equal(fs.readFileSync(wrapper, 'utf8').includes('@NODE_'), false);
    assert.equal(JSON.parse(fs.readFileSync(path.join(base, 'agent.prev', 'package.json'))).version, '2.1.0');

    const replay = spawnSync(process.execPath, [entry, 'self-update', '--install', '--op', 'bootstrap-install-0001'], {
      encoding: 'utf8', cwd: os.tmpdir(), env, timeout: 120_000,
    });
    assert.equal(replay.status, 0, replay.stderr || replay.stdout);
    assert.equal(JSON.parse(replay.stdout).replayed, true);

    fs.appendFileSync(path.join(incoming, 'package.json'), ' ');
    signTree(incoming, pair.privateKey);
    const changed = spawnSync(process.execPath, [entry, 'self-update', '--install', '--op', 'bootstrap-install-0001'], {
      encoding: 'utf8', cwd: os.tmpdir(), env, timeout: 120_000,
    });
    assert.equal(JSON.parse(changed.stdout).action, 'conflict');

    const rollback = spawnSync(process.execPath, [path.join(base, 'agent', 'src', 'index.mjs'), 'self-update', '--rollback', '--op', 'bootstrap-install-0001'], {
      encoding: 'utf8', env: { ...env, LEGIONCTL_HOME: base }, timeout: 120_000,
    });
    const conflict = JSON.parse(rollback.stdout);
    assert.equal(conflict.action, 'conflict');
    assert.equal(fs.existsSync(path.join(base, 'agent')), true);
  } finally { fs.rmSync(base, { recursive: true, force: true }); }
});

test('rollback refuses a tampered signed previous tree and leaves it retained', async () => {
  const isolated = await isolatedModule();
  const oldHome = process.env.LEGIONCTL_HOME;
  try {
    process.env.LEGIONCTL_HOME = isolated.root;
    const previous = path.join(isolated.root, 'agent.prev');
    fs.mkdirSync(path.join(previous, 'src'), { recursive: true });
    fs.writeFileSync(path.join(previous, 'src', 'index.mjs'), 'console.log(JSON.stringify({ok:true,selfTest:{ok:true},contract:3,agentVersion:"3.0.0"}))\n');
    fs.writeFileSync(path.join(previous, 'package.json'), '{"version":"3.0.0"}\n');
    signTree(previous, isolated.pair.privateKey);
    fs.appendFileSync(path.join(previous, 'src', 'index.mjs'), '// tampered\n');
    const result = await isolated.module.runRollback({ system: { id: 'test' } }, { opId: 'tampered-rollback-0001' });
    assert.equal(result.reasonCode, 'signature-invalid');
    assert.equal(fs.existsSync(previous), true);
    assert.equal(fs.existsSync(path.join(isolated.root, 'agent')), false);
  } finally {
    if (oldHome === undefined) delete process.env.LEGIONCTL_HOME; else process.env.LEGIONCTL_HOME = oldHome;
    fs.rmSync(isolated.root, { recursive: true, force: true });
  }
});

test('bundle installs bind exact bytes and conflict with a rollback operation in either direction', async () => {
  const isolated = await isolatedModule();
  const oldHome = process.env.LEGIONCTL_HOME;
  try {
    process.env.LEGIONCTL_HOME = isolated.root;
    const live = path.join(isolated.root, 'agent');
    const candidate = path.join(isolated.root, 'candidate');
    const script = 'console.log(JSON.stringify({ok:true,selfTest:{ok:true},contract:3,agentVersion:"3.0.0"}))\n';
    for (const tree of [live, candidate]) {
      fs.mkdirSync(path.join(tree, 'src'), { recursive: true });
      fs.cpSync(path.join(AGENT, 'install'), path.join(tree, 'install'), { recursive: true });
      fs.writeFileSync(path.join(tree, 'src', 'index.mjs'), `${script}// ${path.basename(tree)}\n`);
      fs.writeFileSync(path.join(tree, 'package.json'), '{"version":"3.0.0"}\n');
      signTree(tree, isolated.pair.privateKey);
    }
    const bytes = bundleTree(candidate);
    const install = (opId, payload = bytes) => isolated.module.runSelfUpdate({ system: { id: 'test' } }, {
      stdin: true, input: PassThrough.from([payload]), opId,
    });
    const first = await install('bundle-install-0001');
    assert.equal(first.action, 'installed', first.message);
    assert.equal((await install('bundle-install-0001')).replayed, true);
    fs.appendFileSync(path.join(candidate, 'src', 'index.mjs'), '// another payload\n');
    signTree(candidate, isolated.pair.privateKey);
    assert.equal((await install('bundle-install-0001', bundleTree(candidate))).action, 'conflict');
    assert.equal((await isolated.module.runRollback({ system: { id: 'test' } }, { opId: 'bundle-install-0001' })).action, 'conflict');
    const rollback = await isolated.module.runRollback({ system: { id: 'test' } }, { opId: 'bundle-rollback-0001' });
    assert.equal(rollback.action, 'rolled-back', rollback.message);
    assert.equal((await install('bundle-rollback-0001')).action, 'conflict');
    assert.equal(fs.readFileSync(path.join(live, 'src', 'index.mjs'), 'utf8'), `${script}// agent\n`);
  } finally {
    if (oldHome === undefined) delete process.env.LEGIONCTL_HOME; else process.env.LEGIONCTL_HOME = oldHome;
    fs.rmSync(isolated.root, { recursive: true, force: true });
  }
});

test('rollback executes the verified copy and retains the outgoing live tree', async () => {
  const isolated = await isolatedModule();
  const oldHome = process.env.LEGIONCTL_HOME;
  try {
    process.env.LEGIONCTL_HOME = isolated.root;
    const live = path.join(isolated.root, 'agent');
    fs.mkdirSync(live, { recursive: true });
    fs.writeFileSync(path.join(live, 'package.json'), '{"version":"3.1.0"}\n');
    const previous = path.join(isolated.root, 'agent.prev');
    fs.mkdirSync(path.join(previous, 'src'), { recursive: true });
    fs.writeFileSync(path.join(previous, 'src', 'index.mjs'), 'console.log(JSON.stringify({ok:true,selfTest:{ok:true},contract:3,agentVersion:"3.0.0"}))\n');
    fs.writeFileSync(path.join(previous, 'package.json'), '{"version":"3.0.0"}\n');
    signTree(previous, isolated.pair.privateKey);
    const result = await isolated.module.runRollback({ system: { id: 'test' } }, { opId: 'signed-rollback-0001' });
    assert.equal(result.ok, true, result.message);
    assert.equal(result.action, 'rolled-back');
    assert.equal(result.manifest.signatureVerified, true);
    assert.equal(JSON.parse(fs.readFileSync(path.join(live, 'package.json'))).version, '3.0.0');
    assert.equal(JSON.parse(fs.readFileSync(path.join(previous, 'package.json'))).version, '3.1.0');
    const replay = await isolated.module.runRollback({ system: { id: 'test' } }, { opId: 'signed-rollback-0001' });
    assert.equal(replay.replayed, true);
    assert.equal(replay.action, 'rolled-back');
    assert.equal(JSON.parse(fs.readFileSync(path.join(live, 'package.json'))).version, '3.0.0');
    assert.equal(JSON.parse(fs.readFileSync(path.join(previous, 'package.json'))).version, '3.1.0');
  } finally {
    if (oldHome === undefined) delete process.env.LEGIONCTL_HOME; else process.env.LEGIONCTL_HOME = oldHome;
    fs.rmSync(isolated.root, { recursive: true, force: true });
  }
});
