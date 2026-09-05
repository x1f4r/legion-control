import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { beginOperation, pruneOperations, readOperation, recoverOperations, updateOperation, wireRecord } from '../src/operations.mjs';
import { cli, deadPid, fixtureConfig, fixtureData, readFixture, withHome, writeConfig } from './helpers.mjs';

function interruptedService(home) {
  const dir = fixtureData(home, { running: 'off' });
  const config = fixtureConfig(dir, { updates: { automatic: false } });
  const id = 'interrupted-service-001';
  beginOperation({ id, kind: 'update', service: 'demo', systemId: 'test' });
  updateOperation(id, { pid: deadPid(), phase: 'installing' });
  return { config, dir, id };
}

test('recovery respects a live watchdog and retries exactly once after it releases the service', async () => {
  await withHome(async (home) => {
    const { config, dir, id } = interruptedService(home);
    const lock = path.join(home, 'watchdog.lock');
    const holder = `old-version\npid=${process.pid}\n`;
    config.services[0].lockFile = lock;
    fs.writeFileSync(lock, holder);
    writeConfig(home, config);
    for (let cycle = 0; cycle < 2; cycle += 1) {
      assert.equal(cli(['cycle'], { home }).payload.action, 'cycled');
      assert.equal(readFixture(dir, 'running'), 'off');
      assert.equal(readFixture(dir, 'start-runs'), '');
      assert.equal(fs.readFileSync(lock, 'utf8'), holder);
      assert.equal(readOperation(id).recoveryPending, true);
      assert.equal(wireRecord(readOperation(id)).recoveryPending, undefined);
    }
    fs.rmSync(lock);
    cli(['cycle'], { home });
    assert.equal(readFixture(dir, 'running'), 'on');
    assert.equal(readFixture(dir, 'start-runs'), '1');
    assert.equal(readOperation(id).recoveryPending, false);
    assert.equal(fs.existsSync(lock), false);
    // A later intentional stop must not be undone by the old recovery record.
    fs.writeFileSync(path.join(dir, 'running'), 'off');
    cli(['cycle'], { home });
    assert.equal(readFixture(dir, 'running'), 'off');
    assert.equal(readFixture(dir, 'start-runs'), '1');
  });
});

test('recovery persistence survives a failed service-state preflight and resumes after repair', async () => {
  await withHome(async (home) => {
    const { config, dir, id } = interruptedService(home);
    writeConfig(home, config);
    fs.mkdirSync(path.join(home, 'state'), { recursive: true });
    const state = path.join(home, 'state', 'service-demo.json');
    fs.writeFileSync(state, '{broken');
    cli(['cycle'], { home });
    assert.equal(readFixture(dir, 'running'), 'off');
    assert.equal(readFixture(dir, 'start-runs'), '');
    assert.equal(readOperation(id).recoveryPending, true);
    fs.writeFileSync(state, '{}');
    cli(['cycle'], { home });
    assert.equal(readFixture(dir, 'running'), 'on');
    assert.equal(readOperation(id).recoveryPending, false);
  });
});

test('an outstanding recovery obligation cannot be pruned after its interrupted operation finishes', async () => {
  await withHome(async (home) => {
    const { id } = interruptedService(home);
    const recovered = recoverOperations({ systemId: 'test' });
    assert.equal(recovered.find((record) => record.id === id).recoveryPending, true);
    updateOperation(id, { finishedAt: '2000-01-01T00:00:00.000Z' });
    assert.equal(pruneOperations().removed.includes(id), false);
    assert.ok(readOperation(id));
    updateOperation(id, { recoveryPending: false });
    assert.equal(pruneOperations().removed.includes(id), true);
  });
});
