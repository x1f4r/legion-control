import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { beginOperation, pruneOperations, readOperation, recoverOperations, updateOperation, wireRecord } from '../src/operations.mjs';
import { isRunning, processEvidence } from '../src/probes/process.mjs';
import {
  cli, commandService, deadPid, fakeNpm, FIXTURE, fixtureConfig, fixtureData, npmCalls, npmPackageVersion, npmService, readFixture, withHome, writeConfig,
} from './helpers.mjs';

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

/**
 * What a run killed part way leaves: its record still running under a process
 * that is gone, in the phase it reached, with whatever it had noted.
 */
function leftInterrupted(id, { kind = 'update', phase = 'installing', ...noted } = {}) {
  beginOperation({ id, kind, service: 'demo', systemId: 'test' });
  updateOperation(id, { pid: deadPid(), phase, ...noted });
}

test('a restart interrupted between its stop and its start is put back by the next cycle', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { running: 'off' });
    writeConfig(home, fixtureConfig(dir, { updates: { automatic: false } }));
    const id = 'interrupted-restart-001';
    leftInterrupted(id, { kind: 'restart', phase: 'restarting' });
    const { payload } = cli(['cycle'], { home });
    assert.deepEqual(payload.recovered.map((entry) => [entry.opId, entry.action]), [[id, 'interrupted']]);
    assert.equal(readFixture(dir, 'running'), 'on', 'a restart is a stop and a start, and only the stop happened');
    assert.equal(readFixture(dir, 'start-runs'), '1');
    assert.equal(readOperation(id).recoveryPending, false);
  });
});

test('an npm update interrupted part way through its install is reinstalled from the version it started from', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { running: 'off' });
    // The install had taken the old tree away and not written the new one.
    const npm = fakeNpm(home, dir, { installed: null });
    writeConfig(home, fixtureConfig(dir, { updates: { automatic: false }, services: [npmService(dir, npm)] }));
    const id = 'interrupted-npm-install-001';
    leftInterrupted(id, { from: '1.0.0', to: '2.0.0', npmPrefix: npm.prefix });

    const { payload } = cli(['cycle'], { home, nodeArgs: npm.nodeArgs });
    assert.doesNotMatch(payload.message, /still owed/);
    assert.deepEqual(npmCalls(dir).filter((call) => call.startsWith('install ')).map((call) => call.split(' ').pop()), ['demo-pkg@1.0.0']);
    assert.equal(npmPackageVersion(npm.prefix, npm.pkg), '1.0.0');
    assert.equal(readFixture(dir, 'running'), 'on');
    assert.equal(readOperation(id).recoveryPending, false);
    assert.ok(readOperation(id).log.some((entry) => entry.line === 'recovery: rolling-back, back to 1.0.0'), 'each step is noted on the record it is for');
  });
});

test('an npm update interrupted once its install was done is only started again', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { running: 'off' });
    const npm = fakeNpm(home, dir, { installed: '2.0.0' });
    writeConfig(home, fixtureConfig(dir, { updates: { automatic: false }, services: [npmService(dir, npm)] }));
    const id = 'interrupted-npm-start-001';
    leftInterrupted(id, { phase: 'starting', from: '1.0.0', to: '2.0.0', npmPrefix: npm.prefix });
    cli(['cycle'], { home, nodeArgs: npm.nodeArgs });
    assert.deepEqual(npmCalls(dir), [], 'the version it was going to is not something to roll back');
    assert.equal(npmPackageVersion(npm.prefix, npm.pkg), '2.0.0');
    assert.equal(readFixture(dir, 'start-runs'), '1');
    assert.equal(readOperation(id).recoveryPending, false);
  });
});

test('a command update interrupted half way runs its rollback command, once', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { running: 'off', installed: '1.5.0', 'rollback-to': '1.0.0' });
    writeConfig(home, fixtureConfig(dir, { updates: { automatic: false } }));
    const id = 'interrupted-command-001';
    leftInterrupted(id, { from: '1.0.0', to: '2.0.0' });
    cli(['cycle'], { home });
    assert.equal(readFixture(dir, 'rollback-runs'), '1');
    assert.equal(readFixture(dir, 'installed'), '1.0.0');
    assert.equal(readFixture(dir, 'running'), 'on');
    assert.equal(readOperation(id).recoveryPending, false);
    cli(['cycle'], { home });
    assert.equal(readFixture(dir, 'rollback-runs'), '1');
  });
});

test('a running service is stopped for a rollback only once the machine is idle', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { running: 'on', installed: '1.5.0', 'rollback-to': '1.0.0', busy: 'on' });
    writeConfig(home, fixtureConfig(dir, { updates: { automatic: false } }));
    const id = 'interrupted-busy-001';
    leftInterrupted(id, { from: '1.0.0', to: '2.0.0' });
    const { payload } = cli(['cycle'], { home });
    assert.match(payload.message, /recovery is still owed: Demo is on 1\.5\.0, neither 1\.0\.0 nor 2\.0\.0, and the rollback to 1\.0\.0 waits for an idle machine/);
    assert.equal(readFixture(dir, 'rollback-runs'), '');
    assert.equal(readOperation(id).recoveryPending, true);

    fs.writeFileSync(path.join(dir, 'busy'), 'off');
    cli(['cycle'], { home });
    assert.equal(readFixture(dir, 'rollback-runs'), '1');
    assert.equal(readFixture(dir, 'installed'), '1.0.0');
    assert.equal(readOperation(id).recoveryPending, false);
  });
});

test('an update that settled the version later is never undone by an older interrupted one', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { running: 'off', latest: '3.0.0' });
    writeConfig(home, fixtureConfig(dir, { updates: { automatic: false } }));
    const id = 'interrupted-superseded-001';
    leftInterrupted(id, { from: '1.0.0', to: '2.0.0' });
    // Somebody updated by hand before any cycle had looked at the old record,
    // and the registry had moved on to 3.0.0 by then.
    assert.equal(cli(['update'], { home }).payload.action, 'updated');
    assert.equal(readFixture(dir, 'installed'), '3.0.0');

    cli(['cycle'], { home });
    assert.equal(readFixture(dir, 'rollback-runs'), '', '3.0.0 is the later update\'s doing, not a half-done install');
    assert.equal(readFixture(dir, 'installed'), '3.0.0');
    assert.equal(readOperation(id).recoveryPending, false);
  });
});

test('a rollback that fails settles once the service is running again, rather than stop it every cycle', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { running: 'off', 'npm-install-fails': '1.0.0' });
    const npm = fakeNpm(home, dir, { installed: null });
    writeConfig(home, fixtureConfig(dir, { updates: { automatic: false }, services: [npmService(dir, npm)] }));
    const id = 'interrupted-npm-failed-001';
    leftInterrupted(id, { from: '1.0.0', to: '2.0.0', npmPrefix: npm.prefix });
    cli(['cycle'], { home, nodeArgs: npm.nodeArgs });
    assert.equal(readFixture(dir, 'running'), 'on', 'whatever the rollback did, the service is not left stopped');
    assert.equal(readOperation(id).recoveryPending, false);
    assert.match(readOperation(id).log.map((entry) => entry.line).join('\n'), /rollback install failed/);
    cli(['cycle'], { home, nodeArgs: npm.nodeArgs });
    assert.equal(npmCalls(dir).filter((call) => call.startsWith('install ')).length, 1, 'tried once');
  });
});

test('a running npm service whose drain refuses is neither stopped nor reinstalled for a rollback', async () => {
  await withHome(async (home) => {
    // A unit that came back by itself, on the tree a killed install left.
    const dir = fixtureData(home, { running: 'on', 'drain-fails': 'on' });
    const npm = fakeNpm(home, dir, { installed: null });
    const drain = { command: [process.execPath, FIXTURE, 'drain', dir], timeoutSeconds: 10 };
    writeConfig(home, fixtureConfig(dir, { updates: { automatic: false }, services: [npmService(dir, npm, { drain })] }));
    const id = 'interrupted-npm-drain-001';
    leftInterrupted(id, { from: '1.0.0', to: '2.0.0', npmPrefix: npm.prefix });
    const run = () => cli(['cycle'], { home, nodeArgs: npm.nodeArgs }).payload;

    assert.match(run().message, /recovery is still owed: Demo is on a version that cannot be read, neither 1\.0\.0 nor 2\.0\.0, and Demo was not rolled back to 1\.0\.0: the drain command failed/);
    assert.equal(readFixture(dir, 'stop-runs'), '', 'a refused drain stops nothing');
    assert.deepEqual(npmCalls(dir), []);
    assert.equal(readOperation(id).recoveryPending, true);

    // Work that starts while the drain runs is seen by a second look, as on
    // the way to an update's stop.
    fs.writeFileSync(path.join(dir, 'drain-fails'), 'off');
    fs.writeFileSync(path.join(dir, 'drain-makes-busy'), 'on');
    assert.match(run().message, /the rollback to 1\.0\.0 waits for an idle machine \(work started while it was being prepared: /);
    assert.equal(readFixture(dir, 'drain-runs'), '2');
    assert.equal(readFixture(dir, 'stop-runs'), '');
    assert.equal(readOperation(id).recoveryPending, true);

    fs.writeFileSync(path.join(dir, 'drain-makes-busy'), 'off');
    fs.writeFileSync(path.join(dir, 'busy'), 'off');
    assert.doesNotMatch(run().message, /still owed/);
    assert.deepEqual(npmCalls(dir).filter((call) => call.startsWith('install ')).map((call) => call.split(' ').pop()), ['demo-pkg@1.0.0']);
    assert.equal(npmPackageVersion(npm.prefix, npm.pkg), '1.0.0');
    assert.equal(readFixture(dir, 'running'), 'on');
    assert.equal(readOperation(id).recoveryPending, false);
  });
});
