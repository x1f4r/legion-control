import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { DatabaseSync } from 'node:sqlite';

const repository = path.resolve(import.meta.dirname, '..');
const installFiles = ['install-linux.sh', 'promote.mjs', 'launcher.mjs', 'launcher.sh', 'dispatch.sh', 'legion-control-update.service', 'legion-control-update.timer', 't3-code.service'];

function sourceFixture(root, { version = '3.0.0', healthy = true } = {}) {
  const agent = path.join(root, `source-${version}-${healthy}`);
  fs.mkdirSync(path.join(agent, 'src'), { recursive: true });
  fs.mkdirSync(path.join(agent, 'install'));
  fs.copyFileSync(path.join(repository, 'agent', 'src', 'agent-swap.mjs'), path.join(agent, 'src', 'agent-swap.mjs'));
  for (const file of installFiles) fs.copyFileSync(path.join(repository, 'agent', 'install', file), path.join(agent, 'install', file));
  fs.writeFileSync(path.join(agent, 'package.json'), JSON.stringify({ version, type: 'module' }));
  fs.writeFileSync(path.join(agent, 'src', 'index.mjs'), `
const command=process.argv[2];
if(command==='version') process.stdout.write(JSON.stringify({ok:true,agentVersion:${JSON.stringify(version)},contract:3,selfTest:{ok:${healthy}}})+'\\n');
else process.stdout.write(JSON.stringify({ok:true,command})+'\\n');
`);
  return agent;
}

function install(source, base, home, extraEnv = {}) {
  return spawnSync('bash', [path.join(source, 'install', 'install-linux.sh'), '--skip-scheduler'], {
    encoding: 'utf8',
    env: { ...process.env, HOME: home, LEGIONCTL_HOME: base, ...extraEnv },
  });
}

test('linux installer handles spaces and apostrophes, preserves config, and copies package metadata', { skip: process.platform === 'win32' }, () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion installer '));
  const home = path.join(root, "home with space and ' apostrophe");
  const base = path.join(home, '.legion-control');
  fs.mkdirSync(base, { recursive: true });
  const config = '{"sentinel":"leave exactly alone"}\n';
  fs.writeFileSync(path.join(base, 'config.json'), config);
  const source = sourceFixture(root);
  const nodeDirectory = path.join(root, "node path ' & | shell");
  fs.mkdirSync(nodeDirectory);
  const node = path.join(nodeDirectory, 'node');
  fs.symlinkSync(process.execPath, node);

  const result = install(source, base, home, { LEGION_NODE_BIN: node });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(fs.readFileSync(path.join(base, 'config.json'), 'utf8'), config);
  assert.equal(JSON.parse(fs.readFileSync(path.join(base, 'agent', 'package.json'), 'utf8')).version, '3.0.0');
  assert.equal(fs.existsSync(path.join(base, 'agent', 'install', 'launcher.mjs')), true);
  const launched = spawnSync(path.join(base, 'bin', 'legionctl'), ['status'], { encoding: 'utf8' });
  assert.equal(launched.status, 0, launched.stderr);
  assert.equal(JSON.parse(launched.stdout).command, 'status');
});

test('linux installer rejects a failed staged self-check without replacing the live tree', { skip: process.platform === 'win32' }, () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion installer rollback '));
  const home = path.join(root, 'home');
  const base = path.join(home, '.legion-control');
  const good = sourceFixture(root, { version: '3.0.0', healthy: true });
  const first = install(good, base, home);
  assert.equal(first.status, 0, first.stderr);

  const bad = sourceFixture(root, { version: '3.0.1', healthy: false });
  const second = install(bad, base, home);
  assert.equal(second.status, 1);
  assert.match(second.stderr, /failed version --check/);
  assert.equal(JSON.parse(fs.readFileSync(path.join(base, 'agent', 'package.json'), 'utf8')).version, '3.0.0');
});

test('installer promotion refuses a held operation mutex and preserves the active version', { skip: process.platform === 'win32' }, () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion installer exclusion '));
  const home = path.join(root, 'home');
  const base = path.join(home, '.legion-control');
  const oldSource = sourceFixture(root, { version: '3.0.0' });
  const installed = install(oldSource, base, home);
  assert.equal(installed.status, 0, installed.stderr);
  const nextSource = sourceFixture(root, { version: '3.0.1' });
  const db = new DatabaseSync(path.join(base, 'op.lock.sqlite'));
  db.exec('BEGIN IMMEDIATE');
  try {
    const blocked = install(nextSource, base, home);
    assert.equal(blocked.status, 1);
    assert.match(blocked.stderr, /another operation is running/);
    assert.equal(JSON.parse(fs.readFileSync(path.join(base, 'agent', 'package.json'))).version, '3.0.0');
    assert.equal(fs.readdirSync(base).some((name) => name.startsWith('agent.install.new.')), true);
  } finally { db.exec('ROLLBACK'); db.close(); }
  const next = install(nextSource, base, home);
  assert.equal(next.status, 0, next.stderr);
  assert.equal(JSON.parse(fs.readFileSync(path.join(base, 'agent', 'package.json'))).version, '3.0.1');
  assert.equal(JSON.parse(fs.readFileSync(path.join(base, 'agent.prev', 'package.json'))).version, '3.0.0');
});
