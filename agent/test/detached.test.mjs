import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { windowsWorkerScript } from '../src/detached.mjs';
import { cli, withHome, writeConfig } from './helpers.mjs';

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
    const conflict = cli(['run', 'second', '--detach', '--op', id], { home });
    assert.equal(conflict.payload.action, 'conflict', conflict.stdout);
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
