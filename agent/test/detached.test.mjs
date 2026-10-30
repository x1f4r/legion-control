import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { pathToFileURL } from 'node:url';
import { windowsWorkerScript } from '../src/detached.mjs';
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
