import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { pathToFileURL } from 'node:url';
import { posixWorkerCommand, userScopeAvailable, windowsWorkerScript } from '../src/detached.mjs';
import { cli, commandService, fixtureConfig, fixtureData, readFixture, withHome, writeConfig } from './helpers.mjs';

async function finish(home, id) {
  const deadline = Date.now() + 25000;
  while (Date.now() < deadline) {
    const file = path.join(home, 'ops', `${id}.json`);
    const op = JSON.parse(fs.readFileSync(file, 'utf8'));
    if (op.state === 'finished') return op;
    await delay(100);
  }
  throw new Error('detached operation did not finish');
}

test('Windows detached launcher carries the private base and treats paths as literals', () => {
  const script = windowsWorkerScript({ node: "C:\\Program Files\\node.exe", entry: "C:\\user's files\\agent\\index.mjs", id: 'test-operation', home: "C:\\user's files\\state", environment: { LEGIONCTL_CLIENT: 'phone', LEGIONCTL_DEVICE: 'travelling laptop', UNRELATED_SECRET: 'never-copied' } });
  assert.match(script, /\$env:LEGIONCTL_HOME = 'C:\\user''s files\\state'/);
  assert.match(script, /& 'C:\\Program Files\\node.exe' 'C:\\user''s files\\agent\\index.mjs' 'op-run' 'test-operation'/);
  assert.match(script, /\$env:LEGIONCTL_CLIENT = 'phone'/);
  assert.equal(script.includes('UNRELATED_SECRET'), false);
});

test('a Linux worker starts in a scope of the user\'s systemd manager, and in place without one', () => {
  const worker = { node: '/opt/node/bin/node', entry: '/srv/agent/src/index.mjs', id: 'scoped-operation' };
  assert.deepEqual(posixWorkerCommand({ ...worker, userScope: true }), {
    file: 'systemd-run',
    args: ['--user', '--scope', '--quiet', '--collect', '--', '/opt/node/bin/node', '/srv/agent/src/index.mjs', 'op-run', 'scoped-operation'],
  });
  assert.deepEqual(posixWorkerCommand({ ...worker, userScope: false }), {
    file: '/opt/node/bin/node', args: ['/srv/agent/src/index.mjs', 'op-run', 'scoped-operation'],
  });
});

test('a user scope is used only where systemd-run proves it can start one', () => {
  const calls = [];
  const answering = (result) => (file, args, options) => { calls.push({ file, args, env: options.env }); return result; };
  const environment = { LEGIONCTL_HOME: '/srv/state', XDG_RUNTIME_DIR: '/run/user/1000' };

  assert.equal(userScopeAvailable({ platform: 'linux', environment, run: answering({ status: 0, error: undefined }) }), true);
  assert.deepEqual(calls.at(-1), { file: 'systemd-run', args: ['--user', '--scope', '--quiet', '--collect', '--', 'true'], env: environment });
  for (const result of [
    { status: 1, error: undefined },
    { status: null, error: Object.assign(new Error('spawnSync systemd-run ENOENT'), { code: 'ENOENT' }) },
    { status: null, signal: 'SIGTERM', error: Object.assign(new Error('spawnSync systemd-run ETIMEDOUT'), { code: 'ETIMEDOUT' }) },
  ]) {
    assert.equal(userScopeAvailable({ platform: 'linux', environment, run: answering(result) }), false, JSON.stringify(result));
  }

  const before = calls.length;
  for (const platform of ['darwin', 'freebsd']) assert.equal(userScopeAvailable({ platform, run: answering({ status: 0 }) }), false);
  assert.equal(calls.length, before, 'nothing is probed where there is no systemd');
});

// Only a Linux host with a user manager can show it: a worker in the session's
// own scope is what logind ends with the session.
test('a detached worker on a host with a user manager runs outside the session\'s scope', {
  skip: !userScopeAvailable() && 'no systemd user manager answers here',
}, async () => {
  await withHome(async (home) => {
    const seen = path.join(home, 'cgroup');
    const action = [process.execPath, '-e', `require('node:fs').copyFileSync('/proc/self/cgroup', ${JSON.stringify(seen)})`];
    writeConfig(home, { system: { id: 'test' }, updates: { automatic: false }, services: [], actions: [{ id: 'where', command: action }], boot: { targets: {} } });
    const id = 'detached-scope-0001';
    const accepted = cli(['run', 'where', '--detach', '--op', id], { home });
    assert.equal(accepted.payload?.detached, true, accepted.stdout + accepted.stderr);
    assert.equal((await finish(home, id)).result.action, 'ran');
    assert.match(fs.readFileSync(seen, 'utf8'), /\/user@[0-9]+\.service\/.*\.scope$/m);
  });
});

test('detached action survives the launching process, preserves isolation and rejects changed intent', async () => {
  await withHome(async (home) => {
    const counter = path.join(home, 'count');
    const action = [process.execPath, '-e', `setTimeout(() => { require('node:fs').appendFileSync(${JSON.stringify(counter)}, 'x') }, 1500)`];
    writeConfig(home, { system: { id: 'test' }, updates: { automatic: false }, services: [], actions: [
      { id: 'first', command: action }, { id: 'second', command: action },
    ], boot: { targets: {} } });
    const id = 'detached-1234-5678';
    const accepted = cli(['run', 'first', '--detach', '--op', id], { home, env: { LEGIONCTL_CLIENT: 'phone', LEGIONCTL_DEVICE: 'test controller' } });
    assert.equal(accepted.payload?.action, 'accepted', accepted.stdout + accepted.stderr);
    assert.equal(accepted.payload.detached, true);
    const reused = cli(['run', 'second', '--detach', '--op', id], { home });
    assert.equal(reused.payload.reasonCode, 'bad-argument', reused.stdout);
    assert.equal(reused.payload.argument, '--op');
    const completed = await finish(home, id);
    assert.equal(completed.result.action, 'ran', JSON.stringify(completed));
    assert.equal(completed.initiator.client, 'phone');
    assert.equal(completed.initiator.device, 'test controller');
    const replay = cli(['run', 'first', '--detach', '--op', id], { home });
    assert.equal(replay.payload.replayed, true);
    assert.equal(replay.payload.action, 'ran');
    assert.equal(fs.readFileSync(counter, 'utf8'), 'x');
  });
});

test('a detach that cannot start its worker does the work in this session, in the command\'s own shape', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0' });
    writeConfig(home, fixtureConfig(dir, {
      services: [commandService(dir)],
      actions: [{ id: 'say', command: [process.execPath, '-e', 'console.log("said")'] }],
    }));
    const nodeArgs = ['--import', pathToFileURL(path.join(import.meta.dirname, 'no-detach.mjs')).href];

    const ran = cli(['run', 'say', '--detach', '--op', 'no-worker-run-0001'], { home, nodeArgs });
    assert.equal(ran.payload?.action, 'ran', ran.stdout + ran.stderr);
    assert.deepEqual([ran.payload.id, ran.payload.exitCode, ran.payload.output.trim(), ran.payload.detached], ['say', 0, 'said', false]);
    assert.equal(ran.payload.op.state, 'finished');

    const updated = cli(['update', '--detach', '--op', 'no-worker-update-01'], { home, nodeArgs });
    assert.equal(updated.payload?.action, 'updated', updated.stdout + updated.stderr);
    assert.deepEqual([updated.payload.service, updated.payload.from, updated.payload.to, updated.payload.detached], ['demo', '1.0.0', '2.0.0', false]);
    assert.equal(readFixture(dir, 'installed'), '2.0.0');
    assert.equal(updated.payload.op.detached, false, 'the record says nobody detached it');
  });
});
