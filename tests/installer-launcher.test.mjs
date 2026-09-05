import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { DatabaseSync } from 'node:sqlite';

const repository = path.resolve(import.meta.dirname, '..');
const launcherSource = path.join(repository, 'agent', 'install', 'launcher.mjs');
const testKeys = crypto.generateKeyPairSync('ed25519');
const testPublic = testKeys.publicKey.export({ type: 'spki', format: 'pem' });

function fixture() {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'legion launcher '));
  const bin = path.join(base, 'bin');
  fs.mkdirSync(bin);
  // Only the isolated launcher copy trusts this test key. Production keeps its
  // pinned release key, whose private half is unavailable to the tests.
  const source = fs.readFileSync(launcherSource, 'utf8').replace(/const PUBLIC_KEY = \[[\s\S]*?\]\.join\('\\n'\);/, `const PUBLIC_KEY = ${JSON.stringify(testPublic)};`);
  fs.writeFileSync(path.join(bin, 'launcher.mjs'), source);
  return fs.realpathSync(base);
}

function tree(base, name, label) {
  const directory = path.join(base, name);
  fs.mkdirSync(path.join(directory, 'src'), { recursive: true });
  fs.writeFileSync(path.join(directory, 'src', 'index.mjs'), `process.stdout.write(JSON.stringify({label:${JSON.stringify(label)},home:process.env.LEGIONCTL_HOME,args:process.argv.slice(2)})+'\\n');\n`);
  fs.writeFileSync(path.join(directory, 'package.json'), JSON.stringify({ version: '3.0.0', type: 'module' }));
  const files = ['package.json', 'src/index.mjs'].map((relative) => {
    const bytes = fs.readFileSync(path.join(directory, relative));
    return { path: relative, size: bytes.length, sha256: crypto.createHash('sha256').update(bytes).digest('hex') };
  });
  const manifest = Buffer.from(JSON.stringify({ schema: 1, version: '3.0.0', contract: 3, files }));
  fs.writeFileSync(path.join(directory, 'MANIFEST.json'), manifest);
  fs.writeFileSync(path.join(directory, 'MANIFEST.json.sig'), crypto.sign(null, manifest, testKeys.privateKey).toString('base64') + '\n');
  return directory;
}

function run(base, args = ['status'], env = {}) {
  return spawnSync(process.execPath, [path.join(base, 'bin', 'launcher.mjs'), ...args], {
    encoding: 'utf8',
    env: { ...process.env, LEGIONCTL_HOME: base, ...env },
  });
}

test('stable launcher forwards argv and pins the absolute install base', () => {
  const base = fixture();
  tree(base, 'agent', 'live');
  const result = run(base, ['run', 'safe-action']);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(JSON.parse(result.stdout), { label: 'live', home: base, args: ['run', 'safe-action'] });
  const otherBase = fs.mkdtempSync(path.join(os.tmpdir(), 'legion wrong base '));
  const pinned = run(base, ['status'], { LEGIONCTL_HOME: otherBase });
  assert.equal(pinned.status, 0, pinned.stderr);
  assert.equal(JSON.parse(pinned.stdout).home, base);
});

test('install rename gap restores the pre-swap live tree and preserves every candidate', () => {
  const base = fixture();
  tree(base, 'agent.prev', 'previous-live');
  tree(base, 'agent.new', 'verified-candidate');
  tree(base, 'agent.swap-old-prev', 'older-backup');
  fs.writeFileSync(path.join(base, 'agent-swap.json'), JSON.stringify({ schema: 1, operation: 'install', phase: 'live-parked' }));

  const result = run(base);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(JSON.parse(result.stdout).label, 'previous-live');
  assert.match(result.stderr, /restored the pre-swap agent/);
  assert.equal(fs.existsSync(path.join(base, 'agent.new', 'src', 'index.mjs')), true);
  assert.equal(fs.existsSync(path.join(base, 'agent.prev', 'src', 'index.mjs')), true);
  assert.equal(JSON.parse(fs.readFileSync(path.join(base, 'agent.prev', 'src', 'index.mjs'), 'utf8').match(/label:("[^"]+")/)[1]), 'older-backup');
});

test('rollback rename gap restores the outgoing live tree without consuming the rollback target', () => {
  const base = fixture();
  tree(base, 'agent.rollback', 'outgoing-live');
  tree(base, 'agent.rollback.new', 'verified-previous');
  fs.writeFileSync(path.join(base, 'agent-swap.json'), JSON.stringify({ schema: 1, operation: 'rollback', phase: 'live-parked' }));

  const result = run(base);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(JSON.parse(result.stdout).label, 'outgoing-live');
  assert.equal(fs.existsSync(path.join(base, 'agent.rollback.new', 'src', 'index.mjs')), true);
});

test('invalid recovery journal fails closed and leaves candidates in place', () => {
  const base = fixture();
  tree(base, 'agent.prev', 'previous-live');
  fs.writeFileSync(path.join(base, 'agent-swap.json'), '{"schema":2,"operation":"install","phase":"live-parked"}');

  const result = run(base);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /swap journal is invalid/);
  assert.equal(fs.existsSync(path.join(base, 'agent.prev', 'src', 'index.mjs')), true);
  assert.equal(fs.existsSync(path.join(base, 'agent')), false);
});

test('a partial or tampered signed-tree marker is refused before agent code runs', () => {
  const base = fixture();
  const active = tree(base, 'agent', 'must-not-run');
  fs.writeFileSync(path.join(active, 'MANIFEST.json'), '{}\n');
  fs.unlinkSync(path.join(active, 'MANIFEST.json.sig'));
  const result = run(base);
  assert.equal(result.status, 1);
  assert.equal(result.stdout, '');
  assert.match(result.stderr, /only one of MANIFEST\.json and MANIFEST\.json\.sig/);
});

test('live operation mutex prevents recovery and no candidate executes', () => {
  const base = fixture();
  tree(base, 'agent.prev', 'must-not-run');
  tree(base, 'agent.new', 'candidate');
  fs.writeFileSync(path.join(base, 'agent-swap.json'), JSON.stringify({ schema: 1, operation: 'install', phase: 'live-parked' }));
  const db = new DatabaseSync(path.join(base, 'op.lock.sqlite'));
  db.exec('BEGIN IMMEDIATE');
  try {
    const result = run(base);
    assert.equal(result.status, 1);
    assert.equal(result.stdout, '');
    assert.match(result.stderr, /another operation is running/);
    assert.equal(fs.existsSync(path.join(base, 'agent')), false);
    assert.equal(fs.existsSync(path.join(base, 'agent.prev')), true);
  } finally { db.exec('ROLLBACK'); db.close(); }
  const result = run(base);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(JSON.parse(result.stdout).label, 'must-not-run');
});

test('tampered or unsigned pre-swap trees are never activated or executed', () => {
  for (const unsigned of [false, true]) {
    const base = fixture();
    const backup = tree(base, 'agent.prev', 'must-not-run');
    if (unsigned) {
      fs.unlinkSync(path.join(backup, 'MANIFEST.json'));
      fs.unlinkSync(path.join(backup, 'MANIFEST.json.sig'));
    } else fs.appendFileSync(path.join(backup, 'src', 'index.mjs'), '\n// tampered\n');
    fs.writeFileSync(path.join(base, 'agent-swap.json'), JSON.stringify({ schema: 1, operation: 'install', phase: 'live-parked' }));
    const result = run(base);
    assert.equal(result.status, 1);
    assert.equal(result.stdout, '');
    assert.match(result.stderr, /refusing automatic recovery/);
    assert.equal(fs.existsSync(path.join(base, 'agent')), false);
    assert.equal(fs.existsSync(backup), true);
  }
});

test('legacy bootstrap recovery requires the exact journal-anchored local baseline', () => {
  for (const tamper of ['none', 'tree', 'inventory', 'journal']) {
    const base = fixture();
    const backup = tree(base, 'agent.prev', 'legacy-live');
    fs.unlinkSync(path.join(backup, 'MANIFEST.json'));
    fs.unlinkSync(path.join(backup, 'MANIFEST.json.sig'));
    const files = ['package.json', 'src/index.mjs'].map((relative) => {
      const bytes = fs.readFileSync(path.join(backup, relative));
      return { path: relative, size: bytes.length, sha256: crypto.createHash('sha256').update(bytes).digest('hex') };
    });
    const inventory = Buffer.from(JSON.stringify({ schema: 1, files }) + '\n');
    fs.writeFileSync(path.join(base, 'agent-baseline.json'), inventory);
    const baselineSha256 = crypto.createHash('sha256').update(inventory).digest('hex');
    fs.writeFileSync(path.join(base, 'agent-swap.json'), JSON.stringify({ schema: 1, operation: 'install', phase: 'live-parked', baselineSha256: tamper === 'journal' ? '0'.repeat(64) : baselineSha256 }));
    if (tamper === 'inventory') fs.appendFileSync(path.join(base, 'agent-baseline.json'), ' ');
    if (tamper === 'tree') fs.appendFileSync(path.join(backup, 'src/index.mjs'), ' ');
    const result = run(base);
    assert.equal(result.status, tamper === 'none' ? 0 : 1, result.stderr);
    if (tamper === 'none') assert.equal(JSON.parse(result.stdout).label, 'legacy-live');
    else {
      assert.equal(result.stdout, '');
      assert.equal(fs.existsSync(backup), true);
      assert.equal(fs.existsSync(path.join(base, 'agent')), false);
    }
  }
});

test('restricted dispatcher never evaluates SSH_ORIGINAL_COMMAND', { skip: process.platform === 'win32' }, () => {
  const base = fixture();
  const bin = path.join(base, 'bin');
  fs.copyFileSync(path.join(repository, 'agent', 'install', 'dispatch.sh'), path.join(bin, 'dispatch.sh'));
  fs.writeFileSync(path.join(bin, 'legionctl'), '#!/bin/sh\nprintf "%s\\n" "$SSH_ORIGINAL_COMMAND"\n');
  fs.chmodSync(path.join(bin, 'dispatch.sh'), 0o755);
  fs.chmodSync(path.join(bin, 'legionctl'), 0o755);
  const marker = path.join(base, 'should-not-exist');
  const original = `$(touch ${marker}) ; echo compromised`;
  const result = spawnSync(path.join(bin, 'dispatch.sh'), [], { encoding: 'utf8', env: { ...process.env, SSH_ORIGINAL_COMMAND: original } });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout.trim(), original);
  assert.equal(fs.existsSync(marker), false);
});
