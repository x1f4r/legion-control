import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { beginOperation, pruneOperations, readOperation, recoverOperations, updateOperation, wireRecord } from '../src/operations.mjs';
import { isRunning, processEvidence } from '../src/probes/process.mjs';
import { cli, commandService, deadPid, FIXTURE, fixtureConfig, fixtureData, readFixture, withHome, writeConfig } from './helpers.mjs';

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

/** The fixture's process, with its running command pointed at a probe that is not there. */
function unreadableProbe(home, dir) {
  const call = (verb) => [process.execPath, FIXTURE, verb, dir];
  return { type: 'command', running: [path.join(home, 'no-such-probe')], start: call('start'), stop: call('stop') };
}

test('the process probe says running, stopped or unknown, and only a positive stopped lets the stop loop on', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home);
    const call = (verb) => [process.execPath, FIXTURE, verb, dir];
    const service = (proc) => commandService(dir, { process: proc });
    const probed = service({ type: 'command', running: call('running'), start: call('start') });
    assert.deepEqual(processEvidence(probed), { state: 'running', error: null });
    fs.writeFileSync(path.join(dir, 'running'), 'off');
    assert.deepEqual(processEvidence(probed), { state: 'stopped', error: null });
    assert.equal(isRunning(probed), false);

    for (const proc of [unreadableProbe(home, dir), { type: 'command', start: call('start') }]) {
      const evidence = processEvidence(service(proc));
      assert.equal(evidence.state, 'unknown');
      assert.ok(evidence.error, 'unknown always says why');
      assert.equal(isRunning(service(proc)), true, 'the stop loop never takes unknown for stopped');
    }
    assert.match(processEvidence(service({ type: 'command', start: call('start') })).error, /has no running command/);
    assert.equal(processEvidence(service({ type: 'none' })).state, 'running', 'nothing to start or stop is nothing that can be down');
  });
});

test('a recovery whose process probe fails stays owed and says why, then restores once the probe reads', async () => {
  await withHome(async (home) => {
    const { config, dir, id } = interruptedService(home);
    config.services[0].process = unreadableProbe(home, dir);
    writeConfig(home, config);
    // Twice, as the timer runs it, and with automatic updates off: recovery
    // runs whatever the policy says.
    for (let cycle = 0; cycle < 2; cycle += 1) {
      const { payload } = cli(['cycle'], { home });
      assert.equal(payload.action, 'cycled');
      assert.match(payload.message, /recovery is still owed: Demo was neither started nor taken for running, because nothing can tell whether it is/);
      assert.equal(readFixture(dir, 'start-runs'), '', 'unknown is not stopped, so nothing was started');
      assert.equal(readOperation(id).recoveryPending, true, 'unknown is not running either, so the obligation stays');
    }
    const reasons = readOperation(id).log.filter((entry) => entry.line.startsWith('recovery is still owed'));
    assert.equal(reasons.length, 1, 'the reason is on the record, once, until it changes');
    assert.match(reasons[0].line, /no-such-probe/);

    writeConfig(home, fixtureConfig(dir, { updates: { automatic: false } }));
    const { payload } = cli(['cycle'], { home });
    assert.doesNotMatch(payload.message, /still owed/);
    assert.equal(readFixture(dir, 'running'), 'on');
    assert.equal(readFixture(dir, 'start-runs'), '1');
    assert.equal(readOperation(id).recoveryPending, false);
    cli(['cycle'], { home });
    assert.equal(readFixture(dir, 'start-runs'), '1', 'restored exactly once');
  });
});

test('a service the probe positively reads as running settles its recovery without a second start', async () => {
  await withHome(async (home) => {
    const { config, dir, id } = interruptedService(home);
    fs.writeFileSync(path.join(dir, 'running'), 'on');
    writeConfig(home, config);
    const { payload } = cli(['cycle'], { home });
    assert.doesNotMatch(payload.message, /still owed/);
    assert.equal(readFixture(dir, 'start-runs'), '');
    assert.equal(readOperation(id).recoveryPending, false);
  });
});
